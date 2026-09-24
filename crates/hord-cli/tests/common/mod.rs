//! Shared helpers for `hord-cli` integration tests.

#![allow(dead_code)]

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// Remove `path` and everything under it, if it exists. Directory
/// workspaces keep a read-only pristine checkout under `.hord/pristine/`
/// (ADR 0016), which `fs::remove_dir_all` alone cannot empty, so write
/// access is restored to every directory first.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path).is_err_and(|err| err.kind() == ErrorKind::NotFound) {
        return Ok(());
    }
    make_writable(path)?;
    fs::remove_dir_all(path)
}

fn make_writable(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir)?.permissions().mode();
        fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

/// Clear a stale leftover at `path` (paths repeat when the OS reuses a
/// pid), failing if it cannot be removed rather than colliding later.
pub fn clear_stale(path: &Path) -> Result<(), String> {
    remove_tree(path).map_err(|err| format!("remove stale temp dir {}: {err}", path.display()))
}

/// Remove a test's temp dir when it is dropped. A drop cannot return an
/// error, and panicking there could abort a failing test and lose its
/// failure, so a failed removal is reported on stderr; [`clear_stale`]
/// fails loudly if the leftover is still there next time.
pub fn drop_tree(path: &Path) {
    if let Err(err) = remove_tree(path) {
        eprintln!("remove temp dir {}: {err}", path.display());
    }
}
