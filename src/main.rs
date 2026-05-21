use anyhow::Result;
use clap::Parser;

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
    let client = JenkinsClient::new(&cli.url, &cli.user, &cli.token);

    match &cli.command {
        Command::Inspect(args) => commands::inspect::run(&client, args).await,
        Command::Build(args)   => commands::build::run(&client, args).await,
        Command::Logs(args)    => commands::logs::run(&client, args).await,
        Command::Config(args)  => commands::config::run(&client, args).await,
    }
}
