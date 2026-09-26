//! Stopping the runner stops what it started. Each case runs under its own
//! `hord serve`, whose lander runs `hord-replay-ref`, the harness command,
//! and a model CLI in the harness's own process group. Killing the runner
//! alone orphaned all of them, and a model kept spending. On SIGINT or
//! SIGTERM the runner asks every `hord serve` it started to stop (SIGINT:
//! its lander cancels running replays, which kills each harness's process
//! group), waits for them, kills any that do not stop, and exits. This runs
//! before any destructor, whose kill-on-drop would skip the graceful stop.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Running `hord serve` processes, by pid.
static SERVERS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// How long each server gets to stop before it is killed.
const GRACE: Duration = Duration::from_secs(20);

fn servers() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    SERVERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Track a `hord serve` the runner started.
pub fn register(pid: u32) {
    servers().insert(pid);
}

/// Stop tracking a `hord serve` the runner stopped itself.
pub fn unregister(pid: u32) {
    servers().remove(&pid);
}

/// Wait for SIGINT or SIGTERM, stop every server, and exit.
pub async fn on_signal() {
    let signal = wait_for_signal().await;
    eprintln!("hord-eval-m5: {signal}: stopping every hord serve it started");
    stop_all().await;
    std::process::exit(130);
}

#[cfg(unix)]
async fn wait_for_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // No handler: never fire (the default action then applies).
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl-C"
}

/// Ask every tracked server to stop, wait up to [`GRACE`], and kill the
/// rest.
async fn stop_all() {
    let pids: Vec<u32> = servers().iter().copied().collect();
    for pid in &pids {
        send("-INT", *pid).await;
    }
    let deadline = Instant::now() + GRACE;
    let mut left = pids;
    while !left.is_empty() && Instant::now() < deadline {
        let mut still = Vec::new();
        for pid in left {
            if alive(pid).await {
                still.push(pid);
            }
        }
        left = still;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for pid in left {
        send("-KILL", pid).await;
    }
}

#[cfg(unix)]
async fn send(signal: &str, pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .await;
}

#[cfg(unix)]
async fn alive(pid: u32) -> bool {
    tokio::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
async fn send(_signal: &str, pid: u32) {
    let _ = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .await;
}

#[cfg(not(unix))]
async fn alive(_pid: u32) -> bool {
    false
}
