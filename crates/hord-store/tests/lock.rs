//! Waiting for a store another holder has open (ADR 0021, "Now").

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hord_store::{Error, Store};

fn temp_repo() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-lock-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pid_file(repo: &Path) -> PathBuf {
    repo.join(".hord").join("index.pid")
}

/// Pid of a process that has already exited.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn zero_timeout_fails_at_once_naming_lock_and_holder() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let held = Store::create(&path).unwrap();
    assert_eq!(
        fs::read_to_string(pid_file(&path)).unwrap(),
        std::process::id().to_string()
    );

    let started = Instant::now();
    let err = Store::open_with_lock_timeout(&path, Duration::ZERO).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    match &err {
        Error::Locked { lock, holder, .. } => {
            assert_eq!(lock, &path.join(".hord").join("index.redb"));
            assert_eq!(*holder, Some(std::process::id()));
        }
        other => panic!("expected Error::Locked, got {other:?}"),
    }
    let message = err.to_string();
    assert!(message.contains("locked"), "{message}");
    assert!(message.contains("index.redb"), "{message}");
    assert!(
        message.contains(&format!("pid {}", std::process::id())),
        "{message}"
    );
    drop(held);
}

#[test]
fn open_waits_for_holder_to_release() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let held = Store::create(&path).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let started = Instant::now();
    let store = Store::open_with_lock_timeout(&path, Duration::from_secs(20)).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(250));
    release.join().unwrap();
    drop(store);
    assert!(!pid_file(&path).exists(), "drop removes the pid file");
}

#[test]
fn bounded_wait_times_out() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let _held = Store::create(&path).unwrap();
    let started = Instant::now();
    let err = Store::open_with_lock_timeout(&path, Duration::from_millis(200)).unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(err, Error::Locked { waited, .. } if waited >= Duration::from_millis(200)));
    assert!(elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5));
}

#[test]
fn stale_pid_file_does_not_block() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    drop(Store::create(&path).unwrap());
    let dead = dead_pid();
    fs::write(pid_file(&path), dead.to_string()).unwrap();

    let store = Store::open_with_lock_timeout(&path, Duration::ZERO).unwrap();
    assert_eq!(
        fs::read_to_string(pid_file(&path)).unwrap(),
        std::process::id().to_string()
    );
    drop(store);
    assert!(!pid_file(&path).exists());
}

#[test]
fn stale_pid_is_not_named_as_holder() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let _held = Store::create(&path).unwrap();
    // The holder's pid file was replaced by a dead process's pid.
    fs::write(pid_file(&path), dead_pid().to_string()).unwrap();
    let err = Store::open_with_lock_timeout(&path, Duration::ZERO).unwrap_err();
    assert!(matches!(err, Error::Locked { holder: None, .. }), "{err:?}");
    assert!(err.to_string().contains("another process"), "{err}");
}
