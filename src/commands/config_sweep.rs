use crate::cli::ConfigSweepArgs;
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::sweep::{extract_queue_id, poll_queue, wait_for_completion};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use xmltree::{Element, XMLNode};

// ── XML patching ──────────────────────────────────────────────────────────────

/// Find the first element with `tag_name` anywhere in the tree and replace its
/// text content with `new_value`. Returns `true` if the tag was found.
pub fn patch_xml_tag(xml: &str, tag_name: &str, new_value: &str) -> Result<String> {
    let mut root =
        Element::parse(xml.as_bytes()).context("parsing config.xml")?;

    if !replace_first(&mut root, tag_name, new_value) {
        anyhow::bail!(
            "XML tag <{tag_name}> not found in config.xml.\n\
             Tip: use `rj config get <job>` to inspect the XML and find the correct tag name."
        );
    }

    let mut buf = Vec::new();
    root.write(&mut buf).context("serialising modified config.xml")?;
    String::from_utf8(buf).context("config.xml is not valid UTF-8 after patching")
}

/// Depth-first search: set the text of the first element named `tag`.
fn replace_first(el: &mut Element, tag: &str, value: &str) -> bool {
    if el.name == tag {
        el.children = vec![XMLNode::Text(value.to_string())];
        return true;
    }
    for child in &mut el.children {
        if let XMLNode::Element(child_el) = child {
            if replace_first(child_el, tag, value) {
                return true;
            }
        }
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

        // 3. Trigger a build on the target (branch job or parent pipeline)
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

async fn trigger_build(client: &JenkinsClient, job: &str, poll_ms: u64) -> Result<u64> {
    let resp = client
        .post(&format!("job/{}/build", encode_job_path(job)))
        .await?
        .form(&Vec::<(String, String)>::new())
        .send()
        .await
        .context("triggering build")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Jenkins returned HTTP {status} triggering build");
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
            no_restore: false,
        };

        run(&client, &args).await.unwrap();

        // Log file path reflects the branch job, not the parent
        assert!(tmp.join("my-pipeline__main__repository__repo-x__#77.log").exists());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
