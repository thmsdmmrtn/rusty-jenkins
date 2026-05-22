use crate::cli::ListTagArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::config_sweep::read_xml_tag;
use crate::commands::resolve_jobs;
use anyhow::{Context, Result};

pub async fn run(client: &JenkinsClient, args: &ListTagArgs) -> Result<()> {
    let jobs = resolve_jobs(client, &args.target).await?;

    for job in &jobs {
        match fetch_tag(client, job, &args.xml_tag).await {
            Ok(Some(value)) => println!("{job}:{value}"),
            Ok(None) => println!("{job}: (tag <{}> not found)", args.xml_tag),
            Err(e) => eprintln!("{job}: error — {e:#}"),
        }
    }
    Ok(())
}

async fn fetch_tag(client: &JenkinsClient, job: &str, tag: &str) -> Result<Option<String>> {
    let resp = client
        .get(&format!("job/{}/config.xml", encode_job_path(job)))
        .await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status}");
    }
    let xml = resp.text().await.context("reading config.xml")?;
    read_xml_tag(&xml, tag)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SAMPLE_XML: &str = r#"<?xml version="1.0"?>
<org.jenkinsci.plugins.workflow.multibranch.WorkflowMultiBranchProject>
  <sources><data><jenkins.branch.BranchSource>
    <source class="...GitHubSCMSource">
      <repository>s3</repository>
    </source>
  </jenkins.branch.BranchSource></data></sources>
</org.jenkinsci.plugins.workflow.multibranch.WorkflowMultiBranchProject>"#;

    #[tokio::test]
    async fn lists_tag_for_explicit_jobs() {
        let server = MockServer::start().await;

        for job in ["folder/job1", "folder/job2"] {
            Mock::given(method("GET"))
                .and(path(format!("/job/folder/job/{job_name}/config.xml",
                    job_name = job.split('/').last().unwrap())))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(SAMPLE_XML),
                )
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                path: None,
                job_names: vec!["folder/job1".into(), "folder/job2".into()],
            },
            xml_tag: "repository".into(),
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn lists_tag_for_folder_jobs() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/abc/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "jobs": [
                    { "name": "job1", "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob" },
                    { "name": "job2", "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob" },
                ]
            })))
            .mount(&server)
            .await;

        for job_name in ["job1", "job2"] {
            Mock::given(method("GET"))
                .and(path(format!("/job/abc/job/{job_name}/config.xml")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(SAMPLE_XML),
                )
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                path: Some("abc".into()),
                job_names: vec![],
            },
            xml_tag: "repository".into(),
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn shows_not_found_when_tag_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<root><other>x</other></root>"))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                path: None,
                job_names: vec!["abc/job1".into()],
            },
            xml_tag: "missing-tag".into(),
        };
        // Should not error — just prints "(tag not found)"
        run(&client, &args).await.unwrap();
    }
}
