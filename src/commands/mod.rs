pub mod build;
pub mod config;
pub mod config_sweep;
pub mod inspect;
pub mod list;
pub mod list_tag;
pub mod logs;
pub mod patch_tag;
pub mod sweep;

use crate::cli::JobTarget;
use crate::client::{encode_job_path, JenkinsClient};
use anyhow::Result;
use serde::Deserialize;

/// Returns true for Jenkins class names that represent folders / containers.
/// Matches `Folder`, `OrganizationFolder`, etc. by checking the suffix.
/// Does NOT match `WorkflowMultiBranchProject` — multibranch pipelines have
/// config.xml and are valid `--job-name` targets.
pub fn is_folder_class(class: &str) -> bool {
    class.to_ascii_lowercase().ends_with("folder")
}

/// Resolve the set of job paths from a [`JobTarget`].
/// Validates that `--job-name` entries are not folders and `--path` entries are.
/// `--path` expands to all direct (non-folder) job children; with `--recursive`
/// it descends into sub-folders too. Both flags can be combined and are repeatable.
pub async fn resolve_jobs(client: &JenkinsClient, target: &JobTarget) -> Result<Vec<String>> {
    validate_job_names(client, &target.job_names).await?;

    let mut jobs: Vec<String> = target.job_names.clone();

    for folder in &target.paths {
        let folder_jobs = list_folder_jobs(client, folder.clone(), target.recursive).await?;
        jobs.extend(folder_jobs);
    }

    if jobs.is_empty() {
        anyhow::bail!("no jobs targeted — provide --path or at least one --job-name");
    }
    Ok(jobs)
}

/// Check that none of the explicitly named jobs is actually a folder.
async fn validate_job_names(client: &JenkinsClient, names: &[String]) -> Result<()> {
    for name in names {
        #[derive(Deserialize)]
        struct Info {
            #[serde(rename = "_class")]
            class: Option<String>,
        }

        let path = format!("job/{}/api/json?tree=_class", encode_job_path(name));
        let resp = client.get(&path).await?;
        if !resp.status().is_success() {
            continue; // non-existent job — let the command itself surface the error
        }
        if let Ok(info) = resp.json::<Info>().await {
            if info.class.as_deref().map(is_folder_class).unwrap_or(false) {
                anyhow::bail!(
                    "'{name}' is a folder — use --path instead of --job-name"
                );
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct FolderInfo {
    #[serde(rename = "_class")]
    class: Option<String>,
    #[serde(default)]
    jobs: Vec<JobEntry>,
}

#[derive(Deserialize)]
struct JobEntry {
    name: String,
    #[serde(rename = "_class")]
    class: String,
}

/// Async-recursive folder listing. Uses `Box::pin` to break the infinite type cycle.
fn list_folder_jobs(
    client: &JenkinsClient,
    folder: String,
    recursive: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>>> + Send + '_>> {
    Box::pin(async move {
        let path = format!(
            "job/{}/api/json?tree=_class,jobs[name,_class]",
            encode_job_path(&folder)
        );
        let resp = client.get(&path).await?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "could not list '{}': HTTP {} — is it a job? Try --job-name instead of --path",
                folder,
                resp.status()
            );
        }
        let info: FolderInfo = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing folder JSON: {e}"))?;

        if let Some(class) = &info.class {
            if !is_folder_class(class) {
                anyhow::bail!(
                    "'{}' is a job, not a folder — use --job-name instead of --path",
                    folder
                );
            }
        }

        let mut jobs = Vec::new();
        for entry in info.jobs {
            let is_folder = entry.class.contains("Folder") || entry.class.contains("folder");
            if is_folder {
                if recursive {
                    let subfolder = format!("{folder}/{}", entry.name);
                    let sub_jobs = list_folder_jobs(client, subfolder, true).await?;
                    jobs.extend(sub_jobs);
                }
            } else {
                jobs.push(format!("{folder}/{}", entry.name));
            }
        }
        Ok(jobs)
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn folder_resp(jobs: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(jobs)
    }

    #[tokio::test]
    async fn resolve_jobs_explicit_names_are_returned_as_is() {
        let server = MockServer::start().await;
        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let target = JobTarget {
            paths: vec![],
            job_names: vec!["folder/job1".into(), "folder/job2".into()],
            recursive: false,
        };
        let jobs = resolve_jobs(&client, &target).await.unwrap();
        assert_eq!(jobs, vec!["folder/job1", "folder/job2"]);
    }

    #[tokio::test]
    async fn resolve_jobs_lists_non_folder_children() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/abc/api/json"))
            .respond_with(folder_resp(serde_json::json!({
                "jobs": [
                    { "name": "job1", "_class": "WorkflowJob" },
                    { "name": "sub",  "_class": "com.cloudbees.hudson.plugins.folder.Folder" },
                ]
            })))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let target = JobTarget {
            paths: vec!["abc".into()],
            job_names: vec![],
            recursive: false,
        };
        let jobs = resolve_jobs(&client, &target).await.unwrap();
        assert_eq!(jobs, vec!["abc/job1"]);
    }

    #[tokio::test]
    async fn resolve_jobs_recursive_descends_into_subfolders() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/parent/api/json"))
            .respond_with(folder_resp(serde_json::json!({
                "jobs": [
                    { "name": "job1", "_class": "WorkflowJob" },
                    { "name": "sub",  "_class": "Folder" },
                ]
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/parent/job/sub/api/json"))
            .respond_with(folder_resp(serde_json::json!({
                "jobs": [
                    { "name": "job2", "_class": "WorkflowJob" },
                ]
            })))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let target = JobTarget {
            paths: vec!["parent".into()],
            job_names: vec![],
            recursive: true,
        };
        let jobs = resolve_jobs(&client, &target).await.unwrap();
        assert_eq!(jobs, vec!["parent/job1", "parent/sub/job2"]);
    }

    #[tokio::test]
    async fn resolve_jobs_multiple_paths_combined() {
        let server = MockServer::start().await;

        for folder in ["team-a", "team-b"] {
            Mock::given(method("GET"))
                .and(path(format!("/job/{folder}/api/json")))
                .respond_with(folder_resp(serde_json::json!({
                    "jobs": [{ "name": "svc", "_class": "WorkflowJob" }]
                })))
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let target = JobTarget {
            paths: vec!["team-a".into(), "team-b".into()],
            job_names: vec!["extra/job".into()],
            recursive: false,
        };
        let jobs = resolve_jobs(&client, &target).await.unwrap();
        assert_eq!(jobs, vec!["extra/job", "team-a/svc", "team-b/svc"]);
    }
}
