use anyhow::{Context, Result};
use clap::Parser;

mod browser;
mod cli;
mod client;
mod commands;

use cli::{Cli, Command};
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

    // Resolve authentication: explicit cookie > --from-chrome > --from-firefox > Basic Auth
    let client = if let Some(cookie) = &cli.cookie {
        JenkinsClient::new_with_cookie(&cli.url, cookie)
    } else if cli.from_chrome {
        let cookie = browser::chrome_cookies(&cli.url)
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
        Command::Inspect(args) => commands::inspect::run(&client, args).await,
        Command::Build(args)   => commands::build::run(&client, args).await,
        Command::Logs(args)    => commands::logs::run(&client, args).await,
        Command::Config(args)  => commands::config::run(&client, args).await,
        Command::Sweep(args)   => commands::sweep::run(&client, args).await,
        Command::List(args)    => commands::list::run(&client, args).await,
    }
}
