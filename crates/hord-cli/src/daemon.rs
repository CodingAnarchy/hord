//! The per-repo daemon (ADR 0021, ADR 0024 amendment): `hord serve --repo
//! <root> --daemon` on the repository's local endpoint (a Unix socket, or a
//! named pipe on Windows), serving `RepoBackend` and `Workspaces`.
//!
//! The CLI connects to a running daemon, or starts one and waits for it.
//! The daemon owns the store and the lander, keeps parse and reference
//! caches warm across calls, and exits after `HORD_DAEMON_IDLE_SECS`
//! (default 300) without calls, when its `.hord/` directory disappears, or
//! when asked (`Shutdown`, which the CLI sends before a command that needs
//! the store itself).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hord_api::{RepoBackend, WorkspacesBackend, proto};
use hord_remote::RemoteRepo;

use crate::txn::block_on;
use crate::workspaces::LocalWorkspaces;

/// How long the CLI waits for a daemon it started.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the CLI waits for a daemon it stopped to let go.
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
/// Default idle time before a daemon exits.
const DEFAULT_IDLE: Duration = Duration::from_secs(300);

fn idle_timeout() -> Duration {
    std::env::var("HORD_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(DEFAULT_IDLE, Duration::from_secs)
}

/// The running daemon for the repository at `root`, if one answers.
pub fn connect(root: &Path) -> Option<RemoteRepo> {
    let remote = block_on(RemoteRepo::connect_local(root)).ok()?;
    block_on(remote.head(proto::HeadRequest {})).ok()?;
    Some(remote)
}

/// The running daemon, else a new one. `None` when none could be started
/// in time (the caller then opens the store itself).
pub fn connect_or_start(root: &Path) -> Result<Option<RemoteRepo>> {
    if let Some(remote) = connect(root) {
        return Ok(Some(remote));
    }
    spawn(root)?;
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Some(remote) = connect(root) {
            return Ok(Some(remote));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "hord: the daemon did not answer on {} within {}s; working without it (see {})",
        hord_api::local::endpoint(root).unwrap_or_else(|e| format!("<no endpoint: {e}>")),
        START_TIMEOUT.as_secs(),
        log_path(root).display()
    );
    Ok(None)
}

/// Ask the running daemon, if any, to exit, and wait until it has
/// released the store.
pub fn stop(root: &Path) -> Result<()> {
    let Some(remote) = connect(root) else {
        return Ok(());
    };
    block_on(remote.workspaces().shutdown(proto::ShutdownRequest {}))
        .context("ask the daemon to stop")?;
    drop(remote);
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if connect(root).is_none() && store_free(root) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("the daemon for {} did not stop", root.display())
}

/// Whether the store can be opened now (no process holds its lock).
fn store_free(root: &Path) -> bool {
    hord_store::Store::open_with_lock_timeout(root, Duration::ZERO).is_ok()
}

/// Where a daemon started by the CLI writes its errors.
pub fn log_path(root: &Path) -> std::path::PathBuf {
    root.join(hord_store::HORD_DIR).join("daemon.log")
}

/// Start `hord serve --repo <root> --daemon`, detached. Its stderr goes to
/// [`log_path`], so a daemon that fails to start leaves the reason behind.
fn spawn(root: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locate the hord executable")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(root))
        .with_context(|| format!("open {}", log_path(root).display()))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve")
        .arg("--repo")
        .arg(root)
        .arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    detach(&mut command);
    command.spawn().context("start the hord daemon")?;
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group: the daemon outlives the command's terminal.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Run as the daemon for `root` until idle, asked to stop, or orphaned.
pub async fn serve(root: &Path) -> Result<()> {
    let endpoint = hord_api::local::endpoint(root)?;
    // A daemon that cannot have the store exits at once: another daemon,
    // or a `--no-daemon` command, holds it.
    let store = tokio::task::spawn_blocking({
        let root = root.to_path_buf();
        move || hord_store::Store::open_with_lock_timeout(&root, Duration::from_secs(2))
    })
    .await??;
    let repo = hord_txn::Repo::from_store(store, hord_txn::RepoOptions::default()).await?;
    let stop = Arc::new(tokio::sync::Notify::new());
    let workspaces: Arc<dyn WorkspacesBackend> = Arc::new(LocalWorkspaces::local(
        repo.clone(),
        Some(Arc::clone(&stop)),
    ));
    let hosts = hord_server::Hosts::from_repo(repo)?;
    let config =
        hord_server::ServerConfig::load(&root.join(hord_store::HORD_DIR).join("server.toml"))?;
    let server = hord_server::Server::new(hosts, config).with_workspaces(workspaces);
    let activity = server.activity();
    let hord_dir = root.join(hord_store::HORD_DIR);
    let idle = idle_timeout();
    let shutdown = async move {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                () = stop.notified() => return,
                _ = tick.tick() => {
                    if activity.idle() >= idle || !hord_dir.is_dir() {
                        return;
                    }
                }
            }
        }
    };
    eprintln!(
        "hord daemon {}: listening on {endpoint}",
        std::process::id()
    );
    server.serve_local(&endpoint, shutdown).await?;
    Ok(())
}
