//! `hord serve [--repo <path>|--root <dir>] [--bind <addr>]` (spec
//! §10.5.1, ADR 0024): gRPC and gRPC-Web on one port, one lander per
//! repository. TLS from `--tls-cert`/`--tls-key` or `server.toml`'s `[tls]`
//! (ADR 0032); without it, loopback only unless `--insecure-bind`.
//! `--auth <file>` (or
//! `server.toml`'s `[auth] file`) requires bearer tokens (spec §10.5.4). `--daemon` runs the
//! repository's per-repo daemon instead ([`crate::daemon`]).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use hord_api::WorkspacesBackend;
use hord_server::{AuthStore, Hosts, ServeOptions, Server, ServerConfig, TlsConfig, UiSigner};

use crate::workspaces::LocalWorkspaces;
use crate::{identity, repo, txn};

/// The address when neither `--bind` nor `server.toml` gives one.
const DEFAULT_BIND: &str = "127.0.0.1:7878";

#[allow(clippy::too_many_arguments)]
pub async fn run(
    repo_path: Option<PathBuf>,
    root: Option<PathBuf>,
    bind: Option<String>,
    insecure_bind: bool,
    tls: Option<TlsConfig>,
    auth: Option<PathBuf>,
    config: Option<PathBuf>,
    daemon: bool,
) -> Result<()> {
    let repo_root = match (&repo_path, &root) {
        (Some(path), _) => Some(path.clone()),
        (None, None) => Some(repo::discover_root()?),
        (None, Some(_)) => None,
    };
    if daemon {
        let root = repo_root.ok_or_else(|| anyhow!("--daemon serves one repository (--repo)"))?;
        return crate::daemon::serve(&root).await;
    }
    let config_path = match (&config, &repo_root, &root) {
        (Some(path), _, _) => path.clone(),
        (None, Some(repo), _) => repo.join(hord_store::HORD_DIR).join("server.toml"),
        (None, None, Some(dir)) => dir.join("server.toml"),
        (None, None, None) => unreachable!("one of --repo or --root"),
    };
    let config = ServerConfig::load(&config_path)?;
    let bind = bind
        .or_else(|| config.bind.clone())
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address {bind:?}"))?;
    let tls = tls.or_else(|| config.tls.clone());
    let options = ServeOptions {
        insecure_bind,
        tls: tls.is_some(),
    };
    // Fail on a bad address before opening anything.
    hord_server::check_bind(addr, &options)?;
    let hosts = match (&repo_root, &root) {
        (Some(path), _) => {
            Hosts::open_repo(path, crate::cmd::replay::lander_options(path)?).await?
        }
        // Each hosted repository's lander runs its own replay harness.
        (None, Some(dir)) => {
            Hosts::open_root(dir, |path| {
                crate::cmd::replay::lander_options(path).map_err(|err| format!("{err:#}"))
            })
            .await?
        }
        (None, None) => unreachable!("one of --repo or --root"),
    };
    let names: Vec<String> = hosts.names().map(str::to_owned).collect();
    let listener = Server::bind(addr, &options).await?;
    let local = listener.local_addr()?;
    let auth = auth.or_else(|| config.auth.as_ref().map(|a| a.file.clone()));
    let (stop_local, local_endpoints) = serve_local_endpoints(&hosts);
    let mut server = Server::new(hosts, config);
    let mut note = String::new();
    if let Some(tls) = &tls {
        server = server
            .with_tls(tls)
            .with_context(|| format!("TLS from {}", tls.cert.display()))?;
        note.push_str(&format!(", TLS from {}", tls.cert.display()));
    }
    if let Some(path) = auth {
        let store =
            AuthStore::open(&path).with_context(|| format!("open auth file {}", path.display()))?;
        note = format!(", tokens from {}", path.display());
        server = server.with_auth(store);
    }
    // Reviews signed in the web UI use this user's existing key (ADR 0030);
    // without one, the UI says to use `hord review`.
    if let Some(signer) = ui_signer() {
        note.push_str(&format!(", UI reviews signed as {}", signer.actor.id()));
        server = server.with_ui_signer(signer);
    }
    // The first line: tests read the address from it.
    let scheme = if tls.is_some() { "https" } else { "http" };
    eprintln!(
        "hord serve: {scheme}://{local} ({}{note})",
        names.join(", ")
    );
    server
        .serve(listener, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    let _ = stop_local.send(true);
    for task in local_endpoints {
        let _ = task.await;
    }
    Ok(())
}

/// Serve each hosted repository on its local endpoint too, with its
/// workspace commands, as its daemon would (ADR 0021): this process holds
/// the store, so `hord` commands in the repository, including a replay
/// harness's `hord propose`, reach it there. They share the lander. The
/// endpoint takes no tokens, like the daemon's; it is the local user's.
fn serve_local_endpoints(
    hosts: &Hosts,
) -> (
    tokio::sync::watch::Sender<bool>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut tasks = Vec::new();
    for (name, local) in hosts.locals() {
        let root = local.repo().store().repo_root().to_path_buf();
        let endpoint = match hord_api::local::endpoint(&root) {
            Ok(endpoint) => endpoint,
            Err(err) => {
                eprintln!("hord serve: no local endpoint for {name}: {err}");
                continue;
            }
        };
        let workspaces: Arc<dyn WorkspacesBackend> =
            Arc::new(LocalWorkspaces::local(local.repo().clone(), None));
        let server = Server::new(
            Hosts::from_local(name.to_owned(), Arc::clone(local)),
            ServerConfig::default(),
        )
        .with_workspaces(workspaces);
        let mut stopped = stopped.clone();
        let name = name.to_owned();
        tasks.push(tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            };
            if let Err(err) = server.serve_local(&endpoint, shutdown).await {
                eprintln!("hord serve: local endpoint of {name}: {err}");
            }
        }));
    }
    (stop, tasks)
}

/// This user's key in `~/.hord/keys/`, if it exists. `hord serve` never
/// creates one.
fn ui_signer() -> Option<UiSigner> {
    let actor = txn::actor();
    let path = identity::key_path(actor.id()).ok()?;
    if !path.exists() {
        return None;
    }
    let key = identity::read_key(&path).ok()?;
    Some(UiSigner {
        actor,
        key: Arc::new(key),
    })
}
