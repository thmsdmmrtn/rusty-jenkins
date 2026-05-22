use anyhow::{Context, Result};
use clap::Parser;

mod browser;
mod cli;
mod client;
mod commands;

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

    if cli.url.is_empty() {
        anyhow::bail!(
            "Jenkins URL is required. Set the JENKINS_URL environment variable or pass --url <URL>."
        );
    }

    // Diagnostic: list cookie names found in the browser, then exit.
    if cli.list_cookies {
        let browser = if cli.from_chrome { "chrome" } else { "firefox" };
        return browser::list_cookie_names(&cli.url, browser, &cli.chrome_profile);
    }

    // Resolve authentication: explicit cookie > --from-chrome > --from-firefox > Basic Auth
    let client = if let Some(cookie) = &cli.cookie {
        JenkinsClient::new_with_cookie(&cli.url, cookie)
    } else if cli.from_chrome {
        let cookie = browser::chrome_cookies(&cli.url, &cli.chrome_profile)
            .context("reading session cookies from Chrome")?;
        eprintln!("Using Chrome session cookies for authentication.");
        JenkinsClient::new_with_cookie(&cli.url, cookie)
    } else if cli.from_firefox {
        let cookie = browser::firefox_cookies(&cli.url)
            .context("reading session cookies from Firefox")?;
        eprintln!("Using Firefox session cookies for authentication.");
        JenkinsClient::new_with_cookie(&cli.url, cookie)
    } else {
        JenkinsClient::new(&cli.url, &cli.user, &cli.token)
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
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}
