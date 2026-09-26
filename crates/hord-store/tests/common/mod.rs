//! Shared helpers for `hord-store` integration tests.

#![allow(dead_code)]

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_store::Store;

/// A fresh directory for a repository, named for the pid and a per-process
/// counter; a leftover from a reused pid is cleared first.
pub fn temp_repo() -> io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = env::temp_dir().join(format!(
        "hord-store-{}-{}",
        process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path)?;
    Ok(path)
}

/// Removes its directory on drop.
pub struct Guard(pub PathBuf);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A store created in a fresh directory, removed when the guard drops.
pub fn store() -> Result<(Guard, Store), Box<dyn Error>> {
    let path = temp_repo()?;
    let store = Store::create(&path)?;
    Ok((Guard(path), store))
}
