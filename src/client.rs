use anyhow::{Context, Result};
use reqwest::{Client, RequestBuilder, Response};
use serde::Deserialize;
use tokio::sync::Mutex;

// ── CSRF crumb types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CrumbResponse {
    crumb: String,
    #[serde(rename = "crumbRequestField")]
    crumb_request_field: String,
}

/// A cached CSRF crumb: (header-field-name, crumb-value).
type CrumbEntry = (String, String);

// ── Client ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct JenkinsClient {
    pub base_url: String,
    username: String,
    token: String,
    http: Client,
    /// Lazily fetched and cached for the lifetime of this client instance.
    crumb_cache: Mutex<Option<CrumbEntry>>,
}

impl JenkinsClient {
    pub fn new(base_url: impl Into<String>, username: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username: username.into(),
            token: token.into(),
            http: Client::new(),
            crumb_cache: Mutex::new(None),
        }
    }

    /// Attach Basic Auth credentials to any request builder.
    fn with_auth(&self, req: RequestBuilder) -> RequestBuilder {
        req.basic_auth(&self.username, Some(&self.token))
    }

    /// Build a full URL from a relative path.
    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    // ── Public HTTP helpers ───────────────────────────────────────────────────

    /// Authenticated GET — returns the raw `Response` so callers can decide
    /// whether to parse JSON, read text, stream bytes, etc.
    pub async fn get(&self, path: &str) -> Result<Response> {
        let url = self.url(path);
        self.with_auth(self.http.get(&url))
            .send()
            .await
            .with_context(|| format!("GET {url}"))
    }

    /// Authenticated POST with CSRF crumb pre-attached.
    /// Returns a `RequestBuilder` so callers can attach a body before sending.
    pub async fn post(&self, path: &str) -> Result<RequestBuilder> {
        let url = self.url(path);
        let (field, value) = self.crumb().await?;
        let req = self.with_auth(self.http.post(&url)).header(field, value);
        Ok(req)
    }

    // ── CSRF crumb ────────────────────────────────────────────────────────────

    /// Return the crumb, fetching it from Jenkins if not yet cached.
    async fn crumb(&self) -> Result<CrumbEntry> {
        let mut cache = self.crumb_cache.lock().await;
        if let Some(entry) = &*cache {
            return Ok(entry.clone());
        }
        let entry = self.fetch_crumb().await?;
        *cache = Some(entry.clone());
        Ok(entry)
    }

    async fn fetch_crumb(&self) -> Result<CrumbEntry> {
        let url = self.url("crumbIssuer/api/json");
        let resp = self.with_auth(self.http.get(&url))
            .send()
            .await
            .context("requesting CSRF crumb")?;

        let status = resp.status();
        if !status.is_success() {
            // Jenkins instances without CSRF protection return 404 here.
            anyhow::bail!("crumbIssuer returned HTTP {status} — CSRF may be disabled on this server");
        }

        let data: CrumbResponse = resp
            .json()
            .await
            .context("deserialising CSRF crumb response")?;

        Ok((data.crumb_request_field, data.crumb))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const USER: &str = "thomas";
    const TOKEN: &str = "test-token";

    fn client(base_url: &str) -> JenkinsClient {
        JenkinsClient::new(base_url, USER, TOKEN)
    }

    // ── Basic Auth ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_attaches_basic_auth_header() {
        let server = MockServer::start().await;

        // "thomas:test-token" base64-encoded = "dGhvbWFzOnRlc3QtdG9rZW4="
        Mock::given(method("GET"))
            .and(path("/job/my-job/api/json"))
            .and(header("authorization", "Basic dGhvbWFzOnRlc3QtdG9rZW4="))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let resp = client(&server.uri())
            .get("/job/my-job/api/json")
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        // wiremock verifies the expectation on drop
    }

    #[tokio::test]
    async fn get_without_auth_header_is_rejected_by_mock() {
        let server = MockServer::start().await;

        // Only accept requests that carry the Authorization header.
        Mock::given(method("GET"))
            .and(path("/probe"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        client(&server.uri()).get("/probe").await.unwrap();
    }

    // ── CSRF crumb fetch ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn post_fetches_csrf_crumb_and_attaches_it() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "crumb": "abc123",
                    "crumbRequestField": "Jenkins-Crumb"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/deploy/build"))
            .and(header("Jenkins-Crumb", "abc123"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let resp = c.post("/job/deploy/build").await.unwrap().send().await.unwrap();
        assert_eq!(resp.status(), 201);
    }

    // ── Crumb caching ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn crumb_is_fetched_only_once_across_multiple_posts() {
        let server = MockServer::start().await;

        // Crumb endpoint must be called exactly once.
        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "crumb": "cached-value",
                    "crumbRequestField": "Jenkins-Crumb"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let c = client(&server.uri());
        c.post("/job/a/build").await.unwrap().send().await.unwrap();
        c.post("/job/b/build").await.unwrap().send().await.unwrap();
        c.post("/job/c/build").await.unwrap().send().await.unwrap();
        // wiremock asserts `.expect(1)` on drop — three POSTs, one crumb fetch
    }

    // ── Crumb error handling ──────────────────────────────────────────────────

    #[tokio::test]
    async fn post_returns_error_when_crumb_endpoint_fails() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let result = client(&server.uri()).post("/job/x/build").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("404"), "expected 404 in error, got: {msg}");
    }
}
