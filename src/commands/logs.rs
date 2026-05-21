use crate::cli::LogsArgs;
use crate::client::JenkinsClient;
use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use serde::Deserialize;
use std::time::Duration;

// ── Minimal types for resolving the latest build number ───────────────────────

#[derive(Deserialize)]
struct JobSummary {
    #[serde(rename = "lastBuild")]
    last_build: Option<LastBuild>,
}

#[derive(Deserialize)]
struct LastBuild {
    number: u64,
}

// ── Header helpers (pub so tests can call them directly) ──────────────────────

/// Read `X-Text-Size` — the byte offset Jenkins wants for the *next* request.
pub fn next_start(headers: &HeaderMap) -> u64 {
    headers
        .get("X-Text-Size")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Return `true` when Jenkins sets `X-More-Data: true`, meaning the build is
/// still running and more log lines will arrive.
pub fn more_data(headers: &HeaderMap) -> bool {
    headers
        .get("X-More-Data")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

// ── Command entry point ───────────────────────────────────────────────────────

pub async fn run(client: &JenkinsClient, args: &LogsArgs) -> Result<()> {
    let build = match args.build {
        Some(n) => n,
        None => resolve_latest(client, &args.job).await?,
    };

    println!("Streaming logs for {} #{}\n", args.job, build);
    stream(client, &args.job, build, args.poll_ms).await
}

async fn resolve_latest(client: &JenkinsClient, job: &str) -> Result<u64> {
    let encoded = job.replace(' ', "%20");
    let resp = client
        .get(&format!("job/{encoded}/api/json"))
        .await?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("could not fetch job info for '{job}': HTTP {status}");
    }

    let summary: JobSummary = resp.json().await.context("parsing job summary")?;
    summary
        .last_build
        .map(|b| b.number)
        .ok_or_else(|| anyhow::anyhow!("job '{job}' has no builds yet"))
}

async fn stream(client: &JenkinsClient, job: &str, build: u64, poll_ms: u64) -> Result<()> {
    let encoded = job.replace(' ', "%20");
    let base = format!("job/{encoded}/{build}/logText/progressiveText");
    let mut start: u64 = 0;

    loop {
        let resp = client
            .get(&format!("{base}?start={start}"))
            .await
            .with_context(|| format!("polling log at offset {start}"))?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("log endpoint returned HTTP {status}");
        }

        // Read headers *before* consuming the body.
        let advance = next_start(resp.headers());
        let keep_polling = more_data(resp.headers());

        let text = resp.text().await.context("reading log chunk")?;
        if !text.is_empty() {
            print!("{text}");
        }

        start = advance;

        if !keep_polling {
            break;
        }

        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use std::str::FromStr;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── next_start() ──────────────────────────────────────────────────────────

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_str(k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn next_start_parses_x_text_size() {
        let h = headers_with(&[("X-Text-Size", "1024")]);
        assert_eq!(next_start(&h), 1024);
    }

    #[test]
    fn next_start_returns_zero_when_header_absent() {
        assert_eq!(next_start(&HeaderMap::new()), 0);
    }

    #[test]
    fn next_start_returns_zero_on_non_numeric_value() {
        let h = headers_with(&[("X-Text-Size", "not-a-number")]);
        assert_eq!(next_start(&h), 0);
    }

    // ── more_data() ───────────────────────────────────────────────────────────

    #[test]
    fn more_data_true_when_header_is_true() {
        let h = headers_with(&[("X-More-Data", "true")]);
        assert!(more_data(&h));
    }

    #[test]
    fn more_data_true_is_case_insensitive() {
        let h = headers_with(&[("X-More-Data", "True")]);
        assert!(more_data(&h));
    }

    #[test]
    fn more_data_false_when_header_is_false() {
        let h = headers_with(&[("X-More-Data", "false")]);
        assert!(!more_data(&h));
    }

    #[test]
    fn more_data_false_when_header_absent() {
        assert!(!more_data(&HeaderMap::new()));
    }

    // ── Streaming loop ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stream_exits_immediately_when_x_more_data_is_absent() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/3/logText/progressiveText"))
            .and(query_param("start", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("Build complete.\n")
                    .append_header("X-Text-Size", "16"),
                // no X-More-Data → done
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        stream(&client, "my-job", 3, 0).await.unwrap();
        // wiremock asserts exactly 1 call on drop
    }

    #[tokio::test]
    async fn stream_polls_until_x_more_data_becomes_false() {
        let server = MockServer::start().await;

        // Page 1: more data coming
        Mock::given(method("GET"))
            .and(path("/job/my-job/5/logText/progressiveText"))
            .and(query_param("start", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("line 1\n")
                    .append_header("X-Text-Size", "7")
                    .append_header("X-More-Data", "true"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Page 2: build finished
        Mock::given(method("GET"))
            .and(path("/job/my-job/5/logText/progressiveText"))
            .and(query_param("start", "7"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("line 2\n")
                    .append_header("X-Text-Size", "14"),
                // no X-More-Data → done
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        stream(&client, "my-job", 5, 0).await.unwrap();
        // wiremock verifies both pages were requested in the right order
    }

    #[tokio::test]
    async fn stream_advances_offset_with_x_text_size() {
        let server = MockServer::start().await;

        // Offset jumps from 0 → 512 → 1024 across three polls.
        for (start_in, size_out, more) in [("0", "512", "true"), ("512", "1024", "true"), ("1024", "1024", "false")] {
            let mut tmpl = ResponseTemplate::new(200)
                .set_body_string("chunk\n")
                .append_header("X-Text-Size", size_out);
            if more == "true" {
                tmpl = tmpl.append_header("X-More-Data", "true");
            }
            Mock::given(method("GET"))
                .and(path("/job/pipe/9/logText/progressiveText"))
                .and(query_param("start", start_in))
                .respond_with(tmpl)
                .expect(1)
                .mount(&server)
                .await;
        }

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        stream(&client, "pipe", 9, 0).await.unwrap();
    }

    #[tokio::test]
    async fn stream_returns_error_on_non_2xx() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/gone/1/logText/progressiveText"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let err = stream(&client, "gone", 1, 0).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── resolve_latest() ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_latest_returns_last_build_number() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/api/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "lastBuild": { "number": 99, "url": "http://x" }
                })),
            )
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let n = resolve_latest(&client, "my-job").await.unwrap();
        assert_eq!(n, 99);
    }

    #[tokio::test]
    async fn resolve_latest_errors_when_no_builds_exist() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/fresh/api/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&serde_json::json!({ "lastBuild": null })),
            )
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let err = resolve_latest(&client, "fresh").await.unwrap_err();
        assert!(err.to_string().contains("no builds"));
    }

    // ── Full run() integration ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_resolves_build_number_and_streams_log() {
        let server = MockServer::start().await;

        // Job summary → build #7
        Mock::given(method("GET"))
            .and(path("/job/deploy/api/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "lastBuild": { "number": 7, "url": "http://x" }
                })),
            )
            .mount(&server)
            .await;

        // Single-page log for build #7
        Mock::given(method("GET"))
            .and(path("/job/deploy/7/logText/progressiveText"))
            .and(query_param("start", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("Finished: SUCCESS\n")
                    .append_header("X-Text-Size", "18"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = LogsArgs { job: "deploy".to_string(), build: None, poll_ms: 0 };
        run(&client, &args).await.unwrap();
    }
}
