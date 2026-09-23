//! Waiting for the redb index lock held by another process (ADR 0021, "Now").
//!
//! redb admits one process per database file. [`acquire`] retries a locked
//! open with backoff until the lock timeout, then fails with
//! [`Error::Locked`]. The process that holds the store writes its pid next to
//! the index so the error can name it. The pid file is informational only: the
//! redb file lock is the lock, so a stale pid file never blocks an open.

use std::env::{self, VarError};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use redb::{Database, DatabaseError};

use crate::{Error, Result};

/// Environment variable with the lock timeout in seconds (`0` fails at once).
pub(crate) const LOCK_TIMEOUT_ENV: &str = "HORD_LOCK_TIMEOUT";
/// Lock timeout when [`LOCK_TIMEOUT_ENV`] is unset.
pub(crate) const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// Pid file of the process holding the index, next to `index.redb`.
const PID_FILE: &str = "index.pid";
const FIRST_DELAY: Duration = Duration::from_millis(2);
const MAX_DELAY: Duration = Duration::from_millis(50);

/// The lock timeout from [`LOCK_TIMEOUT_ENV`], or [`DEFAULT_LOCK_TIMEOUT`].
pub(crate) fn timeout_from_env() -> Result<Duration> {
    match env::var(LOCK_TIMEOUT_ENV) {
        Ok(value) => parse_timeout(&value),
        Err(VarError::NotPresent) => Ok(DEFAULT_LOCK_TIMEOUT),
        Err(VarError::NotUnicode(value)) => {
            Err(Error::LockTimeout(value.to_string_lossy().into_owned()))
        }
    }
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_LOCK_TIMEOUT);
    }
    trimmed
        .parse::<f64>()
        .ok()
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .ok_or_else(|| Error::LockTimeout(value.to_owned()))
}

/// Open the index with `open`, retrying while another process holds it.
///
/// On success, records this process as the holder in `hord_dir`.
pub(crate) fn acquire(
    hord_dir: &Path,
    index: &Path,
    timeout: Duration,
    mut open: impl FnMut() -> std::result::Result<Database, DatabaseError>,
) -> Result<Database> {
    let started = Instant::now();
    let mut delay = FIRST_DELAY;
    loop {
        match open() {
            Ok(db) => {
                write_pid(hord_dir);
                return Ok(db);
            }
            Err(DatabaseError::DatabaseAlreadyOpen) => {
                let waited = started.elapsed();
                if waited >= timeout {
                    return Err(Error::Locked {
                        lock: index.to_path_buf(),
                        holder: holder(hord_dir),
                        waited,
                    });
                }
                std::thread::sleep(jitter(delay).min(timeout - waited));
                delay = (delay * 2).min(MAX_DELAY);
            }
            Err(err) => return Err(Error::index(err)),
        }
    }
}

/// Remove this process's pid file. Call while the index is still open, so a
/// later holder cannot have written its own pid yet.
pub(crate) fn release(hord_dir: &Path) {
    let path = pid_path(hord_dir);
    if read_pid(&path) == Some(std::process::id()) {
        let _ = fs::remove_file(path);
    }
}

fn pid_path(hord_dir: &Path) -> PathBuf {
    hord_dir.join(PID_FILE)
}

/// Best effort: the pid only improves the lock error.
fn write_pid(hord_dir: &Path) {
    let _ = fs::write(pid_path(hord_dir), std::process::id().to_string());
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The pid in the pid file, if that process is still running.
fn holder(hord_dir: &Path) -> Option<u32> {
    let pid = read_pid(&pid_path(hord_dir))?;
    (pid == std::process::id() || is_running(pid)).then_some(pid)
}

#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    // `hord-store` forbids `unsafe`, so no direct `kill(2)`. This runs only
    // when an open has already timed out.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn is_running(_pid: u32) -> bool {
    false
}

/// `delay` scaled into `[delay/2, delay)` so waiting processes spread out.
fn jitter(delay: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let seed = nanos ^ std::process::id().wrapping_mul(0x9E37_79B9);
    let half = delay / 2;
    half + half.mul_f64(f64::from(seed % 1024) / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timeouts() {
        assert_eq!(parse_timeout("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_timeout(" 2.5 ").unwrap(), Duration::from_millis(2500));
        assert_eq!(parse_timeout("").unwrap(), DEFAULT_LOCK_TIMEOUT);
        assert!(matches!(parse_timeout("-1"), Err(Error::LockTimeout(_))));
        assert!(matches!(parse_timeout("soon"), Err(Error::LockTimeout(_))));
    }
}
