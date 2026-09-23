//! Hord command-line interface (`hord`).
//!
//! Commands: `init`, `ws new`, `status`, `propose`, `submit`, `queue`,
//! `land --local`, `conflicts`, `log`, `blame`, `query`, `git import`,
//! `git export`.
//! `--json` is the canonical agent output; human-oriented text is secondary.

#![forbid(unsafe_code)]

mod cli;
mod cmd;
mod git_bridge;
mod intent;
mod output;
mod repo;
mod resolve;
mod txn;

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
            WsCommand::New { base, materialize } => cmd::ws::run_new(cli.json, base, materialize),
            WsCommand::Rm { id } => cmd::ws::run_rm(cli.json, id),
            WsCommand::Gc => cmd::ws::run_gc(cli.json),
        },
        Command::Status {
            workspace,
            paranoid,
        } => cmd::status::run(cli.json, workspace, paranoid),
        Command::Propose { workspace, intent } => cmd::propose::run(cli.json, workspace, intent),
        Command::Submit { change } => cmd::lander::run_submit(cli.json, change),
        Command::Queue { mine } => cmd::lander::run_queue(cli.json, mine),
        Command::Land { local, change } => cmd::lander::run_land(cli.json, local, change),
        Command::Conflicts { change } => cmd::lander::run_conflicts(cli.json, change),
        Command::Log {
            node,
            path,
            actor,
            since,
        } => cmd::log::run(cli.json, node, path, actor, since),
        Command::Blame { target } => cmd::blame::run(cli.json, target),
        Command::Query { edge, node } => cmd::query::run(cli.json, edge, node),
        Command::Git { command } => match command {
            GitCommand::Import { git_ref } => cmd::git::run_import(cli.json, git_ref),
            GitCommand::Export { hord_ref } => cmd::git::run_export(cli.json, hord_ref),
        },
    }
}
