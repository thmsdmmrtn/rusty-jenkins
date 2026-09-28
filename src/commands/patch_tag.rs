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
    if !args.only_if.is_empty() && args.only_if.len() != args.xml_tags.len() {
        anyhow::bail!(
            "--only-if count ({}) does not match --xml-tag count ({}) — give one \
             --only-if per --xml-tag, or none for an unconditional patch",
            args.only_if.len(),
            args.xml_tags.len()
        );
    }

    let jobs = resolve_jobs(client, &args.target).await?;
    let total = jobs.len();
    let (mut patched, mut skipped, mut failed) = (0usize, 0usize, 0usize);

    for (i, job) in jobs.iter().enumerate() {
        println!("{} {}", format!("[{}/{}]", i + 1, total).dimmed(), job.cyan());
        match apply(client, job, &args.xml_tags, &args.values, &args.only_if, args.show_old).await {
            Ok(Outcome::Patched(old_values)) => {
                patched += 1;
                for (j, (tag, new_val)) in args.xml_tags.iter().zip(args.values.iter()).enumerate() {
                    let tag_fmt = format!("<{tag}>").cyan().to_string();
                    let new_fmt = new_val.green().to_string();
                    match old_values.get(j).and_then(|v| v.as_ref()) {
                        Some(prev) => println!("  {tag_fmt}: {} → {new_fmt}", prev.yellow()),
                        None       => println!("  {tag_fmt} → {new_fmt}"),
                    }
                }
            }
            Ok(Outcome::Skipped(reason)) => {
                skipped += 1;
                println!("  {} {reason}", "skipped —".yellow());
            }
            Err(e) => {
                failed += 1;
                println!("  {} {e:#}", "FAILED —".red());
            }
        }
    }

    println!(
        "\n{}",
        format!("{patched} patched, {skipped} skipped, {failed} failed").dimmed()
    );
    Ok(())
}

/// Result of processing one job.
#[derive(Debug)]
enum Outcome {
    /// Config was uploaded. Holds old values per tag (populated only with `show_old`).
    Patched(Vec<Option<String>>),
    /// An `--only-if` condition didn't hold, so nothing was uploaded.
    Skipped(String),
}

/// Fetch config.xml once, check any `--only-if` conditions, apply all tag
/// patches, and upload once. Conditions are evaluated against the same config
/// that gets rewritten, so there is no gap between checking and patching.
async fn apply(
    client: &JenkinsClient,
    job: &str,
    tags: &[String],
    values: &[String],
    conditions: &[String],
    show_old: bool,
) -> Result<Outcome> {
    let path = format!("job/{}/config.xml", encode_job_path(job));

    let resp = client.get(&path).await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET config.xml returned HTTP {status}");
    }
    let original = resp.text().await.context("reading config.xml")?;

    // Every condition must match exactly (the same text `tag list` prints),
    // otherwise the job is left untouched.
    for (tag, expected) in tags.iter().zip(conditions.iter()) {
        match read_xml_tag(&original, tag)? {
            Some(current) if current == *expected => {}
            Some(current) => {
                return Ok(Outcome::Skipped(format!(
                    "<{tag}> is \"{current}\" (only-if \"{expected}\")"
                )));
            }
            None => {
                return Ok(Outcome::Skipped(format!(
                    "<{tag}> not found (only-if \"{expected}\")"
                )));
            }
        }
    }

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
    Ok(Outcome::Patched(old_values))
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
            only_if: vec![],
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
            only_if: vec![],
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
            only_if: vec![],
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
            only_if: vec![],
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
            only_if: vec![],
            show_old: false,
        };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    // ── --only-if ─────────────────────────────────────────────────────────────

    fn xml_with_foo(value: &str) -> String {
        format!("<project><foo>{value}</foo><other>keep</other></project>")
    }

    fn only_if_args(jobs: &[&str], tags: &[&str], values: &[&str], only_if: &[&str]) -> PatchTagArgs {
        PatchTagArgs {
            target: crate::cli::JobTarget {
                paths: vec![],
                job_names: jobs.iter().map(|s| s.to_string()).collect(),
                recursive: false,
            },
            xml_tags: tags.iter().map(|s| s.to_string()).collect(),
            values: values.iter().map(|s| s.to_string()).collect(),
            only_if: only_if.iter().map(|s| s.to_string()).collect(),
            show_old: false,
        }
    }

    #[tokio::test]
    async fn only_if_patches_matching_job_and_skips_non_matching() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // job1 has foo=bar → must be patched exactly once.
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml_with_foo("bar")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/job/abc/job/job1/config.xml"))
            .and(body_string_contains("<foo>new</foo>"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // job2 has foo=baz → must never be uploaded.
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job2/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml_with_foo("baz")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/job/abc/job/job2/config.xml"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = only_if_args(&["abc/job1", "abc/job2"], &["foo"], &["new"], &["bar"]);
        run(&client, &args).await.unwrap();
        // wiremock verifies expect(1) / expect(0) on drop
    }

    /// Serve `xml` for job1's config, with a crumb mounted so any wrongful
    /// upload would genuinely reach Jenkins — and trip the `expect(0)`.
    async fn server_with_job1(xml: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml.to_string()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        server
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn only_if_skips_job_where_tag_is_missing() {
        let server = server_with_job1("<project><other>x</other></project>").await;
        let client = JenkinsClient::new(&server.uri(), "u", "p");

        // Assert on the outcome itself: an unconditional patch would *fail* here
        // (tag not found), so "no upload" alone can't prove the job was skipped.
        let outcome = apply(&client, "abc/job1", &strings(&["foo"]), &strings(&["new"]), &strings(&["bar"]), false)
            .await
            .unwrap();
        match outcome {
            Outcome::Skipped(reason) => assert!(reason.contains("not found"), "got: {reason}"),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn only_if_requires_every_condition_to_match() {
        // foo matches "bar", but other is "keep" rather than the expected "nope".
        let server = server_with_job1(&xml_with_foo("bar")).await;
        let client = JenkinsClient::new(&server.uri(), "u", "p");

        let outcome = apply(
            &client,
            "abc/job1",
            &strings(&["foo", "other"]),
            &strings(&["new", "changed"]),
            &strings(&["bar", "nope"]),
            false,
        )
        .await
        .unwrap();
        match outcome {
            Outcome::Skipped(reason) => {
                assert!(reason.contains("<other>") && reason.contains("keep"), "got: {reason}")
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn only_if_patches_all_tags_when_every_condition_matches() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/job/abc/job/job1/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml_with_foo("bar")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/job/abc/job/job1/config.xml"))
            .and(body_string_contains("<foo>new</foo>"))
            .and(body_string_contains("<other>changed</other>"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = only_if_args(&["abc/job1"], &["foo", "other"], &["new", "changed"], &["bar", "keep"]);
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn only_if_count_mismatch_errors_before_any_request() {
        // Dead address proves validation happens before the network.
        let client = JenkinsClient::new("http://127.0.0.1:1", "u", "p");
        let args = only_if_args(&["abc/job1"], &["foo", "other"], &["a", "b"], &["bar"]);
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("--only-if count"), "got: {err}");
    }
}
