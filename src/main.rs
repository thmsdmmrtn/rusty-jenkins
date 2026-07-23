use anyhow::{Context, Result};
use clap::Parser;

mod browser;
mod cli;
mod client;
mod commands;
mod manifest;

use cli::{Cli, Command, TagAction};
use clap::CommandFactory;
use client::JenkinsClient;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // {e:#} prints the full anyhow error chain, one cause per line.
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Resolve the profile manifest: an explicit --manifest path wins, otherwise
    // search upward from the current directory for an rj.toml.
    let manifest_path = match &cli.manifest {
        Some(p) => Some(std::path::PathBuf::from(p)),
        None => std::env::current_dir().ok().and_then(|d| manifest::discover(&d)),
    };

    // Load it if present. A failure to load an *explicitly requested* manifest is
    // fatal; an auto-discovered one that won't parse is downgraded to a warning so
    // a stray rj.toml elsewhere on disk never breaks unrelated commands.
    let manifest = match &manifest_path {
        Some(p) => match manifest::load(p) {
            Ok(m) => Some(m),
            Err(e) if cli.manifest.is_some() => return Err(e),
            Err(e) => {
                eprintln!("warning: ignoring manifest {}: {e:#}", p.display());
                None
            }
        },
        None => None,
    };

    // URL precedence: --url / JENKINS_URL first, then the manifest's [defaults].url.
    let url = cli
        .url
        .clone()
        .or_else(|| manifest.as_ref().and_then(|m| m.defaults.url.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Jenkins URL is required. Set JENKINS_URL, pass --url <URL>, or add \
                 [defaults] url = \"…\" to your rj.toml."
            )
        })?;
    let url = url.as_str();

    // Diagnostic: list cookie names found in the browser, then exit.
    if cli.list_cookies {
        let browser = if cli.from_chrome { "chrome" } else { "firefox" };
        return browser::list_cookie_names(url, browser, &cli.chrome_profile);
    }

    // Resolve authentication: explicit cookie > --from-chrome > --from-firefox > Basic Auth
    let client = if let Some(cookie) = &cli.cookie {
        JenkinsClient::new_with_cookie(url, cookie)
    } else if cli.from_chrome {
        let cookie = browser::chrome_cookies(url, &cli.chrome_profile)
            .context("reading session cookies from Chrome")?;
        eprintln!("Using Chrome session cookies for authentication.");
        JenkinsClient::new_with_cookie(url, cookie)
    } else if cli.from_firefox {
        let cookie = browser::firefox_cookies(url)
            .context("reading session cookies from Firefox")?;
        eprintln!("Using Firefox session cookies for authentication.");
        JenkinsClient::new_with_cookie(url, cookie)
    } else {
        JenkinsClient::new(url, &cli.user, &cli.token)
    };

    match &cli.command {
        Some(Command::Inspect(args)) => commands::inspect::run(&client, args).await,
        Some(Command::Build(args))   => commands::build::run(&client, args).await,
        Some(Command::Logs(args))    => commands::logs::run(&client, args).await,
        Some(Command::Config(args))  => commands::config::run(&client, args).await,
        Some(Command::Sweep(args))   => commands::sweep::run(&client, args).await,
        Some(Command::List(args))    => commands::list::run(&client, args).await,
        Some(Command::Tag(tag))      => match &tag.action {
            TagAction::List(args)  => commands::list_tag::run(&client, args).await,
            TagAction::Patch(args) => commands::patch_tag::run(&client, args).await,
        },
        Some(Command::Run(args)) => {
            let m = require_manifest(manifest.as_ref(), manifest_path.as_deref())?;
            commands::run::run(&client, m, args).await
        }
        Some(Command::Profiles(args)) => {
            let m = require_manifest(manifest.as_ref(), manifest_path.as_deref())?;
            commands::profiles::run(m, args)
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// Commands that consume the manifest need one to exist. Turn the "no manifest
/// found" case into a clear, actionable error instead of a generic panic.
fn require_manifest<'a>(
    manifest: Option<&'a manifest::Manifest>,
    path: Option<&std::path::Path>,
) -> Result<&'a manifest::Manifest> {
    manifest.ok_or_else(|| match path {
        Some(p) => anyhow::anyhow!("manifest '{}' could not be loaded", p.display()),
        None => anyhow::anyhow!(
            "no rj.toml manifest found — create one in this directory (or an ancestor), \
             or pass --manifest <path>. See the example in examples/rj.toml."
        ),
    })
}
