//! `hord serve [--repo <path>|--root <dir>] [--bind <addr>]` (spec
//! §10.5.1, ADR 0024): gRPC and gRPC-Web on one port, one lander per
//! repository. Loopback only unless `--insecure-bind`. `--daemon` runs the
//! repository's per-repo daemon instead ([`crate::daemon`]).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use hord_server::{Hosts, ServeOptions, Server, ServerConfig};

use crate::repo;

/// The address when neither `--bind` nor `server.toml` gives one.
const DEFAULT_BIND: &str = "127.0.0.1:7878";

pub async fn run(
    repo_path: Option<PathBuf>,
    root: Option<PathBuf>,
    bind: Option<String>,
    insecure_bind: bool,
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
    // Fail on a bad address before opening anything.
    hord_server::check_bind(addr, insecure_bind)?;
    let hosts = match (&repo_root, &root) {
        (Some(path), _) => Hosts::open_repo(path, hord_txn::RepoOptions::default()).await?,
        (None, Some(dir)) => Hosts::open_root(dir, hord_txn::RepoOptions::default).await?,
        (None, None) => unreachable!("one of --repo or --root"),
    };
    let names: Vec<String> = hosts.names().map(str::to_owned).collect();
    let listener = Server::bind(addr, &ServeOptions { insecure_bind }).await?;
    let local = listener.local_addr()?;
    eprintln!("hord serve: http://{local} ({})", names.join(", "));
    let server = Server::new(hosts, config);
    server
        .serve(listener, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
