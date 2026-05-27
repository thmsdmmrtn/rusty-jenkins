use crate::cli::PatchTagArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::config_sweep::{patch_xml_tag, read_xml_tag};
use crate::commands::resolve_jobs;
use anyhow::{Context, Result};
use colored::Colorize;

pub async fn run(client: &JenkinsClient, args: &PatchTagArgs) -> Result<()> {
    if args.xml_tags.is_empty() {
        anyhow::bail!("at least one --xml-tag is required");
    }
    if args.xml_tags.len() != args.values.len() {
        anyhow::bail!(
            "--xml-tag count ({}) does not match --value count ({})",
            args.xml_tags.len(),
            args.values.len()
        );
    }

    let jobs = resolve_jobs(client, &args.target).await?;
    let total = jobs.len();

    for (i, job) in jobs.iter().enumerate() {
        println!("{} {}", format!("[{}/{}]", i + 1, total).dimmed(), job.cyan());
        match apply(client, job, &args.xml_tags, &args.values, args.show_old).await {
            Ok(old_values) => {
                for (j, (tag, new_val)) in args.xml_tags.iter().zip(args.values.iter()).enumerate() {
                    let tag_fmt = format!("<{tag}>").cyan().to_string();
                    let new_fmt = new_val.green().to_string();
                    match old_values.get(j).and_then(|v| v.as_ref()) {
                        Some(prev) => println!("  {tag_fmt}: {} → {new_fmt}", prev.yellow()),
                        None       => println!("  {tag_fmt} → {new_fmt}"),
                    }
                }
            }
            Err(e) => println!("  {} {e:#}", "FAILED —".red()),
        }
    }
    Ok(())
}

/// Fetch config.xml once, apply all tag patches, upload once.
/// Returns old values per tag (populated only when `show_old` is true).
async fn apply(
    client: &JenkinsClient,
    job: &str,
    tags: &[String],
    values: &[String],
    show_old: bool,
) -> Result<Vec<Option<String>>> {
    let path = format!("job/{}/config.xml", encode_job_path(job));

    let resp = client.get(&path).await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET config.xml returned HTTP {status}");
    }
    let original = resp.text().await.context("reading config.xml")?;

    let old_values: Vec<Option<String>> = if show_old {
        tags.iter()
            .map(|tag| read_xml_tag(&original, tag))
            .collect::<Result<_>>()?
    } else {
        vec![None; tags.len()]
    };

    let mut xml = original;
    for (tag, value) in tags.iter().zip(values.iter()) {
        xml = patch_xml_tag(&xml, tag, value)?;
    }

    let resp = client
        .post(&path)
        .await?
        .header("Content-Type", "application/xml")
        .body(xml)
        .send()
        .await
        .context("uploading config.xml")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("POST config.xml returned HTTP {status}");
    }
    Ok(old_values)
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
  <scm>
    <remote>git@github.com:org/old-repo.git</remote>
    <branch>develop</branch>
  </scm>
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
                paths: vec![],
                job_names: vec!["abc/job1".into(), "abc/job2".into()],
                recursive: false,
            },
            xml_tags: vec!["remote".into()],
            values: vec!["new-repo".into()],
            show_old: false,
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn patches_multiple_tags_in_single_upload() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .expect(1)
            .mount(&server)
            .await;

        // Both tag changes must appear in the single POST body.
        Mock::given(method("POST"))
            .and(path("/job/abc/job/job1/config.xml"))
            .and(body_string_contains("<remote>new-remote</remote>"))
            .and(body_string_contains("<branch>main</branch>"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = PatchTagArgs {
            target: crate::cli::JobTarget {
                paths: vec![],
                job_names: vec!["abc/job1".into()],
                recursive: false,
            },
            xml_tags: vec!["remote".into(), "branch".into()],
            values: vec!["new-remote".into(), "main".into()],
            show_old: false,
        };
        run(&client, &args).await.unwrap();
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
                paths: vec!["team".into()],
                job_names: vec![],
                recursive: false,
            },
            xml_tags: vec!["remote".into()],
            values: vec!["new-value".into()],
            show_old: false,
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn continues_on_individual_job_failure() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

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
                paths: vec![],
                job_names: vec!["abc/job1".into(), "abc/job2".into()],
                recursive: false,
            },
            xml_tags: vec!["remote".into()],
            values: vec!["x".into()],
            show_old: false,
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn errors_when_tag_value_counts_mismatch() {
        let server = MockServer::start().await;
        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = PatchTagArgs {
            target: crate::cli::JobTarget {
                paths: vec![],
                job_names: vec!["abc/job1".into()],
                recursive: false,
            },
            xml_tags: vec!["tag1".into(), "tag2".into()],
            values: vec!["val1".into()],
            show_old: false,
        };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }
}
