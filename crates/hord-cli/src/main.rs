//! Hord command-line interface (`hord`).
//!
//! M0 commands: `init`, `ws new`, `status`, `log`, `git import`, `git export`.
//! `--json` is the canonical agent output; human-oriented text is secondary.

#![forbid(unsafe_code)]

mod cli;
mod cmd;
mod git_bridge;
mod output;
mod repo;

use anyhow::{Context, Result};
use clap::Parser;

use cli::{Cli, Command, GitCommand, WsCommand};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(err) = run(cli).await {
        output::fail(json, &err);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    tokio::task::spawn_blocking(move || run_blocking(cli))
        .await
        .context("command task panicked")?
}

fn run_blocking(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init { from_git } => cmd::init::run(cli.json, from_git),
        Command::Ws { command } => match command {
            WsCommand::New { base } => cmd::ws::run_new(cli.json, base),
        },
        Command::Status { workspace } => cmd::status::run(cli.json, workspace),
        Command::Log => cmd::log::run(cli.json),
        Command::Git { command } => match command {
            GitCommand::Import { git_ref } => cmd::git::run_import(cli.json, git_ref),
            GitCommand::Export { hord_ref } => cmd::git::run_export(cli.json, hord_ref),
        },
    }
}
