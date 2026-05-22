use crate::cli::PatchTagArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::config_sweep::{patch_xml_tag, read_xml_tag};
use crate::commands::resolve_jobs;
use anyhow::{Context, Result};
use colored::Colorize;

pub async fn run(client: &JenkinsClient, args: &PatchTagArgs) -> Result<()> {
    let jobs = resolve_jobs(client, &args.target).await?;
    let total = jobs.len();

    for (i, job) in jobs.iter().enumerate() {
        print!("{} {} … ", format!("[{}/{}]", i + 1, total).dimmed(), job.cyan());
        match apply(client, job, &args.xml_tag, &args.value, args.show_old).await {
            Ok(old) => {
                let tag = format!("<{}>", args.xml_tag).cyan().to_string();
                let new_val = args.value.green().to_string();
                if let Some(prev) = old {
                    println!("{tag}: {} → {new_val}", prev.yellow());
                } else {
                    println!("{tag} → {new_val}");
                }
            }
            Err(e) => println!("{} {e:#}", "FAILED —".red()),
        }
    }
    Ok(())
}

/// Returns the old value when `show_old` is true, `None` otherwise.
async fn apply(
    client: &JenkinsClient,
    job: &str,
    tag: &str,
    value: &str,
    show_old: bool,
) -> Result<Option<String>> {
    let path = format!("job/{}/config.xml", encode_job_path(job));

    let resp = client.get(&path).await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET config.xml returned HTTP {status}");
    }
    let original = resp.text().await.context("reading config.xml")?;

    let old = if show_old {
        read_xml_tag(&original, tag)?
    } else {
        None
    };

    let patched = patch_xml_tag(&original, tag, value)?;

    let resp = client
        .post(&path)
        .await?
        .header("Content-Type", "application/xml")
        .body(patched)
        .send()
        .await
        .context("uploading config.xml")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("POST config.xml returned HTTP {status}");
    }
    Ok(old)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn crumb() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(&serde_json::json!({
            "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
        }))
    }

    const SAMPLE_XML: &str = r#"<?xml version="1.0"?>
<project>
  <scm><remote>git@github.com:org/old-repo.git</remote></scm>
</project>"#;

    #[tokio::test]
    async fn patches_tag_for_each_job_no_build_triggered() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        for job in ["job1", "job2"] {
            Mock::given(method("GET"))
                .and(path(format!("/job/abc/job/{job}/config.xml")))
                .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path(format!("/job/abc/job/{job}/config.xml")))
                .and(header("Content-Type", "application/xml"))
                .and(body_string_contains("<remote>new-repo</remote>"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = PatchTagArgs {
            target: crate::cli::JobTarget {
                path: None,
                job_names: vec!["abc/job1".into(), "abc/job2".into()],
            },
            xml_tag: "remote".into(),
            value: "new-repo".into(),
            show_old: false,
        };
        run(&client, &args).await.unwrap();
        // wiremock asserts expect(1) on each POST on drop
    }

    #[tokio::test]
    async fn patches_all_jobs_in_folder() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/team/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "jobs": [
                    { "name": "alpha", "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob" },
                    { "name": "beta",  "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob" },
                    { "name": "sub",   "_class": "com.cloudbees.hudson.plugins.folder.Folder" },
                ]
            })))
            .mount(&server)
            .await;

        for job in ["alpha", "beta"] {
            Mock::given(method("GET"))
                .and(path(format!("/job/team/job/{job}/config.xml")))
                .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path(format!("/job/team/job/{job}/config.xml")))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = PatchTagArgs {
            target: crate::cli::JobTarget {
                path: Some("team".into()),
                job_names: vec![],
            },
            xml_tag: "remote".into(),
            value: "new-value".into(),
            show_old: false,
        };
        run(&client, &args).await.unwrap();
        // sub-folder is skipped; only alpha + beta are patched
    }

    #[tokio::test]
    async fn continues_on_individual_job_failure() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // job1: config fetch fails
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        // job2: succeeds
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job2/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/job/abc/job/job2/config.xml"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = PatchTagArgs {
            target: crate::cli::JobTarget {
                path: None,
                job_names: vec!["abc/job1".into(), "abc/job2".into()],
            },
            xml_tag: "remote".into(),
            value: "x".into(),
            show_old: false,
        };
        // Should not bail — job1 failure is printed but loop continues to job2
        run(&client, &args).await.unwrap();
    }
}
