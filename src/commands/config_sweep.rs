use crate::cli::ConfigSweepArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::sweep::{extract_queue_id, poll_queue, wait_for_completion};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use xmltree::{Element, XMLNode};

// ── XML path helpers ─────────────────────────────────────────────────────────
//
// Tag paths use `/` to navigate: "branches/name" finds the first <name> that
// is a descendant of the first <branches>, anywhere in the tree.
// Single-segment names keep the original any-depth depth-first behaviour.
//
// Examples for the Jenkins pipeline config XML:
//   "name"           → first <name> anywhere       (could be a param name)
//   "branches/name"  → <name> inside <branches>    (the branch spec)
//   "BranchSpec/name"→ <name> inside <...BranchSpec>

/// Read the text of the first element matching `tag_path`.
pub fn read_xml_tag(xml: &str, tag_path: &str) -> Result<Option<String>> {
    let root = Element::parse(xml.as_bytes()).context("parsing config.xml")?;
    let segs: Vec<&str> = tag_path.split('/').collect();
    Ok(find_by_path(&root, &segs))
}

/// Patch the text of the first element matching `tag_path`.
pub fn patch_xml_tag(xml: &str, tag_path: &str, new_value: &str) -> Result<String> {
    let mut root = Element::parse(xml.as_bytes()).context("parsing config.xml")?;
    let segs: Vec<&str> = tag_path.split('/').collect();

    if !replace_by_path(&mut root, &segs, new_value) {
        anyhow::bail!(
            "XML path <{tag_path}> not found in config.xml.\n\
             Tip: use `rj config get <job>` to inspect the XML and find the correct path.\n\
             Use / to disambiguate: e.g. `branches/name` instead of just `name`."
        );
    }

    let mut buf = Vec::new();
    root.write(&mut buf).context("serialising modified config.xml")?;
    String::from_utf8(buf).context("config.xml is not valid UTF-8 after patching")
}

// ── Path-based tree traversal ─────────────────────────────────────────────────

/// Navigate a `/`-separated path. Each segment is located by DFS within the
/// match of the previous segment. Single-segment = any-depth DFS (original behaviour).
fn find_by_path(el: &Element, path: &[&str]) -> Option<String> {
    match path {
        [] => None,
        [tag] => dfs_text(el, tag),
        [head, rest @ ..] => dfs_then_path(el, head, rest),
    }
}

/// DFS for the first element named `head`; once found, continue with `rest`.
fn dfs_then_path(el: &Element, head: &str, rest: &[&str]) -> Option<String> {
    if el.name == head {
        return find_by_path(el, rest);
    }
    for child in &el.children {
        if let XMLNode::Element(c) = child {
            if let Some(v) = dfs_then_path(c, head, rest) {
                return Some(v);
            }
        }
    }
    None
}

/// DFS for the first element named `tag`; return its text content.
fn dfs_text(el: &Element, tag: &str) -> Option<String> {
    if el.name == tag {
        let t: String = el.children.iter()
            .filter_map(|n| if let XMLNode::Text(t) = n { Some(t.as_str()) } else { None })
            .collect();
        return Some(t);
    }
    for child in &el.children {
        if let XMLNode::Element(c) = child { if let Some(v) = dfs_text(c, tag) { return Some(v); } }
    }
    None
}

fn replace_by_path(el: &mut Element, path: &[&str], value: &str) -> bool {
    match path {
        [] => false,
        [tag] => replace_first(el, tag, value),
        [head, rest @ ..] => replace_dfs_then_path(el, head, rest, value),
    }
}

fn replace_dfs_then_path(el: &mut Element, head: &str, rest: &[&str], value: &str) -> bool {
    if el.name == head {
        return replace_by_path(el, rest, value);
    }
    for child in &mut el.children {
        if let XMLNode::Element(c) = child {
            if replace_dfs_then_path(c, head, rest, value) { return true; }
        }
    }
    false
}

fn replace_first(el: &mut Element, tag: &str, value: &str) -> bool {
    if el.name == tag {
        el.children = vec![XMLNode::Text(value.to_string())];
        return true;
    }
    for child in &mut el.children {
        if let XMLNode::Element(c) = child { if replace_first(c, tag, value) { return true; } }
    }
    false
}

// ── Command entry point ───────────────────────────────────────────────────────

pub async fn run(client: &JenkinsClient, args: &ConfigSweepArgs) -> Result<()> {
    if args.values.is_empty() {
        anyhow::bail!("provide at least one --value to sweep over");
    }

    // Config is always read/written on the parent pipeline job.
    // Builds, waits, and logs target the specific branch when --branch is given.
    let build_target = match &args.branch {
        Some(branch) => format!("{}/{}", args.job, branch),
        None => args.job.clone(),
    };

    if let Some(branch) = &args.branch {
        println!(
            "Config: '{}' — Build target: '{}/{}' (no branch scan triggered)",
            args.job, args.job, branch
        );
    }

    let out_dir = Path::new(&args.output_dir);
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory '{}'", args.output_dir))?;

    // Fetch the original config once — we patch a copy each iteration.
    println!("Fetching config.xml for '{}'…", args.job);
    let original_xml = fetch_config(client, &args.job).await?;

    let total = args.values.len();
    for (i, value) in args.values.iter().enumerate() {
        println!("\n[{}/{}] <{}> = {}", i + 1, total, args.xml_tag, value);

        // 1. Patch the parent pipeline's config XML
        let patched = match patch_xml_tag(&original_xml, &args.xml_tag, value) {
            Ok(xml) => xml,
            Err(e) => {
                eprintln!("  Could not patch XML: {e:#}");
                continue;
            }
        };

        // 2. Upload the patched config to the parent pipeline
        if let Err(e) = upload_config(client, &args.job, &patched).await {
            eprintln!("  Could not upload config: {e:#}");
            continue;
        }
        println!("  Config updated.");

        // Small delay to let Jenkins finish processing the config change before
        // accepting a build request. Configurable via --post-config-delay-ms.
        if args.post_config_delay_ms > 0 {
            println!("  Waiting {}ms for Jenkins to apply config…", args.post_config_delay_ms);
            tokio::time::sleep(std::time::Duration::from_millis(args.post_config_delay_ms)).await;
        }

        // 3a. When targeting a specific branch, scan the parent pipeline first.
        //     After a repo change Jenkins marks branch jobs stale until a new
        //     scan confirms the branch exists in the new repo — without this the
        //     branch build returns 400 no matter how long we wait.
        if args.branch.is_some() && !args.skip_scan {
            println!("  Scanning parent pipeline to index new repository…");
            match run_scan(client, &args.job, args.poll_ms).await {
                Ok(()) => println!("  Scan complete."),
                Err(e) => eprintln!("  Scan failed: {e:#}. Attempting branch build anyway…"),
            }
            // Brief settling delay — Jenkins needs a moment after indexing
            // to mark branch jobs as buildable before accepting a build request.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        // 3b. Trigger a build on the target (branch job or parent pipeline)
        let build_num =
            match trigger_build(client, &build_target, args.poll_ms).await {
                Ok(n) => { println!("  Queued as build #{n}"); n }
                Err(e) => { eprintln!("  Could not trigger build: {e:#}"); continue; }
            };

        // 4. Wait for completion on the build target
        let result =
            match wait_for_completion(client, &build_target, build_num, args.poll_ms).await {
                Ok(r) => r.unwrap_or_else(|| "UNKNOWN".to_string()),
                Err(e) => { eprintln!("  Error waiting for build: {e:#}"); continue; }
            };
        println!("  Result: {result}");

        // 5. Save the console log from the build target
        let log_path = log_filename(out_dir, &build_target, &args.xml_tag, value, build_num);
        match save_log(client, &build_target, build_num, &log_path).await {
            Ok(()) => println!("  Log:    {}", log_path.display()),
            Err(e) => eprintln!("  Could not save log: {e:#}"),
        }
    }

    // 6. Restore original config unless the user opted out
    if args.no_restore {
        println!("\nSkipping config restore (--no-restore).");
    } else {
        print!("\nRestoring original config.xml… ");
        match upload_config(client, &args.job, &original_xml).await {
            Ok(()) => println!("done."),
            Err(e) => eprintln!("FAILED: {e:#}"),
        }
    }

    println!("\nConfig sweep complete. Logs in '{}'.", args.output_dir);
    Ok(())
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

async fn fetch_config(client: &JenkinsClient, job: &str) -> Result<String> {
    let resp = client
        .get(&format!("job/{}/config.xml", encode_job_path(job)))
        .await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Jenkins returned HTTP {status} fetching config.xml for '{job}'");
    }
    resp.text().await.context("reading config.xml body")
}

/// Trigger a branch scan (indexing run) on the parent pipeline and wait for
/// it to finish. This re-discovers branches in the new repository so Jenkins
/// accepts a branch build request without returning HTTP 400.
///
/// CloudBees CI and some Jenkins versions handle indexing synchronously and
/// return 200 with no Location header instead of queuing the scan. We handle
/// both cases: queue-based (poll to completion) and fire-and-forget (poll the
/// indexing sub-resource directly).
async fn run_scan(client: &JenkinsClient, parent_job: &str, poll_ms: u64) -> Result<()> {
    let resp = client
        .post(&format!("job/{}/build", encode_job_path(parent_job)))
        .await?
        .form(&Vec::<(String, String)>::new())
        .send()
        .await
        .context("triggering branch scan")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("scan trigger returned HTTP {status}");
    }

    // Case 1: Jenkins queued the scan — poll it to completion via the queue.
    if let Some(loc) = resp.headers().get("Location").and_then(|v| v.to_str().ok()) {
        if let Ok(queue_id) = extract_queue_id(loc) {
            if let Ok(scan_build) = poll_queue(client, queue_id, poll_ms).await {
                wait_for_completion(client, parent_job, scan_build, poll_ms).await?;
                return Ok(());
            }
        }
    }

    // Case 2: No Location header — CloudBees CI / some Jenkins versions start
    // indexing synchronously. Poll the pipeline's indexing sub-resource until
    // it reports not-building, then give it a short extra settling delay.
    poll_indexing(client, parent_job, poll_ms).await
}

/// Poll `GET /job/<pipeline>/indexing/api/json` until building == false.
/// Falls back gracefully if the endpoint doesn't exist (older Jenkins).
async fn poll_indexing(client: &JenkinsClient, parent_job: &str, poll_ms: u64) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct IndexingStatus {
        building: Option<bool>,
    }

    let path = format!("job/{}/indexing/api/json?tree=building", encode_job_path(parent_job));
    let effective_poll = poll_ms.max(1000); // at least 1 s between polls

    loop {
        let resp = client.get(&path).await?;

        // 404 means this Jenkins version doesn't expose the indexing sub-resource.
        // Just return — the scan was triggered, we can't track it further.
        if resp.status() == 404 {
            // Give the scan a fixed settling time before the caller proceeds.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            return Ok(());
        }

        if !resp.status().is_success() {
            // Non-fatal — proceed with the branch build anyway.
            return Ok(());
        }

        let status: IndexingStatus = match resp.json().await {
            Ok(s) => s,
            Err(_) => return Ok(()), // can't parse; proceed
        };

        match status.building {
            Some(false) | None => return Ok(()),
            Some(true) => {
                tokio::time::sleep(std::time::Duration::from_millis(effective_poll)).await;
            }
        }
    }
}

async fn upload_config(client: &JenkinsClient, job: &str, xml: &str) -> Result<()> {
    let resp = client
        .post(&format!("job/{}/config.xml", encode_job_path(job)))
        .await?
        .header("Content-Type", "application/xml")
        .body(xml.to_string())
        .send()
        .await
        .context("uploading config.xml")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Jenkins returned HTTP {status} uploading config.xml");
    }
    Ok(())
}

/// Try `/build`, then immediately `/buildWithParameters` on the same attempt if
/// that returns 400. Multibranch pipeline branches in CloudBees CI sometimes
/// only accept `buildWithParameters` even for jobs with no parameters.
/// Retries up to 5 attempts with exponential backoff between attempts.
async fn trigger_build(client: &JenkinsClient, job: &str, poll_ms: u64) -> Result<u64> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut delay_ms = 2_000u64;

    for attempt in 1..=MAX_ATTEMPTS {
        for endpoint in ["build", "buildWithParameters"] {
            let resp = client
                .post(&format!("job/{}/{endpoint}", encode_job_path(job)))
                .await?
                .form(&Vec::<(String, String)>::new())
                .send()
                .await
                .with_context(|| format!("triggering build via /{endpoint}"))?;

            let status = resp.status();

            if status == 400 {
                let body = resp.text().await.unwrap_or_default();
                let body_preview = body.lines().next().unwrap_or("(empty)");
                eprintln!(
                    "  HTTP 400 via /{endpoint} (attempt {attempt}/{MAX_ATTEMPTS}): {body_preview}"
                );
                // Try the other endpoint within this attempt before giving up.
                continue;
            }

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let body_preview = body.lines().next().unwrap_or("(empty)");
                anyhow::bail!(
                    "Jenkins returned HTTP {status} via /{endpoint}: {body_preview}"
                );
            }

            let location = resp
                .headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("no Location header in build response"))?
                .to_string();

            let queue_id = extract_queue_id(&location)?;
            return poll_queue(client, queue_id, poll_ms).await;
        }

        // Both endpoints returned 400 this attempt — wait then retry.
        if attempt < MAX_ATTEMPTS {
            eprintln!("  Both endpoints returned 400. Retrying in {delay_ms}ms…");
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            delay_ms *= 2;
        }
    }

    anyhow::bail!(
        "build trigger failed after {MAX_ATTEMPTS} attempts on both /build and \
         /buildWithParameters — check the 400 messages above for Jenkins' reason"
    )
}

async fn save_log(client: &JenkinsClient, job: &str, build: u64, path: &Path) -> Result<()> {
    let resp = client
        .get(&format!("job/{}/{build}/consoleText", encode_job_path(job)))
        .await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("consoleText endpoint returned HTTP {status}");
    }
    let text = resp.text().await.context("reading console log")?;
    std::fs::write(path, text).with_context(|| format!("writing log to '{}'", path.display()))
}

fn log_filename(dir: &Path, build_target: &str, tag: &str, value: &str, build: u64) -> PathBuf {
    let safe_target = build_target.replace('/', "__");
    let safe_value  = value.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '], "_");
    dir.join(format!("{safe_target}__{tag}__{safe_value}__#{build}.log"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<org.jenkinsci.plugins.workflow.multibranch.WorkflowMultiBranchProject>
  <sources>
    <data>
      <jenkins.branch.BranchSource>
        <source class="org.jenkinsci.plugins.github_branch_source.GitHubSCMSource">
          <repoOwner>my-org</repoOwner>
          <repository>original-repo</repository>
        </source>
      </jenkins.branch.BranchSource>
    </data>
  </sources>
</org.jenkinsci.plugins.workflow.multibranch.WorkflowMultiBranchProject>"#;

    fn crumb() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(&serde_json::json!({
            "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
        }))
    }

    // ── path syntax ───────────────────────────────────────────────────────────

    // XML that has two <name> tags — one for a param, one for a branch spec.
    const AMBIGUOUS_XML: &str = r#"<flow-definition>
      <properties>
        <hudson.model.ParametersDefinitionProperty>
          <parameterDefinitions>
            <hudson.model.StringParameterDefinition>
              <name>FOOBAR</name>
            </hudson.model.StringParameterDefinition>
          </parameterDefinitions>
        </hudson.model.ParametersDefinitionProperty>
      </properties>
      <definition>
        <scm>
          <branches>
            <hudson.plugins.git.BranchSpec>
              <name>*/main</name>
            </hudson.plugins.git.BranchSpec>
          </branches>
        </scm>
      </definition>
    </flow-definition>"#;

    #[test]
    fn single_segment_finds_first_occurrence() {
        // Without a path, DFS picks up "FOOBAR" first.
        assert_eq!(read_xml_tag(AMBIGUOUS_XML, "name").unwrap(), Some("FOOBAR".into()));
    }

    #[test]
    fn path_branches_name_finds_branch_spec() {
        assert_eq!(
            read_xml_tag(AMBIGUOUS_XML, "branches/name").unwrap(),
            Some("*/main".into())
        );
    }

    #[test]
    fn path_branchspec_name_finds_branch_spec() {
        assert_eq!(
            read_xml_tag(AMBIGUOUS_XML, "hudson.plugins.git.BranchSpec/name").unwrap(),
            Some("*/main".into())
        );
    }

    #[test]
    fn path_patch_changes_correct_name_tag() {
        let patched = patch_xml_tag(AMBIGUOUS_XML, "branches/name", "*/develop").unwrap();
        assert!(patched.contains("*/develop"));
        assert!(patched.contains("<name>FOOBAR</name>")); // param name untouched
    }

    #[test]
    fn path_error_message_includes_the_path() {
        let err = patch_xml_tag(AMBIGUOUS_XML, "missing/tag", "x").unwrap_err();
        assert!(err.to_string().contains("missing/tag"));
    }

    // ── patch_xml_tag ─────────────────────────────────────────────────────────

    #[test]
    fn patch_xml_tag_replaces_target_text() {
        let patched = patch_xml_tag(SAMPLE_XML, "repository", "new-repo").unwrap();
        assert!(patched.contains("<repository>new-repo</repository>"));
        assert!(!patched.contains("original-repo"));
    }

    #[test]
    fn patch_xml_tag_does_not_alter_other_tags() {
        let patched = patch_xml_tag(SAMPLE_XML, "repository", "new-repo").unwrap();
        assert!(patched.contains("<repoOwner>my-org</repoOwner>"));
    }

    #[test]
    fn patch_xml_tag_errors_when_tag_not_found() {
        let err = patch_xml_tag(SAMPLE_XML, "nonexistent", "value").unwrap_err();
        assert!(err.to_string().contains("<nonexistent>"));
    }

    #[test]
    fn patch_xml_tag_works_on_deeply_nested_tag() {
        let xml = r#"<root><a><b><c>old</c></b></a></root>"#;
        let patched = patch_xml_tag(xml, "c", "new").unwrap();
        assert!(patched.contains("<c>new</c>"));
    }

    #[test]
    fn patch_xml_tag_replaces_only_first_occurrence() {
        let xml = r#"<root><item>first</item><item>second</item></root>"#;
        let patched = patch_xml_tag(xml, "item", "replaced").unwrap();
        assert!(patched.contains("<item>replaced</item>"));
        assert!(patched.contains("<item>second</item>"));
    }

    // ── Full run() integration ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_patches_config_triggers_build_and_restores() {
        let server = MockServer::start().await;

        // Crumb (called multiple times)
        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // Fetch original config
        Mock::given(method("GET"))
            .and(path("/job/my-job/config.xml"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(SAMPLE_XML)
                    .append_header("Content-Type", "application/xml"),
            )
            .mount(&server)
            .await;

        // Upload patched config (once per value + once for restore = 3 total)
        Mock::given(method("POST"))
            .and(path("/job/my-job/config.xml"))
            .and(header("Content-Type", "application/xml"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        for (queue_id, build_num) in [(1u64, 10u64), (2, 11)] {
            Mock::given(method("POST"))
                .and(path("/job/my-job/build"))
                .respond_with(
                    ResponseTemplate::new(201).append_header(
                        "Location",
                        format!("{}/queue/item/{queue_id}/", server.uri()),
                    ),
                )
                .up_to_n_times(1)
                .mount(&server)
                .await;

            Mock::given(method("GET"))
                .and(path(format!("/queue/item/{queue_id}/api/json")))
                .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "executable": { "number": build_num, "url": "http://x" }
                })))
                .mount(&server)
                .await;

            Mock::given(method("GET"))
                .and(path(format!("/job/my-job/{build_num}/api/json")))
                .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                    "building": false, "result": "SUCCESS"
                })))
                .mount(&server)
                .await;

            Mock::given(method("GET"))
                .and(path(format!("/job/my-job/{build_num}/consoleText")))
                .respond_with(ResponseTemplate::new(200).set_body_string("log\n"))
                .mount(&server)
                .await;
        }

        let tmp_dir = std::env::temp_dir().join("rj_config_sweep_test");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSweepArgs {
            job: "my-job".to_string(),
            xml_tag: "repository".to_string(),
            values: vec!["repo-a".to_string(), "repo-b".to_string()],
            output_dir: tmp_dir.to_str().unwrap().to_string(),
            poll_ms: 0,
            branch: None,
            post_config_delay_ms: 0,
            skip_scan: true,
            no_restore: false,
        };

        run(&client, &args).await.unwrap();

        // Log files should exist
        assert!(tmp_dir.join("my-job__repository__repo-a__#10.log").exists());
        assert!(tmp_dir.join("my-job__repository__repo-b__#11.log").exists());

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[tokio::test]
    async fn run_uploads_correct_patched_value_for_each_iteration() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/config.xml"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(SAMPLE_XML),
            )
            .mount(&server)
            .await;

        // Verify the patched value appears in the POST body
        Mock::given(method("POST"))
            .and(path("/job/my-job/config.xml"))
            .and(body_string_contains("<repository>patched-repo</repository>"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // Restore POST (original XML)
        Mock::given(method("POST"))
            .and(path("/job/my-job/config.xml"))
            .and(body_string_contains("<repository>original-repo</repository>"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST")).and(path("/job/my-job/build"))
            .respond_with(ResponseTemplate::new(201)
                .append_header("Location", format!("{}/queue/item/1/", server.uri())))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/queue/item/1/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 5, "url": "http://x" }
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/job/my-job/5/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "SUCCESS"
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/job/my-job/5/consoleText"))
            .respond_with(ResponseTemplate::new(200).set_body_string("log\n"))
            .mount(&server).await;

        let tmp = std::env::temp_dir().join("rj_config_sweep_body_test");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSweepArgs {
            job: "my-job".to_string(),
            xml_tag: "repository".to_string(),
            values: vec!["patched-repo".to_string()],
            output_dir: tmp.to_str().unwrap().to_string(),
            poll_ms: 0,
            branch: None,
            post_config_delay_ms: 0,
            skip_scan: true,
            no_restore: false,
        };

        run(&client, &args).await.unwrap();
        std::fs::remove_dir_all(&tmp).ok();
        // wiremock asserts expect(1) on both POST mocks on drop
    }

    #[tokio::test]
    async fn branch_flag_builds_branch_job_not_parent_pipeline() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // Config is fetched/uploaded on the PARENT pipeline
        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/config.xml"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(SAMPLE_XML),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/my-pipeline/config.xml"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Build is triggered on the BRANCH, not the parent
        Mock::given(method("POST"))
            .and(path("/job/my-pipeline/job/main/build"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", format!("{}/queue/item/9/", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/queue/item/9/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 77, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        // Status and logs come from the BRANCH build
        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/job/main/77/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "SUCCESS"
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/job/main/77/consoleText"))
            .respond_with(ResponseTemplate::new(200).set_body_string("branch log\n"))
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_config_sweep_branch_test");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSweepArgs {
            job: "my-pipeline".to_string(),
            xml_tag: "repository".to_string(),
            values: vec!["repo-x".to_string()],
            output_dir: tmp.to_str().unwrap().to_string(),
            poll_ms: 0,
            branch: Some("main".to_string()),
            post_config_delay_ms: 0,
            skip_scan: true,
            no_restore: false,
        };

        run(&client, &args).await.unwrap();

        // Log file path reflects the branch job, not the parent
        assert!(tmp.join("my-pipeline__main__repository__repo-x__#77.log").exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn trigger_build_falls_back_to_build_with_parameters_on_400() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // /build always returns 400
        Mock::given(method("POST"))
            .and(path("/job/my-job/build"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        // /buildWithParameters succeeds on first try
        Mock::given(method("POST"))
            .and(path("/job/my-job/buildWithParameters"))
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

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let build_num = trigger_build(&client, "my-job", 0).await.unwrap();
        assert_eq!(build_num, 5);
    }

    #[tokio::test]
    async fn trigger_build_retries_across_attempts_when_both_endpoints_fail() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        // Both endpoints return 400 for the first attempt, then /build succeeds
        Mock::given(method("POST"))
            .and(path("/job/my-job/build"))
            .respond_with(ResponseTemplate::new(400))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/my-job/buildWithParameters"))
            .respond_with(ResponseTemplate::new(400))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/my-job/build"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", format!("{}/queue/item/2/", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/queue/item/2/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 7, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let build_num = trigger_build(&client, "my-job", 0).await.unwrap();
        assert_eq!(build_num, 7);
    }

    #[tokio::test]
    async fn scan_runs_before_branch_build_when_skip_scan_is_false() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb())
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .mount(&server)
            .await;

        // Config upload (patched) + restore
        Mock::given(method("POST"))
            .and(path("/job/my-pipeline/config.xml"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Scan triggered on parent pipeline — must happen before branch build
        Mock::given(method("POST"))
            .and(path("/job/my-pipeline/build"))
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
                "executable": { "number": 10, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        // Scan build completes
        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/10/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "SUCCESS"
            })))
            .mount(&server)
            .await;

        // Branch build triggered after scan
        Mock::given(method("POST"))
            .and(path("/job/my-pipeline/job/main/build"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("Location", format!("{}/queue/item/2/", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/queue/item/2/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "executable": { "number": 20, "url": "http://x" }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/job/main/20/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "building": false, "result": "SUCCESS"
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-pipeline/job/main/20/consoleText"))
            .respond_with(ResponseTemplate::new(200).set_body_string("log\n"))
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_config_sweep_scan_test");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSweepArgs {
            job: "my-pipeline".to_string(),
            xml_tag: "repository".to_string(),
            values: vec!["repo-y".to_string()],
            output_dir: tmp.to_str().unwrap().to_string(),
            poll_ms: 0,
            branch: Some("main".to_string()),
            post_config_delay_ms: 0,
            skip_scan: false,   // scan runs first
            no_restore: false,
        };

        run(&client, &args).await.unwrap();
        std::fs::remove_dir_all(&tmp).ok();
        // wiremock asserts expect(1) on both the scan POST and branch POST
    }
}
