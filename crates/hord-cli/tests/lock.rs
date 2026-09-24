//! Concurrent CLI processes on one repository wait for the store (ADR 0021)
//! when they open it themselves (`--no-daemon`). With the daemon, see
//! `daemon.rs`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LIB: &str =
    "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    alpha() + 1\n}\n";

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hord(dir: &Path, args: &[&str], lock_timeout: Option<&str>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hord"));
    cmd.args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_NO_DAEMON", "1")
        .env_remove("HORD_AGENT_MODEL");
    match lock_timeout {
        Some(secs) => cmd.env("HORD_LOCK_TIMEOUT", secs),
        None => cmd.env_remove("HORD_LOCK_TIMEOUT"),
    };
    cmd
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", describe(&out));
}

/// A git repo with `src/lib.rs`, imported into a fresh hord repo.
fn setup() -> (TempDir, TempDir) {
    let source = TempDir::new("hord-lock-git");
    fs::create_dir_all(source.0.join("src")).unwrap();
    fs::write(source.0.join("src/lib.rs"), LIB).unwrap();
    git(&source.0, &["init", "-q", "-b", "main"]);
    git(&source.0, &["add", "."]);
    git(&source.0, &["commit", "-q", "-m", "fixture"]);
    let repo = TempDir::new("hord-lock-repo");
    let out = hord(
        &repo.0,
        &["init", "--from-git", source.0.to_str().unwrap()],
        None,
    )
    .output()
    .unwrap();
    assert!(out.status.success(), "init: {}", describe(&out));
    (source, repo)
}

fn pid_file(repo: &Path) -> PathBuf {
    repo.join(".hord").join("index.pid")
}

/// The review's repro (docs/review/system.md item 5): before ADR 0021, seven
/// of eight failed with `Database already open. Cannot acquire lock.`
#[test]
fn eight_concurrent_ws_new_all_succeed() {
    let (_source, repo) = setup();
    let children: Vec<_> = (0..8)
        .map(|_| {
            hord(&repo.0, &["ws", "new", "--json"], None)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ids = HashSet::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "ws new: {}", describe(&out));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let id = v["id"].as_str().unwrap().to_owned();
        assert!(Path::new(v["materialization"].as_str().unwrap()).is_dir());
        assert!(ids.insert(id), "duplicate workspace id");
    }
    assert_eq!(ids.len(), 8);
    assert!(
        !pid_file(&repo.0).exists(),
        "the last holder removed its pid"
    );
}

#[test]
fn zero_timeout_fails_fast_with_a_typed_json_error() {
    let (_source, repo) = setup();
    let held = hord_store::Store::open(&repo.0).unwrap();

    let started = Instant::now();
    let out = hord(&repo.0, &["ws", "new", "--json"], Some("0"))
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stderr)
        .unwrap_or_else(|err| panic!("{err}: {}", describe(&out)));
    assert_eq!(v["kind"], "store_locked", "{v}");
    assert_eq!(v["holder"], std::process::id(), "{v}");
    assert!(v["lock"].as_str().unwrap().ends_with("index.redb"), "{v}");
    assert!(v["waitedSecs"].as_f64().unwrap() < 1.0, "{v}");
    let message = v["error"].as_str().unwrap();
    assert!(
        message.contains(&format!("locked by pid {}", std::process::id())),
        "{message}"
    );

    let text = hord(&repo.0, &["status"], Some("0")).output().unwrap();
    assert_eq!(text.status.code(), Some(1), "{}", describe(&text));
    let stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        stderr.starts_with("error: hord store is locked by pid"),
        "{stderr}"
    );
    drop(held);
}

#[test]
fn a_waiting_command_runs_once_the_holder_releases() {
    let (_source, repo) = setup();
    let held = hord_store::Store::open(&repo.0).unwrap();
    let child = hord(&repo.0, &["ws", "new", "--json"], Some("30"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "ws new: {}", describe(&out));
}

#[test]
fn stale_pid_file_does_not_block() {
    let (_source, repo) = setup();
    let mut dead = Command::new("true").spawn().unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    fs::write(pid_file(&repo.0), pid.to_string()).unwrap();

    let out = hord(&repo.0, &["ws", "new", "--json"], Some("0"))
        .output()
        .unwrap();
    assert!(out.status.success(), "ws new: {}", describe(&out));
    assert!(!pid_file(&repo.0).exists());
}

#[test]
fn invalid_lock_timeout_is_a_clear_error() {
    let (_source, repo) = setup();
    let out = hord(&repo.0, &["status", "--json"], Some("soon"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("HORD_LOCK_TIMEOUT"),
        "{v}"
    );
}
