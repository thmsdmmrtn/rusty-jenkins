//! `rj run <profile>` — trigger a build from a named profile in `rj.toml`.
//!
//! This is the command that solves "parameter overload": a Jenkins job (or a
//! Jenkinsfile) exposes a single `PROFILE` choice instead of thirty string and
//! boolean inputs. The parameter bundle lives in `rj.toml` under version
//! control, where it is diffable, reviewable, and testable with `--dry-run`.

use crate::cli::RunArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::build::parse_params;
use crate::commands::sweep::{extract_queue_id, poll_queue, wait_for_completion};
use crate::manifest::{Manifest, ResolvedProfile};
use anyhow::{Context, Result};
use colored::Colorize;

pub async fn run(client: &JenkinsClient, manifest: &Manifest, args: &RunArgs) -> Result<()> {
    // Resolve the profile (flattening its `extends` chain) …
    let mut resolved = manifest.resolve(&args.profile)?;

    // … apply the optional job override …
    if let Some(job) = &args.job {
        resolved.job = job.clone();
    }

    // … then layer CLI `-p` overrides on top (highest precedence).
    for (key, value) in parse_params(&args.params)? {
        resolved.set_param(key, value);
    }

    print_resolved(&resolved);

    if args.dry_run {
        println!("\n{}", "Dry run — no build triggered.".dimmed());
        return Ok(());
    }

    let build_num = trigger(client, &resolved).await?;
    println!("\n{} build {}", "Triggered".green(), format!("#{build_num}").cyan());

    if args.wait {
        let result = wait_for_completion(client, &resolved.job, build_num, args.poll_ms)
            .await?
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let colored = match result.as_str() {
            "SUCCESS" => result.green().to_string(),
            "FAILURE" => result.red().to_string(),
            "UNSTABLE" => result.yellow().to_string(),
            _ => result.dimmed().to_string(),
        };
        println!("Result: {colored}");
        // Make the exit code meaningful for pipelines: any non-SUCCESS fails.
        if result != "SUCCESS" {
            anyhow::bail!("build {build_num} finished with result {result}");
        }
    }

    Ok(())
}

// ── Trigger + queue resolution ─────────────────────────────────────────────────

/// POST the build and poll the queue until Jenkins assigns a build number.
/// Uses `/buildWithParameters` when the profile carries params, `/build` otherwise.
async fn trigger(client: &JenkinsClient, resolved: &ResolvedProfile) -> Result<u64> {
    let endpoint = if resolved.params.is_empty() {
        "build"
    } else {
        "buildWithParameters"
    };
    let path = format!("job/{}/{endpoint}", encode_job_path(&resolved.job));

    let resp = client
        .post(&path)
        .await?
        .form(&resolved.params)
        .send()
        .await
        .context("triggering build")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "Jenkins returned HTTP {status} triggering '{}' — check the job name and permissions",
            resolved.job
        );
    }

    let location = resp
        .headers()
        .get("Location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("no Location header in build response"))?
        .to_string();

    let queue_id = extract_queue_id(&location)?;
    // poll_ms of 0 here would busy-loop; the queue usually resolves in one poll.
    poll_queue(client, queue_id, 500).await
}

// ── Display ─────────────────────────────────────────────────────────────────────

fn print_resolved(r: &ResolvedProfile) {
    println!("Profile: {}", r.name.cyan().bold());
    if let Some(desc) = &r.description {
        println!("Desc:    {desc}");
    }
    println!("Job:     {}", r.job.cyan());
    if r.params.is_empty() {
        println!("Params:  {}", "(none)".dimmed());
    } else {
        println!("Params:");
        for (k, v) in &r.params {
            println!("  {:<24} = {}", k, v.yellow());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MANIFEST: &str = r#"
        [profiles.smoke]
        job = "platform/tests"
        [profiles.smoke.params]
        SUITE = "smoke"
        PARALLELISM = 2
    "#;

    fn crumb() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(&serde_json::json!({
            "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
        }))
    }

    fn args(profile: &str) -> RunArgs {
        RunArgs {
            profile: profile.to_string(),
            params: vec![],
            job: None,
            dry_run: false,
            wait: false,
            poll_ms: 0,
        }
    }

    #[tokio::test]
    async fn dry_run_does_not_touch_the_network() {
        // Pointing at a dead address proves no request is made.
        let client = JenkinsClient::new("http://127.0.0.1:1", "u", "p");
        let manifest = parse(MANIFEST).unwrap();
        let mut a = args("smoke");
        a.dry_run = true;
        run(&client, &manifest, &a).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_profile_errors_before_network() {
        let client = JenkinsClient::new("http://127.0.0.1:1", "u", "p");
        let manifest = parse(MANIFEST).unwrap();
        let err = run(&client, &manifest, &args("nope")).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn run_posts_resolved_params_to_build_with_parameters() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/platform/job/tests/buildWithParameters"))
            .and(body_string_contains("SUITE=smoke"))
            .and(body_string_contains("PARALLELISM=2"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", format!("{}/queue/item/3/", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/queue/item/3/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 88, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let manifest = parse(MANIFEST).unwrap();
        run(&client, &manifest, &args("smoke")).await.unwrap();
    }

    #[tokio::test]
    async fn cli_param_override_wins_over_profile() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // SUITE overridden to "regression" on the CLI must appear in the body,
        // and the original "smoke" value must not.
        Mock::given(method("POST"))
            .and(path("/job/platform/job/tests/buildWithParameters"))
            .and(body_string_contains("SUITE=regression"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", format!("{}/queue/item/1/", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/queue/item/1/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 5, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let manifest = parse(MANIFEST).unwrap();
        let mut a = args("smoke");
        a.params = vec!["SUITE=regression".to_string()];
        run(&client, &manifest, &a).await.unwrap();
    }

    #[tokio::test]
    async fn wait_flag_fails_run_on_non_success_result() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/platform/job/tests/buildWithParameters"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", format!("{}/queue/item/1/", server.uri())),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/queue/item/1/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 9, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/platform/job/tests/9/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "FAILURE"
            })))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let manifest = parse(MANIFEST).unwrap();
        let mut a = args("smoke");
        a.wait = true;
        let err = run(&client, &manifest, &a).await.unwrap_err();
        assert!(err.to_string().contains("FAILURE"));
    }
}
