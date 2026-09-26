//! Waiting for a store another holder has open (ADR 0021, "Now").

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hord_store::{Error, Store};

use common::{Guard, temp_repo};

fn pid_file(repo: &Path) -> PathBuf {
    repo.join(".hord").join("index.pid")
}

/// Pid of a process that has already exited.
fn dead_pid() -> std::io::Result<u32> {
    let mut child = std::process::Command::new("true").spawn()?;
    let pid = child.id();
    child.wait()?;
    Ok(pid)
}

#[test]
fn zero_timeout_fails_at_once_naming_lock_and_holder() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let held = Store::create(&path)?;
    assert_eq!(
        fs::read_to_string(pid_file(&path))?,
        std::process::id().to_string()
    );

    let started = Instant::now();
    let err = Store::open_with_lock_timeout(&path, Duration::ZERO)
        .expect_err("opening a held store with a zero timeout fails");
    assert!(started.elapsed() < Duration::from_secs(1));
    match &err {
        Error::Locked { lock, holder, .. } => {
            assert_eq!(lock, &path.join(".hord").join("index.redb"));
            assert_eq!(*holder, Some(std::process::id()));
        }
        other => return Err(format!("expected Error::Locked, got {other:?}").into()),
    }
    let message = err.to_string();
    assert!(message.contains("locked"), "{message}");
    assert!(message.contains("index.redb"), "{message}");
    assert!(
        message.contains(&format!("pid {}", std::process::id())),
        "{message}"
    );
    drop(held);
    Ok(())
}

#[test]
fn open_waits_for_holder_to_release() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let held = Store::create(&path)?;
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let started = Instant::now();
    let store = Store::open_with_lock_timeout(&path, Duration::from_secs(20))?;
    assert!(started.elapsed() >= Duration::from_millis(250));
    release
        .join()
        .expect("join the thread that releases the store");
    drop(store);
    assert!(!pid_file(&path).exists(), "drop removes the pid file");
    Ok(())
}

#[test]
fn bounded_wait_times_out() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let _held = Store::create(&path)?;
    let started = Instant::now();
    let err = Store::open_with_lock_timeout(&path, Duration::from_millis(200))
        .expect_err("opening a held store times out");
    let elapsed = started.elapsed();
    assert!(matches!(err, Error::Locked { waited, .. } if waited >= Duration::from_millis(200)));
    assert!(elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5));
    Ok(())
}

#[test]
fn stale_pid_file_does_not_block() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    drop(Store::create(&path)?);
    let dead = dead_pid()?;
    fs::write(pid_file(&path), dead.to_string())?;

    let store = Store::open_with_lock_timeout(&path, Duration::ZERO)?;
    assert_eq!(
        fs::read_to_string(pid_file(&path))?,
        std::process::id().to_string()
    );
    drop(store);
    assert!(!pid_file(&path).exists());
    Ok(())
}

#[test]
fn stale_pid_is_not_named_as_holder() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let _held = Store::create(&path)?;
    // The holder's pid file was replaced by a dead process's pid.
    fs::write(pid_file(&path), dead_pid()?.to_string())?;
    let err = Store::open_with_lock_timeout(&path, Duration::ZERO)
        .expect_err("opening a store held by a live process fails");
    assert!(matches!(err, Error::Locked { holder: None, .. }), "{err:?}");
    assert!(err.to_string().contains("another process"), "{err}");
    Ok(())
}
