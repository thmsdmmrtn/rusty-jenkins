use crate::cli::ListArgs;
use crate::client::{encode_job_path, JenkinsClient};
use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;

// ── Jenkins API types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FolderInfo {
    #[serde(default)]
    jobs: Vec<JobEntry>,
}

#[derive(Debug, Deserialize)]
struct JobEntry {
    name: String,
    #[serde(rename = "_class")]
    class: String,
    /// Absent on folders. May have `_anime` suffix while a build is running.
    color: Option<String>,
}

impl JobEntry {
    fn is_folder(&self) -> bool {
        // CloudBees and standard Jenkins folder class names all contain "Folder"
        self.class.contains("Folder") || self.class.contains("folder")
    }

    /// True when the `_anime` suffix is present, indicating a running build.
    fn is_building(&self) -> bool {
        self.color.as_deref().map(|c| c.ends_with("_anime")).unwrap_or(false)
    }

    /// Map the Jenkins color field to a readable status string.
    fn status(&self) -> &str {
        match self.color.as_deref().map(|c| c.trim_end_matches("_anime")) {
            Some("blue")     => "SUCCESS",
            Some("red")      => "FAILED",
            Some("yellow")   => "UNSTABLE",
            Some("aborted")  => "ABORTED",
            Some("disabled") => "DISABLED",
            _                => "NOT BUILT",
        }
    }
}

// ── Command entry point ───────────────────────────────────────────────────────

pub async fn run(client: &JenkinsClient, args: &ListArgs) -> Result<()> {
    let (display_path, api_path) = match &args.path {
        Some(p) if !p.is_empty() => (
            format!("{p}/"),
            format!("job/{}/api/json?tree=jobs[name,_class,color]", encode_job_path(p)),
        ),
        _ => (
            "(root)".to_string(),
            "api/json?tree=jobs[name,_class,color]".to_string(),
        ),
    };

    let resp = client.get(&api_path).await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "Jenkins returned HTTP {status} — check the path is a folder, not a job.\n\
             Hint: use `rj inspect <path>` if this is a job."
        );
    }

    let info: FolderInfo = resp.json().await.context("parsing folder JSON")?;

    println!("{}", display_path.cyan().bold());

    if info.jobs.is_empty() {
        println!("  {}", "(empty — or this path points to a job rather than a folder)".dimmed());
        println!("  {}", "Hint: use `rj inspect <path>` for job details.".dimmed());
        return Ok(());
    }

    let mut folders = 0usize;
    let mut jobs = 0usize;

    for entry in &info.jobs {
        if entry.is_folder() {
            folders += 1;
            println!("  {}  {}", "[FOLDER]".cyan(), entry.name.cyan());
        } else {
            jobs += 1;
            let status_colored = match entry.status() {
                "SUCCESS"  => "SUCCESS".green().to_string(),
                "FAILED"   => "FAILED".red().to_string(),
                "UNSTABLE" => "UNSTABLE".yellow().to_string(),
                "ABORTED"  => "ABORTED".dimmed().to_string(),
                "DISABLED" => "DISABLED".dimmed().to_string(),
                other      => other.dimmed().to_string(),
            };
            let building = if entry.is_building() {
                format!("  {}", "*building*".blue().bold())
            } else {
                String::new()
            };
            println!("  {}  {:<40} {}{}",
                "[JOB]".normal(),
                entry.name,
                status_colored,
                building,
            );
        }
    }

    println!("\n  {}", format!("{folders} folder(s), {jobs} job(s)").dimmed());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn entry(name: &str, class: &str, color: Option<&str>) -> JobEntry {
        JobEntry {
            name: name.to_string(),
            class: class.to_string(),
            color: color.map(str::to_string),
        }
    }

    // ── is_folder ─────────────────────────────────────────────────────────────

    #[test]
    fn is_folder_detects_standard_folder_class() {
        assert!(entry("f", "com.cloudbees.hudson.plugins.folder.Folder", None).is_folder());
    }

    #[test]
    fn is_folder_detects_cloudbees_folder_class() {
        assert!(entry("f", "com.cloudbees.hudson.plugins.folder.CloudBeesFolder", None).is_folder());
    }

    #[test]
    fn is_folder_false_for_pipeline_job() {
        assert!(!entry("j", "org.jenkinsci.plugins.workflow.job.WorkflowJob", Some("blue")).is_folder());
    }

    #[test]
    fn is_folder_false_for_freestyle_job() {
        assert!(!entry("j", "hudson.model.FreeStyleProject", Some("red")).is_folder());
    }

    // ── status / is_building ──────────────────────────────────────────────────

    #[test]
    fn status_maps_all_color_values() {
        let cases = [
            ("blue",     "SUCCESS"),
            ("red",      "FAILED"),
            ("yellow",   "UNSTABLE"),
            ("aborted",  "ABORTED"),
            ("disabled", "DISABLED"),
            ("notbuilt", "NOT BUILT"),
        ];
        for (color, expected) in cases {
            let e = entry("j", "hudson.model.FreeStyleProject", Some(color));
            assert_eq!(e.status(), expected, "color={color}");
        }
    }

    #[test]
    fn status_strips_anime_suffix_before_mapping() {
        let e = entry("j", "hudson.model.FreeStyleProject", Some("blue_anime"));
        assert_eq!(e.status(), "SUCCESS");
        assert!(e.is_building());
    }

    #[test]
    fn is_building_false_when_no_anime_suffix() {
        assert!(!entry("j", "hudson.model.FreeStyleProject", Some("blue")).is_building());
    }

    #[test]
    fn is_building_false_when_color_absent() {
        assert!(!entry("f", "com.cloudbees.hudson.plugins.folder.Folder", None).is_building());
    }

    // ── HTTP: listing a folder ────────────────────────────────────────────────

    #[tokio::test]
    async fn list_folder_calls_correct_nested_path() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/parent/job/child/api/json"))
            .and(query_param("tree", "jobs[name,_class,color]"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "jobs": [
                    { "name": "my-job", "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob", "color": "blue" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListArgs { path: Some("parent/child".to_string()) };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn list_root_calls_root_api_endpoint() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/json"))
            .and(query_param("tree", "jobs[name,_class,color]"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "jobs": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListArgs { path: None };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn list_returns_error_on_404() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/nonexistent/api/json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListArgs { path: Some("nonexistent".to_string()) };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn list_mixed_folders_and_jobs() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/team/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "jobs": [
                    { "name": "sub-folder",  "_class": "com.cloudbees.hudson.plugins.folder.Folder", "color": null },
                    { "name": "deploy",      "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob", "color": "blue" },
                    { "name": "nightly",     "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob", "color": "red" },
                    { "name": "in-progress", "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob", "color": "blue_anime" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListArgs { path: Some("team".to_string()) };
        run(&client, &args).await.unwrap();
    }
}
