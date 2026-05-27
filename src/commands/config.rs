use crate::cli::{ConfigAction, ConfigArgs, ConfigGetArgs, ConfigSetArgs};
use crate::client::{encode_job_path, JenkinsClient};
use crate::commands::resolve_jobs;
use anyhow::{Context, Result};
use colored::Colorize;

// ── Command dispatcher ────────────────────────────────────────────────────────

pub async fn run(client: &JenkinsClient, args: &ConfigArgs) -> Result<()> {
    match &args.action {
        ConfigAction::Get(a)   => get(client, a).await,
        ConfigAction::Set(a)   => set(client, a).await,
        ConfigAction::Sweep(a) => crate::commands::config_sweep::run(client, a).await,
    }
}

// ── config get ────────────────────────────────────────────────────────────────

async fn get(client: &JenkinsClient, args: &ConfigGetArgs) -> Result<()> {
    let jobs = resolve_jobs(client, &args.target).await?;
    let multi = jobs.len() > 1;

    if let Some(dir) = &args.output_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating output directory '{dir}'"))?;
    }

    for job in &jobs {
        let resp = client
            .get(&format!("job/{}/config.xml", encode_job_path(job)))
            .await?;
        let status = resp.status();
        if !status.is_success() {
            eprintln!("{}: HTTP {status}", job.cyan());
            continue;
        }
        let xml = resp.text().await.context("reading config.xml body")?;

        match &args.output_dir {
            Some(dir) => {
                let filename = format!("{}.xml", job.replace('/', "_"));
                let dest = std::path::Path::new(dir).join(&filename);
                std::fs::write(&dest, &xml)
                    .with_context(|| format!("writing '{}'", dest.display()))?;
                println!("saved: {}", dest.display().to_string().green());
            }
            None => {
                if multi {
                    println!("{}", format!("=== {job} ===").cyan().bold());
                }
                println!("{xml}");
            }
        }
    }
    Ok(())
}

// ── config set ────────────────────────────────────────────────────────────────

async fn set(client: &JenkinsClient, args: &ConfigSetArgs) -> Result<()> {
    let xml = std::fs::read_to_string(&args.file)
        .with_context(|| format!("reading local file '{}'", args.file))?;

    let resp = client
        .post(&format!("job/{}/config.xml", encode_job_path(&args.job)))
        .await?
        .header("Content-Type", "application/xml")
        .body(xml)
        .send()
        .await
        .context("uploading config.xml")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "Jenkins returned HTTP {status} when updating config for '{}'",
            args.job
        );
    }

    println!("Configuration updated for '{}'.", args.job);
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

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
  <description>My CI job</description>
  <builders>
    <hudson.tasks.Shell>
      <command>echo hello</command>
    </hudson.tasks.Shell>
  </builders>
</project>"#;

    fn crumb_mock_template() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(&serde_json::json!({
            "crumb": "tok", "crumbRequestField": "Jenkins-Crumb"
        }))
    }

    fn job_target(job: &str) -> crate::cli::JobTarget {
        crate::cli::JobTarget {
            paths: vec![],
            job_names: vec![job.to_string()],
            recursive: false,
        }
    }

    fn folder_target(folder: &str) -> crate::cli::JobTarget {
        crate::cli::JobTarget {
            paths: vec![folder.to_string()],
            job_names: vec![],
            recursive: false,
        }
    }

    // ── config get ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_fetches_config_xml_from_correct_endpoint() {
        let server = MockServer::start().await;

        // validation call
        Mock::given(method("GET"))
            .and(path("/job/my-job/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                &serde_json::json!({ "_class": "org.jenkinsci.plugins.workflow.job.WorkflowJob" }),
            ))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/config.xml"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(SAMPLE_XML)
                    .append_header("Content-Type", "application/xml"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigGetArgs { target: job_target("my-job"), output_dir: None };
        get(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn get_saves_to_file_when_output_dir_given() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                &serde_json::json!({ "_class": "WorkflowJob" }),
            ))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/my-job/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .expect(1)
            .mount(&server)
            .await;

        let tmp_dir = std::env::temp_dir().join("rj_test_config_get_save");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigGetArgs {
            target: job_target("my-job"),
            output_dir: Some(tmp_dir.to_str().unwrap().to_string()),
        };
        get(&client, &args).await.unwrap();

        let saved = std::fs::read_to_string(tmp_dir.join("my-job.xml")).unwrap();
        assert!(saved.contains("<command>echo hello</command>"));
        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[tokio::test]
    async fn get_fetches_all_jobs_in_folder() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/my-folder/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "_class": "com.cloudbees.hudson.plugins.folder.Folder",
                "jobs": [
                    { "name": "job1", "_class": "WorkflowJob" },
                    { "name": "job2", "_class": "WorkflowJob" },
                ]
            })))
            .mount(&server)
            .await;

        for job_name in ["job1", "job2"] {
            Mock::given(method("GET"))
                .and(path(format!("/job/my-folder/job/{job_name}/config.xml")))
                .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
                .expect(1)
                .mount(&server)
                .await;
        }

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigGetArgs { target: folder_target("my-folder"), output_dir: None };
        get(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn get_saves_folder_jobs_with_underscored_filenames() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/team/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
                "_class": "com.cloudbees.hudson.plugins.folder.Folder",
                "jobs": [{ "name": "svc", "_class": "WorkflowJob" }]
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/team/job/svc/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .expect(1)
            .mount(&server)
            .await;

        let tmp_dir = std::env::temp_dir().join("rj_test_config_get_folder_save");
        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigGetArgs {
            target: folder_target("team"),
            output_dir: Some(tmp_dir.to_str().unwrap().to_string()),
        };
        get(&client, &args).await.unwrap();

        assert!(tmp_dir.join("team_svc.xml").exists());
        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    // ── config set ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_posts_to_correct_endpoint_with_xml_content_type() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb_mock_template())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/my-job/config.xml"))
            .and(header("Content-Type", "application/xml"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_test_config_type.xml");
        std::fs::write(&tmp, SAMPLE_XML).unwrap();

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSetArgs {
            job: "my-job".to_string(),
            file: tmp.to_str().unwrap().to_string(),
        };
        set(&client, &args).await.unwrap();

        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn set_sends_file_contents_verbatim_as_body() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb_mock_template())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/my-job/config.xml"))
            .and(body_string_contains("<command>echo hello</command>"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_test_config_body.xml");
        std::fs::write(&tmp, SAMPLE_XML).unwrap();

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSetArgs {
            job: "my-job".to_string(),
            file: tmp.to_str().unwrap().to_string(),
        };
        set(&client, &args).await.unwrap();

        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn set_attaches_csrf_crumb_on_post() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb_mock_template())
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/my-job/config.xml"))
            .and(header("Jenkins-Crumb", "tok"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_test_config_crumb.xml");
        std::fs::write(&tmp, SAMPLE_XML).unwrap();

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSetArgs {
            job: "my-job".to_string(),
            file: tmp.to_str().unwrap().to_string(),
        };
        set(&client, &args).await.unwrap();

        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn set_returns_error_when_file_does_not_exist() {
        let client = crate::client::JenkinsClient::new("http://127.0.0.1:1", "u", "p");
        let args = ConfigSetArgs {
            job: "my-job".to_string(),
            file: "/nonexistent/path/config.xml".to_string(),
        };
        let err = set(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("reading local file"));
    }

    #[tokio::test]
    async fn set_returns_error_on_non_2xx_response() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/crumbIssuer/api/json"))
            .respond_with(crumb_mock_template())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/job/readonly/config.xml"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("rj_test_config_403.xml");
        std::fs::write(&tmp, SAMPLE_XML).unwrap();

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigSetArgs {
            job: "readonly".to_string(),
            file: tmp.to_str().unwrap().to_string(),
        };
        let err = set(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("403"));

        std::fs::remove_file(&tmp).ok();
    }

    // ── run() dispatcher ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_dispatches_get_action() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/job/dispatch-job/api/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                &serde_json::json!({ "_class": "WorkflowJob" }),
            ))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/job/dispatch-job/config.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_XML))
            .expect(1)
            .mount(&server)
            .await;

        let client = crate::client::JenkinsClient::new(&server.uri(), "u", "p");
        let args = ConfigArgs {
            action: ConfigAction::Get(ConfigGetArgs {
                target: job_target("dispatch-job"),
                output_dir: None,
            }),
        };
        run(&client, &args).await.unwrap();
    }
}
