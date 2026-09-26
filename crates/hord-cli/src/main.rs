//! Hord command-line interface (`hord`).
//!
//! Commands: `init`, `ws new|list|rm|gc`, `status`, `verify`, `propose`, `submit`,
//! `queue`, `land --local`, `conflicts`, `log`, `blame`, `query`, `watch`,
//! `remote add|rm|list|set-default`, `serve`, `git import`, `git export`,
//! `policy check`, `replay`, `arbitrate`, `review`, `login`, `token mint`,
//! `user add`, `key show|verify`.
//!
//! `--json` is the canonical agent output: the protobuf JSON mapping of the
//! command's `hord.proto` message (ADR 0024). Human-oriented text is
//! secondary.
//!
//! Commands reach the repository through its per-repo daemon (started on
//! demand), `--no-daemon`, or `--remote` / the default upstream
//! ([`session`]).

#![deny(unsafe_code)]

mod cli;
mod cmd;
mod daemon;
mod git_bridge;
mod identity;
mod intent;
mod output;
mod remotes;
mod repo;
mod resolve;
mod session;
mod txn;
mod workspaces;

use anyhow::{Context, Result};
use clap::Parser;
use hord_server::TlsConfig;

use cli::{
    Cli, Command, GitCommand, KeyCommand, PolicyCommand, RemoteCommand, TokenCommand, UserCommand,
    WsCommand,
};
use session::Target;

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
    if let Command::Serve {
        repo,
        root,
        bind,
        insecure_bind,
        tls_cert,
        tls_key,
        auth,
        config,
        daemon,
    } = cli.command
    {
        let tls = tls_cert
            .zip(tls_key)
            .map(|(cert, key)| TlsConfig { cert, key });
        return cmd::serve::run(repo, root, bind, insecure_bind, tls, auth, config, daemon).await;
    }
    tokio::task::spawn_blocking(move || run_blocking(cli))
        .await
        .context("command task panicked")?
}

/// Commands that open the store in this process: a running daemon holds
/// it, so it is asked to stop first.
fn needs_store(command: &Command) -> bool {
    matches!(command, Command::Git { .. })
}

fn run_blocking(cli: Cli) -> Result<()> {
    let target = Target::new(cli.remote.clone(), cli.no_daemon);
    if needs_store(&cli.command)
        && let Ok(root) = repo::discover_root()
    {
        daemon::stop(&root)?;
    }
    let json = cli.json;
    match cli.command {
        Command::Init { from_git } => cmd::init::run(json, from_git),
        Command::Ws { command } => match command {
            WsCommand::New { base, materialize } => {
                cmd::ws::run_new(json, &target, base, materialize)
            }
            WsCommand::List => cmd::ws::run_list(json, &target),
            WsCommand::Rm { id } => cmd::ws::run_rm(json, &target, id),
            WsCommand::Gc => cmd::ws::run_gc(json, &target),
        },
        Command::Status {
            workspace,
            paranoid,
        } => cmd::status::run(json, &target, workspace, paranoid),
        Command::Verify {
            workspace,
            plan_only,
        } => cmd::verify::run(json, &target, workspace, plan_only),
        Command::Propose { workspace, intent } => {
            cmd::propose::run(json, &target, workspace, intent)
        }
        Command::Submit { change } => cmd::lander::run_submit(json, &target, change),
        Command::Queue { mine } => cmd::lander::run_queue(json, &target, mine),
        Command::Land { local, change } => cmd::lander::run_land(json, &target, local, change),
        Command::Conflicts { change } => cmd::lander::run_conflicts(json, &target, change),
        Command::Log {
            node,
            path,
            actor,
            since,
        } => cmd::log::run(json, &target, node, path, actor, since),
        Command::Blame { target: spec } => cmd::blame::run(json, &target, spec),
        Command::Query { edge, node } => cmd::query::run(json, &target, edge, node),
        Command::Watch {
            queue,
            change,
            from,
        } => cmd::watch::run(json, &target, queue, change, from),
        Command::Remote { command } => match command {
            RemoteCommand::Add { name, url, ca_file } => {
                cmd::remote::run_add(json, name, url, ca_file)
            }
            RemoteCommand::Rm { name } => cmd::remote::run_rm(json, name),
            RemoteCommand::List => cmd::remote::run_list(json),
            RemoteCommand::SetDefault { name, clear } => {
                cmd::remote::run_set_default(json, name, clear)
            }
        },
        Command::Serve { .. } => unreachable!("handled before the blocking pool"),
        Command::Git { command } => match command {
            GitCommand::Import { git_ref } => cmd::git::run_import(json, git_ref),
            GitCommand::Export { hord_ref } => cmd::git::run_export(json, hord_ref),
        },
        Command::Review {
            change,
            kind,
            approve,
            reject: _,
            message,
        } => cmd::review::run(
            json,
            &target,
            cmd::review::Review {
                change,
                kind,
                approve,
                message,
            },
        ),
        Command::Login {
            remote,
            user,
            password_stdin,
            token,
            key_file,
        } => {
            let method = match (token, key_file) {
                (Some(token), Some(key_file)) => cmd::login::Method::Token { token, key_file },
                _ => cmd::login::Method::Password {
                    user,
                    stdin: password_stdin,
                },
            };
            cmd::login::run(json, &target, remote, method)
        }
        Command::Token { command } => match command {
            TokenCommand::Mint {
                agent,
                model,
                harness,
                scopes,
                key_out,
            } => cmd::token::run_mint(
                json,
                &target,
                cmd::token::Mint {
                    agent,
                    model,
                    harness,
                    scopes,
                    key_out,
                },
            ),
        },
        Command::User { command } => match command {
            UserCommand::Add {
                name,
                auth_file,
                scopes,
                password_stdin,
            } => cmd::user::run_add(json, name, auth_file, scopes, password_stdin),
        },
        Command::Key { command } => match command {
            KeyCommand::Show { name } => cmd::key::run_show(json, name),
            KeyCommand::Verify { object, key } => cmd::key::run_verify(json, &target, object, key),
        },
        Command::Policy { command } => match command {
            PolicyCommand::Check { workspace, policy } => {
                cmd::policy::run_check(json, &target, workspace, policy)
            }
        },
        Command::Replay {
            change,
            harness,
            wall_time_secs,
            tokens,
            cost_usd,
            note,
        } => cmd::replay::run(
            json,
            &target,
            change,
            harness,
            cmd::replay::Limits {
                wall_time_secs,
                tokens,
                cost_usd,
            },
            note,
        ),
        Command::Arbitrate {
            change,
            pick,
            edit,
            replay,
            note,
        } => cmd::arbitrate::run(json, &target, change, pick, edit, replay, note),
    }
}
