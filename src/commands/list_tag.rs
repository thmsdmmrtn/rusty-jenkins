use crate::cli::ListTagArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::config_sweep::read_xml_tag;
use crate::commands::resolve_jobs;
use anyhow::{Context, Result};
use colored::Colorize;

pub async fn run(client: &JenkinsClient, args: &ListTagArgs) -> Result<()> {
    if args.xml_tags.is_empty() {
        anyhow::bail!("at least one --xml-tag is required");
    }

    let jobs = resolve_jobs(client, &args.target).await?;
    let multi = args.xml_tags.len() > 1;

    for job in &jobs {
        match fetch_tags(client, job, &args.xml_tags).await {
            Ok(values) => {
                if multi {
                    println!("{}", job.cyan());
                    for (tag, val) in args.xml_tags.iter().zip(values) {
                        match val {
                            Some(v) => println!("  {}:{}", tag.cyan(), v.yellow()),
                            None => println!("  {}", format!("(tag <{tag}> not found)").dimmed()),
                        }
                    }
                } else {
                    match values.into_iter().next().unwrap() {
                        Some(v) => println!("{}:{}", job.cyan(), v.yellow()),
                        None => println!(
                            "{}: {}",
                            job.cyan(),
                            format!("(tag <{}> not found)", args.xml_tags[0]).dimmed()
                        ),
                    }
                }
            }
            Err(e) => eprintln!("{}: {} {e:#}", job.cyan(), "error —".red()),
        }
    }
    Ok(())
}

async fn fetch_tags(
    client: &JenkinsClient,
    job: &str,
    tags: &[String],
) -> Result<Vec<Option<String>>> {
    let resp = client
        .get(&format!("job/{}/config.xml", encode_job_path(job)))
        .await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status}");
    }
    let xml = resp.text().await.context("reading config.xml")?;
    tags.iter().map(|tag| read_xml_tag(&xml, tag)).collect()
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
      <branch>main</branch>
    </source>
  </jenkins.branch.BranchSource></data></sources>
</org.jenkinsci.plugins.workflow.multibranch.WorkflowMultiBranchProject>"#;

    #[tokio::test]
    async fn lists_tag_for_explicit_jobs() {
        let server = MockServer::start().await;

        for job in ["folder/job1", "folder/job2"] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/job/folder/job/{job_name}/config.xml",
                    job_name = job.split('/').last().unwrap()
                )))
                .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                paths: vec![],
                job_names: vec!["folder/job1".into(), "folder/job2".into()],
                recursive: false,
            },
            xml_tags: vec!["repository".into()],
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
                .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
                .mount(&server)
                .await;
        }

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                paths: vec!["abc".into()],
                job_names: vec![],
                recursive: false,
            },
            xml_tags: vec!["repository".into()],
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn lists_multiple_tags_from_single_config_fetch() {
        let server = MockServer::start().await;

        // Only one GET to config.xml even when multiple tags are requested.
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .expect(1)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                paths: vec![],
                job_names: vec!["abc/job1".into()],
                recursive: false,
            },
            xml_tags: vec!["repository".into(), "branch".into()],
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
                paths: vec![],
                job_names: vec!["abc/job1".into()],
                recursive: false,
            },
            xml_tags: vec!["missing-tag".into()],
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn errors_when_no_xml_tag_provided() {
        let server = MockServer::start().await;
        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = ListTagArgs {
            target: crate::cli::JobTarget {
                paths: vec![],
                job_names: vec!["abc/job1".into()],
                recursive: false,
            },
            xml_tags: vec![],
        };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("--xml-tag"));
    }
}
