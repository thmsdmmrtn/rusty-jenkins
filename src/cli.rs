use clap::{Args, Parser, Subcommand};

/// rusty-jenkins (rj) — A Jenkins REST API CLI
#[derive(Debug, Parser)]
#[command(name = "rj", version, about, long_about = None)]
pub struct Cli {
    /// Jenkins base URL (e.g. http://192.168.2.58:8080)
    #[arg(long, global = true, env = "JENKINS_URL", default_value = "http://192.168.2.58:8080")]
    pub url: String,

    /// Jenkins username
    #[arg(long, global = true, env = "JENKINS_USER", default_value = "admin")]
    pub user: String,

    /// Jenkins API token or password
    #[arg(long, global = true, env = "JENKINS_TOKEN", default_value = "XXXXX")]
    pub token: String,

    /// Read the session cookie from your default Firefox profile.
    /// Use when Jenkins is behind SSO (e.g. Okta) — log in via Firefox first.
    #[arg(long, global = true, default_value_t = false)]
    pub from_firefox: bool,

    /// Read the session cookie from your default Chrome profile.
    /// Use when Jenkins is behind SSO (e.g. Okta) — log in via Chrome first.
    /// On Windows, Chrome cookie values are decrypted automatically via DPAPI.
    #[arg(long, global = true, default_value_t = false)]
    pub from_chrome: bool,

    /// Chrome profile directory name to read cookies from (default: "Default").
    /// If you use a non-default Chrome profile (e.g. a work profile) set this
    /// to the profile folder name shown in chrome://version (e.g. "Profile 1").
    #[arg(long, global = true, default_value = "Default")]
    pub chrome_profile: String,

    /// Explicit session cookie string (e.g. "JSESSIONID.abc=value123").
    /// Must be in name=value format. Overrides --from-firefox and --from-chrome.
    /// Can also be set via the JENKINS_COOKIE env var.
    #[arg(long, global = true, env = "JENKINS_COOKIE")]
    pub cookie: Option<String>,

    /// Print cookie names found in the browser for the Jenkins hostname, then exit.
    /// Use this to diagnose --from-chrome or --from-firefox authentication issues.
    /// Cookie values are NOT printed.
    #[arg(long, global = true, default_value_t = false)]
    pub list_cookies: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect a job's parameters and recent build status
    Inspect(InspectArgs),

    /// Trigger a job build (optionally with parameters)
    Build(BuildArgs),

    /// Stream live console log for a job's most-recent (or specific) build
    Logs(LogsArgs),

    /// Get or set a job's XML configuration
    Config(ConfigArgs),

    /// Run a job repeatedly, varying one parameter each time, and save each build's log
    Sweep(SweepArgs),

    /// List the jobs and sub-folders inside a folder (or the root)
    List(ListArgs),

    /// Patch an XML tag in a job's config, trigger a build for each value,
    /// wait for completion, save the log, then restore the original config
    ConfigSweep(ConfigSweepArgs),
}

// ── inspect ──────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Jenkins job name (URL-encoded if it contains spaces)
    pub job: String,
}

// ── build ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Jenkins job name
    pub job: String,

    /// Key=value build parameters (repeatable: -p KEY=VALUE -p OTHER=VALUE)
    #[arg(short = 'p', long = "param", value_name = "KEY=VALUE")]
    pub params: Vec<String>,
}

// ── logs ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Jenkins job name
    pub job: String,

    /// Build number to fetch logs for (defaults to the latest build)
    #[arg(short, long)]
    pub build: Option<u64>,

    /// Polling interval in milliseconds when streaming live logs
    #[arg(long, default_value_t = 1000)]
    pub poll_ms: u64,
}

// ── config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Download and print the job's config.xml
    Get(ConfigGetArgs),

    /// Upload a local config.xml file to replace the job's configuration
    Set(ConfigSetArgs),
}

#[derive(Debug, Args)]
pub struct ConfigGetArgs {
    /// Jenkins job name
    pub job: String,
}

#[derive(Debug, Args)]
pub struct ConfigSetArgs {
    /// Jenkins job name
    pub job: String,

    /// Path to the local config.xml file to upload
    pub file: String,
}

// ── sweep ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct SweepArgs {
    /// Jenkins job name
    pub job: String,

    /// Name of the parameter to vary across builds
    #[arg(long)]
    pub param_name: String,

    /// Values to iterate through — one build is triggered per value.
    /// Accepts a space-separated list after a single flag, so a shell array
    /// expands naturally: --value "${foo[@]}"
    #[arg(long = "value", short = 'v', num_args = 1..)]
    pub values: Vec<String>,

    /// Additional fixed parameters applied to every build (-p KEY=VALUE)
    #[arg(short = 'p', long = "param", value_name = "KEY=VALUE")]
    pub params: Vec<String>,

    /// Directory where per-build log files are written (created if absent)
    #[arg(long, default_value = "sweep-logs")]
    pub output_dir: String,

    /// Polling interval in milliseconds (queue wait and build-complete wait)
    #[arg(long, default_value_t = 2000)]
    pub poll_ms: u64,
}

// ── list ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Folder path to list (slash-separated, e.g. "folder/subfolder").
    /// Omit to list the Jenkins root.
    pub path: Option<String>,
}

// ── config-sweep ─────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct ConfigSweepArgs {
    /// Jenkins job name
    pub job: String,

    /// XML tag name whose text content should be changed for each run.
    /// e.g. "repository" for Branch Sources → Repository Name
    #[arg(long)]
    pub xml_tag: String,

    /// Values to iterate through — one build is triggered per value
    #[arg(long = "value", short = 'v', num_args = 1..)]
    pub values: Vec<String>,

    /// Directory where per-build log files are written (created if absent)
    #[arg(long, default_value = "config-sweep-logs")]
    pub output_dir: String,

    /// Polling interval in milliseconds (queue wait and build-complete wait)
    #[arg(long, default_value_t = 2000)]
    pub poll_ms: u64,

    /// Build a specific branch of the pipeline instead of triggering a scan.
    /// Use this with multibranch pipelines to avoid kicking off every branch.
    /// e.g. --branch main
    #[arg(long)]
    pub branch: Option<String>,

    /// Skip restoring the original config.xml after the sweep completes
    #[arg(long, default_value_t = false)]
    pub no_restore: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        // prepend the binary name that clap expects as argv[0]
        let mut full: Vec<&str> = vec!["rj"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    // ── global flag defaults ─────────────────────────────────────────────────

    #[test]
    fn defaults_are_applied_when_flags_are_omitted() {
        // Clear any ambient env vars so we're testing compile-time defaults, not env overrides.
        let saved: Vec<(&str, Option<String>)> = ["JENKINS_URL", "JENKINS_USER", "JENKINS_TOKEN"]
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            // SAFETY: single-threaded test; no other thread reads these vars concurrently.
            unsafe { std::env::remove_var(k) };
        }

        let cli = parse(&["inspect", "my-job"]);
        assert_eq!(cli.url, "http://192.168.2.58:8080");
        assert_eq!(cli.user, "admin");
        assert_eq!(cli.token, "XXXXX");

        for (k, v) in saved {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    #[test]
    fn global_flags_override_defaults() {
        let cli = parse(&[
            "--url", "http://jenkins.local:9090",
            "--user", "bob",
            "--token", "secret",
            "inspect", "my-job",
        ]);
        assert_eq!(cli.url, "http://jenkins.local:9090");
        assert_eq!(cli.user, "bob");
        assert_eq!(cli.token, "secret");
    }

    // ── inspect ──────────────────────────────────────────────────────────────

    #[test]
    fn inspect_parses_job_name() {
        let cli = parse(&["inspect", "deploy-prod"]);
        match cli.command {
            Some(Command::Inspect(args)) => assert_eq!(args.job, "deploy-prod"),
            _ => panic!("expected Inspect variant"),
        }
    }

    // ── build ─────────────────────────────────────────────────────────────────

    #[test]
    fn build_parses_job_with_no_params() {
        let cli = parse(&["build", "nightly-tests"]);
        match cli.command {
            Some(Command::Build(args)) => {
                assert_eq!(args.job, "nightly-tests");
                assert!(args.params.is_empty());
            }
            _ => panic!("expected Build variant"),
        }
    }

    #[test]
    fn build_parses_multiple_params() {
        let cli = parse(&["build", "deploy", "-p", "ENV=staging", "-p", "VERSION=1.2.3"]);
        match cli.command {
            Some(Command::Build(args)) => {
                assert_eq!(args.job, "deploy");
                assert_eq!(args.params, vec!["ENV=staging", "VERSION=1.2.3"]);
            }
            _ => panic!("expected Build variant"),
        }
    }

    // ── logs ──────────────────────────────────────────────────────────────────

    #[test]
    fn logs_defaults_to_latest_build() {
        let cli = parse(&["logs", "my-job"]);
        match cli.command {
            Some(Command::Logs(args)) => {
                assert_eq!(args.job, "my-job");
                assert_eq!(args.build, None);
                assert_eq!(args.poll_ms, 1000);
            }
            _ => panic!("expected Logs variant"),
        }
    }

    #[test]
    fn logs_accepts_explicit_build_number_and_poll_interval() {
        let cli = parse(&["logs", "my-job", "--build", "42", "--poll-ms", "500"]);
        match cli.command {
            Some(Command::Logs(args)) => {
                assert_eq!(args.build, Some(42));
                assert_eq!(args.poll_ms, 500);
            }
            _ => panic!("expected Logs variant"),
        }
    }

    // ── config ────────────────────────────────────────────────────────────────

    #[test]
    fn config_get_parses_job_name() {
        let cli = parse(&["config", "get", "my-job"]);
        match cli.command {
            Some(Command::Config(cfg)) => match cfg.action {
                ConfigAction::Get(args) => assert_eq!(args.job, "my-job"),
                _ => panic!("expected Get variant"),
            },
            _ => panic!("expected Config variant"),
        }
    }

    #[test]
    fn config_set_parses_job_and_file() {
        let cli = parse(&["config", "set", "my-job", "/tmp/config.xml"]);
        match cli.command {
            Some(Command::Config(cfg)) => match cfg.action {
                ConfigAction::Set(args) => {
                    assert_eq!(args.job, "my-job");
                    assert_eq!(args.file, "/tmp/config.xml");
                }
                _ => panic!("expected Set variant"),
            },
            _ => panic!("expected Config variant"),
        }
    }

    // ── sweep ─────────────────────────────────────────────────────────────────

    #[test]
    fn sweep_accepts_repeated_value_flags() {
        // Traditional: one --value flag per item
        let cli = parse(&["sweep", "my-job", "--param-name", "ENV",
                          "--value", "staging", "--value", "prod"]);
        match cli.command {
            Some(Command::Sweep(args)) => assert_eq!(args.values, vec!["staging", "prod"]),
            _ => panic!("expected Sweep variant"),
        }
    }

    #[test]
    fn sweep_accepts_multiple_values_after_single_flag() {
        // Shell array style: --value "${foo[@]}" expands to --value bar baz bam
        let cli = parse(&["sweep", "my-job", "--param-name", "ENV",
                          "--value", "bar", "baz", "bam",
                          "-p", "VERSION=1.0"]);
        match cli.command {
            Some(Command::Sweep(args)) => {
                assert_eq!(args.values, vec!["bar", "baz", "bam"]);
                assert_eq!(args.params, vec!["VERSION=1.0"]);
            }
            _ => panic!("expected Sweep variant"),
        }
    }

    #[test]
    fn sweep_defaults() {
        let cli = parse(&["sweep", "my-job", "--param-name", "ENV", "--value", "x"]);
        match cli.command {
            Some(Command::Sweep(args)) => {
                assert_eq!(args.output_dir, "sweep-logs");
                assert_eq!(args.poll_ms, 2000);
                assert!(args.params.is_empty());
            }
            _ => panic!("expected Sweep variant"),
        }
    }
}
