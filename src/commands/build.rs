use crate::cli::BuildArgs;
use crate::client::{encode_job_path, JenkinsClient};
use anyhow::{Context, Result};

// ── Parameter parsing ─────────────────────────────────────────────────────────

/// Split each `"KEY=VALUE"` string into a `(key, value)` pair.
/// Splits on the *first* `=` only, so values containing `=` are handled correctly.
pub fn parse_params(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow::anyhow!(
                    "invalid parameter '{s}': expected KEY=VALUE format"
                ))
        })
        .collect()
}

// ── Command entry point ───────────────────────────────────────────────────────

pub async fn run(client: &JenkinsClient, args: &BuildArgs) -> Result<()> {
    let encoded = encode_job_path(&args.job);

    let (path, pairs) = if args.params.is_empty() {
        (format!("job/{encoded}/build"), vec![])
    } else {
        let pairs = parse_params(&args.params)?;
        (format!("job/{encoded}/buildWithParameters"), pairs)
    };

    let mut req = client.post(&path).await?;

    // Jenkins requires a non-empty body on /build even with no parameters.
    // Using form() satisfies this; an empty vec produces an empty form body.
    req = req.form(&pairs);

    let resp = req.send().await.context("sending build request")?;
    let status = resp.status();

    if !status.is_success() {
        anyhow::bail!(
            "Jenkins returned HTTP {status} — check the job name and your permissions"
        );
    }

    // Jenkins 201 response carries the queue item URL in the Location header.
    match resp.headers().get("Location") {
        Some(loc) => println!("Queued: {}", loc.to_str().unwrap_or("(unreadable URL)")),
        None => println!("Build triggered (no Location header returned)."),
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── parse_params ──────────────────────────────────────────────────────────

    #[test]
    fn parse_params_splits_on_first_equals_sign() {
        let raw = vec!["ENV=staging".to_string(), "VERSION=1.2.3".to_string()];
        let pairs = parse_params(&raw).unwrap();
        assert_eq!(pairs, vec![
            ("ENV".to_string(),     "staging".to_string()),
            ("VERSION".to_string(), "1.2.3".to_string()),
        ]);
    }

    #[test]
    fn parse_params_value_may_contain_equals_sign() {
        // e.g. passing a URL or a base64 string as a value
        let raw = vec!["MSG=hello=world".to_string()];
        let pairs = parse_params(&raw).unwrap();
        assert_eq!(pairs, vec![("MSG".to_string(), "hello=world".to_string())]);
    }

    #[test]
    fn parse_params_empty_value_is_accepted() {
        let raw = vec!["FLAG=".to_string()];
        let pairs = parse_params(&raw).unwrap();
        assert_eq!(pairs, vec![("FLAG".to_string(), String::new())]);
    }

    #[test]
    fn parse_params_errors_when_equals_is_absent() {
        let raw = vec!["NOEQUALSSIGN".to_string()];
        let err = parse_params(&raw).unwrap_err();
        assert!(err.to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn parse_params_mixed_valid_and_invalid_stops_at_first_error() {
        let raw = vec!["GOOD=ok".to_string(), "BAD".to_string()];
        assert!(parse_params(&raw).is_err());
    }

    // ── Plain build (no parameters) ───────────────────────────────────────────

    #[tokio::test]
    async fn plain_build_posts_to_build_endpoint() {
        let server = MockServer::start().await;

        // Crumb mock
        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/nightly/build"))
            .and(header("Jenkins-Crumb", "tok"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", "http://jenkins/queue/item/7/"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = BuildArgs { job: "nightly".to_string(), params: vec![] };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn plain_build_returns_error_on_non_2xx() {
        let server = MockServer::start().await;

        Mock::given(method("GET")).and(path("/crumbIssuer/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "crumb": "x", "crumbRequestField": "Jenkins-Crumb"
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST")).and(path("/job/locked/build"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = BuildArgs { job: "locked".to_string(), params: vec![] };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── Parameterized build ───────────────────────────────────────────────────

    #[tokio::test]
    async fn parameterized_build_posts_to_build_with_parameters_endpoint() {
        let server = MockServer::start().await;

        Mock::given(method("GET")).and(path("/crumbIssuer/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/deploy/buildWithParameters"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", "http://jenkins/queue/item/42/"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = BuildArgs {
            job: "deploy".to_string(),
            params: vec!["ENV=staging".to_string(), "VERSION=1.2.3".to_string()],
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn parameterized_build_encodes_params_in_form_body() {
        let server = MockServer::start().await;

        Mock::given(method("GET")).and(path("/crumbIssuer/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
            })))
            .mount(&server)
            .await;

        // Verify both key=value pairs appear in the form-encoded body.
        Mock::given(method("POST"))
            .and(path("/job/deploy/buildWithParameters"))
            .and(body_string_contains("ENV=staging"))
            .and(body_string_contains("VERSION=1.2.3"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = BuildArgs {
            job: "deploy".to_string(),
            params: vec!["ENV=staging".to_string(), "VERSION=1.2.3".to_string()],
        };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_param_format_short_circuits_before_any_http_call() {
        // No mock server needed — we should never reach the network.
        let client = crate::client::JenkinsClient::new("http://127.0.0.1:1", "u", "p");
        let args = BuildArgs {
            job: "deploy".to_string(),
            params: vec!["NOEQUALSSIGN".to_string()],
        };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("KEY=VALUE"));
    }
}
