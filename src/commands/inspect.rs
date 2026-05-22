use crate::cli::InspectArgs;
use crate::client::{encode_job_path, JenkinsClient};
use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;
use serde_json::Value;

// ── Jenkins API types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct JobInfo {
    pub name: String,
    pub description: Option<String>,
    pub buildable: bool,
    #[serde(rename = "lastBuild")]
    pub last_build: Option<BuildRef>,
    #[serde(default)]
    pub property: Vec<Property>,
}

#[derive(Debug, Deserialize)]
pub struct BuildRef {
    pub number: u64,
    pub result: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Property {
    #[serde(rename = "_class")]
    #[allow(dead_code)]
    pub class: String,
    #[serde(default)]
    pub parameter_definitions: Vec<ParamDef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamDef {
    #[serde(rename = "_class")]
    pub class: String,
    pub name: String,
    pub description: Option<String>,
    pub default_parameter_value: Option<DefaultValue>,
    #[serde(default)]
    pub choices: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct DefaultValue {
    pub value: Option<Value>,
}

impl ParamDef {
    /// Derive a short human-readable type from the fully-qualified Java class name.
    /// e.g. "hudson.model.StringParameterDefinition" → "String"
    pub fn kind(&self) -> &str {
        self.class
            .rsplit('.')
            .next()
            .unwrap_or(&self.class)
            .trim_end_matches("ParameterDefinition")
    }
}

// ── Command entry point ───────────────────────────────────────────────────────

// Fields we request from Jenkins. Without an explicit `tree`, the `result`
// field on `lastBuild` is omitted from the response even for finished builds.
const TREE: &str = "name,description,buildable,\
    lastBuild[number,result],\
    property[parameterDefinitions[\
        _class,name,description,\
        defaultParameterValue[value],\
        choices\
    ]]";

pub async fn run(client: &JenkinsClient, args: &InspectArgs) -> Result<()> {
    // Spaces in job names need encoding; nested jobs use job/folder/job/name.
    let path = format!("job/{}/api/json?tree={TREE}", encode_job_path(&args.job));

    let resp = client.get(&path).await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Jenkins returned HTTP {status} for job '{}'", args.job);
    }

    let info: JobInfo = resp.json().await.context("parsing job JSON")?;
    print_job(&info);
    Ok(())
}

// ── Display ───────────────────────────────────────────────────────────────────

fn format_default(p: &ParamDef) -> String {
    match &p.default_parameter_value {
        Some(dv) => match &dv.value {
            Some(Value::String(s)) => format!("\"{s}\""),
            Some(v) => v.to_string(),
            None => "(none)".to_string(),
        },
        None => "(none)".to_string(),
    }
}

fn color_result(result: &str) -> String {
    match result {
        "SUCCESS"     => result.green().to_string(),
        "FAILURE"     => result.red().to_string(),
        "UNSTABLE"    => result.yellow().to_string(),
        "ABORTED"     => result.dimmed().to_string(),
        "IN PROGRESS" => result.blue().to_string(),
        other         => other.normal().to_string(),
    }
}

fn print_job(info: &JobInfo) {
    println!("Job:        {}", info.name.cyan().bold());
    if let Some(desc) = &info.description {
        if !desc.trim().is_empty() {
            println!("Desc:       {desc}");
        }
    }
    println!("Buildable:  {}", info.buildable);
    match &info.last_build {
        Some(b) => println!(
            "Last build: {} — {}",
            format!("#{}", b.number).dimmed(),
            color_result(b.result.as_deref().unwrap_or("IN PROGRESS"))
        ),
        None => println!("Last build: {}", "(none)".dimmed()),
    }

    let params: Vec<&ParamDef> = info
        .property
        .iter()
        .flat_map(|p| &p.parameter_definitions)
        .collect();

    if params.is_empty() {
        println!("\n{}", "No parameters defined.".dimmed());
        return;
    }

    println!("\n{}:", "Parameters".bold());
    for p in &params {
        let kind = p.kind();
        let desc_suffix = match p.description.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(d) => format!("  {}", format!("— {d}").dimmed()),
            None => String::new(),
        };

        if kind == "Choice" {
            println!(
                "  {:<22} {} Choices: {}{}",
                p.name,
                format!("[{kind:<8}]").cyan(),
                p.choices.join(", ").yellow(),
                desc_suffix,
            );
        } else {
            println!(
                "  {:<22} {} Default: {:<20}{}",
                p.name,
                format!("[{kind:<8}]").cyan(),
                format_default(p).yellow(),
                desc_suffix,
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic Jenkins /api/json payload covering String, Boolean, and Choice.
    const SAMPLE: &str = r#"{
        "name": "deploy-prod",
        "description": "Deploys to the production environment",
        "buildable": true,
        "lastBuild": {
            "number": 42,
            "result": "SUCCESS",
            "url": "http://jenkins/job/deploy-prod/42/"
        },
        "property": [
            {
                "_class": "hudson.model.ParametersDefinitionProperty",
                "parameterDefinitions": [
                    {
                        "_class": "hudson.model.StringParameterDefinition",
                        "name": "ENV",
                        "description": "Target environment",
                        "defaultParameterValue": { "value": "staging" }
                    },
                    {
                        "_class": "hudson.model.BooleanParameterDefinition",
                        "name": "VERBOSE",
                        "description": "Enable verbose output",
                        "defaultParameterValue": { "value": false }
                    },
                    {
                        "_class": "hudson.model.ChoiceParameterDefinition",
                        "name": "REGION",
                        "description": "AWS region",
                        "choices": ["us-east-1", "eu-west-1", "ap-southeast-1"],
                        "defaultParameterValue": { "value": "us-east-1" }
                    }
                ]
            }
        ]
    }"#;

    fn params(info: &JobInfo) -> Vec<&ParamDef> {
        info.property.iter().flat_map(|p| &p.parameter_definitions).collect()
    }

    // ── Metadata ──────────────────────────────────────────────────────────────

    #[test]
    fn parses_job_metadata() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(info.name, "deploy-prod");
        assert_eq!(info.description.as_deref(), Some("Deploys to the production environment"));
        assert!(info.buildable);
    }

    #[test]
    fn parses_last_build_number_and_result() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        let b = info.last_build.unwrap();
        assert_eq!(b.number, 42);
        assert_eq!(b.result.as_deref(), Some("SUCCESS"));
    }

    // ── Parameter count & names ───────────────────────────────────────────────

    #[test]
    fn parses_all_three_parameter_definitions() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        let ps = params(&info);
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].name, "ENV");
        assert_eq!(ps[1].name, "VERBOSE");
        assert_eq!(ps[2].name, "REGION");
    }

    // ── kind() helper ─────────────────────────────────────────────────────────

    #[test]
    fn kind_strips_java_package_and_suffix() {
        let make = |class: &str| ParamDef {
            class: class.to_string(),
            name: String::new(),
            description: None,
            default_parameter_value: None,
            choices: vec![],
        };
        assert_eq!(make("hudson.model.StringParameterDefinition").kind(),   "String");
        assert_eq!(make("hudson.model.BooleanParameterDefinition").kind(),  "Boolean");
        assert_eq!(make("hudson.model.ChoiceParameterDefinition").kind(),   "Choice");
        assert_eq!(make("hudson.model.TextParameterDefinition").kind(),     "Text");
        assert_eq!(make("hudson.model.PasswordParameterDefinition").kind(), "Password");
    }

    // ── Default values ────────────────────────────────────────────────────────

    #[test]
    fn string_param_default_is_string_value() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        let env = params(&info)[0];
        assert_eq!(env.kind(), "String");
        let val = env.default_parameter_value.as_ref().unwrap().value.as_ref().unwrap();
        assert_eq!(val, "staging");
    }

    #[test]
    fn boolean_param_default_is_false() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        let verbose = params(&info)[1];
        assert_eq!(verbose.kind(), "Boolean");
        let val = verbose.default_parameter_value.as_ref().unwrap().value.as_ref().unwrap();
        assert_eq!(val, &Value::Bool(false));
    }

    #[test]
    fn format_default_quotes_strings_and_serialises_booleans() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        let ps = params(&info);
        assert_eq!(format_default(ps[0]), "\"staging\"");
        assert_eq!(format_default(ps[1]), "false");
    }

    // ── Choice parameter ──────────────────────────────────────────────────────

    #[test]
    fn choice_param_has_all_three_choices() {
        let info: JobInfo = serde_json::from_str(SAMPLE).unwrap();
        let region = params(&info)[2];
        assert_eq!(region.kind(), "Choice");
        assert_eq!(region.choices, vec!["us-east-1", "eu-west-1", "ap-southeast-1"]);
    }

    // ── Edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn job_with_no_parameters_and_no_last_build() {
        let payload = r#"{
            "name": "simple-job",
            "buildable": true,
            "lastBuild": null,
            "property": []
        }"#;
        let info: JobInfo = serde_json::from_str(payload).unwrap();
        assert!(params(&info).is_empty());
        assert!(info.last_build.is_none());
    }

    #[test]
    fn property_without_parameter_definitions_is_skipped() {
        // Some property types (e.g. GitHubProjectProperty) have no parameterDefinitions.
        let payload = r#"{
            "name": "git-job",
            "buildable": true,
            "lastBuild": null,
            "property": [
                { "_class": "com.coravy.hudson.plugins.github.GithubProjectProperty" }
            ]
        }"#;
        let info: JobInfo = serde_json::from_str(payload).unwrap();
        assert!(params(&info).is_empty());
    }

    // ── HTTP integration ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_calls_correct_endpoint_and_succeeds() {
        use crate::client::JenkinsClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/job/deploy-prod/api/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(SAMPLE, "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = InspectArgs { job: "deploy-prod".to_string() };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn run_requests_tree_param_so_result_field_is_returned() {
        // Regression test: without ?tree=..., Jenkins omits `result` from
        // lastBuild even on finished builds, causing completed builds to
        // display as "IN PROGRESS".
        use crate::client::JenkinsClient;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/job/deploy-prod/api/json"))
            .and(query_param("tree", TREE))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(SAMPLE, "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = InspectArgs { job: "deploy-prod".to_string() };
        run(&client, &args).await.unwrap();
    }

    #[tokio::test]
    async fn run_returns_error_on_404() {
        use crate::client::JenkinsClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/job/missing/api/json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = JenkinsClient::new(&server.uri(), "u", "p");
        let args = InspectArgs { job: "missing".to_string() };
        let err = run(&client, &args).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
