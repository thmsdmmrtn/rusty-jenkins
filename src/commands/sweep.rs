use crate::cli::SweepArgs;
use crate::client::JenkinsClient;
use crate::commands::build::parse_params;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ── Jenkins API types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct QueueItem {
    cancelled: Option<bool>,
    executable: Option<QueueExecutable>,
}

#[derive(Deserialize)]
struct QueueExecutable {
    number: u64,
}

#[derive(Deserialize)]
struct BuildStatus {
    building: bool,
    result: Option<String>,
}

// ── Queue URL parsing ─────────────────────────────────────────────────────────

/// Extract the numeric queue item ID from a Jenkins `Location` header URL.
/// e.g. `http://jenkins.example.com:8080/queue/item/123/` → `123`
pub fn extract_queue_id(location: &str) -> Result<u64> {
    location
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("could not parse queue item ID from URL: {location}"))
}

// ── Command entry point ───────────────────────────────────────────────────────

pub async fn run(client: &JenkinsClient, args: &SweepArgs) -> Result<()> {
    if args.values.is_empty() {
        anyhow::bail!("provide at least one --value to sweep over");
    }

    let fixed = parse_params(&args.params)?;
    let out_dir = Path::new(&args.output_dir);
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory '{}'", args.output_dir))?;

    let total = args.values.len();
    for (i, value) in args.values.iter().enumerate() {
        println!("\n[{}/{}] {}={}", i + 1, total, args.param_name, value);

        // Build the full parameter list: fixed params + the varying one.
        let mut params = fixed.clone();
        params.push((args.param_name.clone(), value.clone()));

        // 1. Trigger build and resolve to a build number via the queue.
        let build_num = match trigger_and_resolve(client, &args.job, &params, args.poll_ms).await {
            Ok(n) => {
                println!("  Queued as build #{n}");
                n
            }
            Err(e) => {
                eprintln!("  Could not trigger build: {e:#}");
                continue;
            }
        };

        // 2. Wait for the build to finish.
        let result = match wait_for_completion(client, &args.job, build_num, args.poll_ms).await {
            Ok(r) => r.unwrap_or_else(|| "UNKNOWN".to_string()),
            Err(e) => {
                eprintln!("  Error waiting for build: {e:#}");
                continue;
            }
        };
        println!("  Result: {result}");

        // 3. Save the console log.
        let log_path = log_filename(out_dir, &args.job, &args.param_name, value, build_num);
        match save_log(client, &args.job, build_num, &log_path).await {
            Ok(()) => println!("  Log:    {}", log_path.display()),
            Err(e) => eprintln!("  Could not save log: {e:#}"),
        }
    }

    println!("\nSweep complete. Logs in '{}'.", args.output_dir);
    Ok(())
}

// ── Step 1: trigger build + poll queue until build number assigned ─────────────

async fn trigger_and_resolve(
    client: &JenkinsClient,
    job: &str,
    params: &[(String, String)],
    poll_ms: u64,
) -> Result<u64> {
    let encoded = job.replace(' ', "%20");
    let resp = client
        .post(&format!("job/{encoded}/buildWithParameters"))
        .await?
        .form(params)
        .send()
        .await
        .context("triggering build")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Jenkins returned HTTP {status} when triggering build");
    }

    let location = resp
        .headers()
        .get("Location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("no Location header in build response"))?
        .to_string();

    let queue_id = extract_queue_id(&location)?;
    poll_queue(client, queue_id, poll_ms).await
}

async fn poll_queue(client: &JenkinsClient, queue_id: u64, poll_ms: u64) -> Result<u64> {
    loop {
        let resp = client
            .get(&format!("queue/item/{queue_id}/api/json"))
            .await
            .context("polling queue item")?;

        let item: QueueItem = resp.json().await.context("parsing queue item")?;

        if item.cancelled.unwrap_or(false) {
            anyhow::bail!("build was cancelled while waiting in the queue");
        }

        if let Some(exec) = item.executable {
            return Ok(exec.number);
        }

        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
}

// ── Step 2: poll build until it finishes ─────────────────────────────────────

async fn wait_for_completion(
    client: &JenkinsClient,
    job: &str,
    build: u64,
    poll_ms: u64,
) -> Result<Option<String>> {
    let encoded = job.replace(' ', "%20");
    loop {
        let resp = client
            .get(&format!("job/{encoded}/{build}/api/json?tree=building,result"))
            .await
            .context("polling build status")?;

        let s: BuildStatus = resp.json().await.context("parsing build status")?;

        if !s.building {
            return Ok(s.result);
        }

        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
}

// ── Step 3: fetch the complete console log and write to disk ─────────────────

async fn save_log(client: &JenkinsClient, job: &str, build: u64, path: &Path) -> Result<()> {
    let encoded = job.replace(' ', "%20");
    let resp = client
        .get(&format!("job/{encoded}/{build}/consoleText"))
        .await
        .context("fetching console log")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("consoleText endpoint returned HTTP {status}");
    }

    let text = resp.text().await.context("reading console log")?;
    std::fs::write(path, text).with_context(|| format!("writing log to '{}'", path.display()))
}

fn log_filename(dir: &Path, job: &str, param: &str, value: &str, build: u64) -> PathBuf {
    // Sanitise value so it's safe to use as part of a filename.
    let safe_value = value.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '], "_");
    dir.join(format!("{job}__{param}__{safe_value}__#{build}.log"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn crumb() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(&serde_json::json!({
            "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
        }))
    }

    // ── extract_queue_id ──────────────────────────────────────────────────────

    #[test]
    fn extract_queue_id_parses_standard_url() {
        assert_eq!(
            extract_queue_id("http://jenkins.example.com:8080/queue/item/42/").unwrap(),
            42
        );
    }

    #[test]
    fn extract_queue_id_works_without_trailing_slash() {
        assert_eq!(
            extract_queue_id("http://jenkins.example.com:8080/queue/item/99").unwrap(),
            99
        );
    }

    #[test]
    fn extract_queue_id_errors_on_non_numeric_segment() {
        assert!(extract_queue_id("http://jenkins.example.com/queue/item/abc/").is_err());
    }

    // ── log_filename ──────────────────────────────────────────────────────────

    #[test]
    fn log_filename_sanitises_special_characters() {
        let p = log_filename(Path::new("/tmp"), "my-job", "ENV", "us-east-1/prod", 7);
        assert_eq!(p.file_name().unwrap(), "my-job__ENV__us-east-1_prod__#7.log");
    }

    #[test]
    fn log_filename_format_is_job_param_value_build() {
        let p = log_filename(Path::new("logs"), "deploy", "VERSION", "1.2.3", 55);
        assert_eq!(p.file_name().unwrap(), "deploy__VERSION__1.2.3__#55.log");
    }

    // ── poll_queue ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn poll_queue_returns_build_number_when_executable_present() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/queue/item/7/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 42, "url": "http://x" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let n = poll_queue(&client, 7, 0).await.unwrap();
        assert_eq!(n, 42);
    }

    #[tokio::test]
    async fn poll_queue_retries_until_executable_appears() {
        let server = MockServer::start().await;

        // First response: not yet assigned
        Mock::given(method("GET"))
            .and(path("/queue/item/3/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({})))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second response: build assigned
        Mock::given(method("GET"))
            .and(path("/queue/item/3/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 10, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let n = poll_queue(&client, 3, 0).await.unwrap();
        assert_eq!(n, 10);
    }

    #[tokio::test]
    async fn poll_queue_errors_when_build_is_cancelled() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/queue/item/5/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "cancelled": true
            })))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let err = poll_queue(&client, 5, 0).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    // ── wait_for_completion ───────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_completion_returns_result_when_not_building() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/42/api/json"))
            .and(query_param("tree", "building,result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "SUCCESS"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let result = wait_for_completion(&client, "my-job", 42, 0).await.unwrap();
        assert_eq!(result.as_deref(), Some("SUCCESS"));
    }

    #[tokio::test]
    async fn wait_for_completion_polls_while_building_is_true() {
        let server = MockServer::start().await;

        // First poll: still running
        Mock::given(method("GET"))
            .and(path("/job/my-job/5/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": true, "result": null
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second poll: done
        Mock::given(method("GET"))
            .and(path("/job/my-job/5/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "FAILURE"
            })))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let result = wait_for_completion(&client, "my-job", 5, 0).await.unwrap();
        assert_eq!(result.as_deref(), Some("FAILURE"));
    }

    // ── save_log ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn save_log_writes_console_text_to_disk() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/9/consoleText"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Build output here\n"))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_sweep_test.log");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        save_log(&client, "my-job", 9, &tmp).await.unwrap();

        let contents = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(contents, "Build output here\n");
        std::fs::remove_file(&tmp).ok();
    }

    // ── Full run() sweep ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_triggers_one_build_per_value_and_saves_logs() {
        let server = MockServer::start().await;

        // Crumb
        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        for (value, queue_id, build_num) in [("staging", 1u64, 10u64), ("prod", 2, 11)] {
            // POST trigger → Location header
            Mock::given(method("POST"))
                .and(path("/job/deploy/buildWithParameters"))
                .and(body_string_contains(format!("ENV={value}")))
                .respond_with(
                    ResponseTemplate::new(201).append_header(
                        "Location",
                        format!("{}/queue/item/{queue_id}/", server.uri()),
                    ),
                )
                .expect(1)
                .mount(&server)
                .await;

            // Queue item → build number
            Mock::given(method("GET"))
                .and(path(format!("/queue/item/{queue_id}/api/json")))
                .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "executable": { "number": build_num, "url": "http://x" }
                })))
                .mount(&server)
                .await;

            // Build status → complete
            Mock::given(method("GET"))
                .and(path(format!("/job/deploy/{build_num}/api/json")))
                .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "building": false, "result": "SUCCESS"
                })))
                .mount(&server)
                .await;

            // Console log
            Mock::given(method("GET"))
                .and(path(format!("/job/deploy/{build_num}/consoleText")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(format!("Log for {value}\n")),
                )
                .mount(&server)
                .await;
        }

        let tmp_dir = std::env::temp_dir().join("rj_sweep_run_test");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = SweepArgs {
            job: "deploy".to_string(),
            param_name: "ENV".to_string(),
            values: vec!["staging".to_string(), "prod".to_string()],
            params: vec![],
            output_dir: tmp_dir.to_str().unwrap().to_string(),
            poll_ms: 0,
        };

        run(&client, &args).await.unwrap();

        // Verify log files were written with expected content.
        let staging_log = tmp_dir.join("deploy__ENV__staging__#10.log");
        let prod_log    = tmp_dir.join("deploy__ENV__prod__#11.log");
        assert_eq!(std::fs::read_to_string(&staging_log).unwrap(), "Log for staging\n");
        assert_eq!(std::fs::read_to_string(&prod_log).unwrap(),    "Log for prod\n");

        std::fs::remove_dir_all(&tmp_dir).ok();
    }
}
