//! Shared helpers for `hord-git` integration tests.

#![allow(dead_code)]

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh directory under the system temp dir, removed on drop. Its name
/// is `tag`, the pid and a per-process counter; a leftover from a reused
/// pid is cleared first.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> io::Result<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = env::temp_dir().join(format!(
            "hord-git-{tag}-{}-{}",
            process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
