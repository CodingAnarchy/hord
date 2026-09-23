//! Directory workspaces as copy-on-write clones of a pristine checkout
//! (ADR 0016).
//!
//! Each base snapshot is checked out once under `.hord/pristine/<snapshot>/`.
//! Only that top directory is read-only (ADR 0016 amendment): a clone cannot
//! write through to the pristine, and read-only files would cost every clone
//! an O(files) `chmod` pass, since `clonefile` copies mode bits. A workspace directory is a filesystem clone
//! of it: one `clonefile(2)` of the whole tree on APFS, a `FICLONE` per file
//! on btrfs/XFS (both through `reflink-copy`), or a plain copy where neither
//! works. Data blocks are shared until a file is written.
//!
//! A stat index (path → size, mtime) is written next to each checkout as
//! `<dir>.stat`. `propose` re-reads only files whose stat changed; paranoid
//! mode re-hashes everything (a tool that keeps size and mtime while changing
//! content defeats the index, as it does git's). The index is computed once,
//! when the pristine checkout is written: a clone keeps each file's size and
//! mtime, so every clone of that pristine reuses it without walking the
//! clone. Inodes are not compared (a clone gets new ones), as with git's
//! `core.checkStat=minimal`. A copy does not keep mtimes, so a copied
//! checkout is walked once for its own index.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{RepoPath, SnapshotId};
use serde::{Deserialize, Serialize};

use crate::repo::{Inner, fs_path};
use crate::{Error, Result};

/// How a `Directory` workspace's files were materialized (ADR 0016).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MaterializeMode {
    /// Copy-on-write clone of the pristine checkout. Asked for by default;
    /// falls back to [`MaterializeMode::Copy`] where the filesystem cannot
    /// clone.
    #[default]
    Clone,
    /// Plain copy of every file.
    Copy,
}

impl MaterializeMode {
    /// `clone` or `copy`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clone => "clone",
            Self::Copy => "copy",
        }
    }
}

/// Stat of one checked-out file: what a clone preserves.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Stat {
    pub size: u64,
    pub mtime_ns: i128,
}

impl Stat {
    pub fn of(meta: &fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
        Self {
            size: meta.len(),
            mtime_ns,
        }
    }
}

/// Stat index of a checkout, stored as canonical CBOR in `<dir>.stat`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StatIndex {
    pub mode: Option<MaterializeMode>,
    pub files: BTreeMap<RepoPath, Stat>,
}

pub(crate) fn stat_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".stat");
    dir.with_file_name(name)
}

pub(crate) fn load_stat_index(dir: &Path) -> Option<StatIndex> {
    let bytes = fs::read(stat_path(dir)).ok()?;
    hord_encoding::decode(&bytes).ok()
}

fn write_stat_index(dir: &Path, index: &StatIndex) -> Result<()> {
    fs::write(stat_path(dir), hord_encoding::encode(index)?)?;
    Ok(())
}

/// Stat every file under `dir`.
fn index_of(dir: &Path, mode: Option<MaterializeMode>) -> Result<StatIndex> {
    let mut index = StatIndex {
        mode,
        files: BTreeMap::new(),
    };
    walk_files(dir, &mut Vec::new(), &mut |path, meta| {
        index.files.insert(path, Stat::of(meta));
    })?;
    Ok(index)
}

impl Inner {
    fn pristine_root(&self) -> PathBuf {
        self.store.hord_dir().join("pristine")
    }

    /// The read-only checkout of `snapshot`, written once.
    pub(crate) fn pristine(&self, snapshot: SnapshotId) -> Result<PathBuf> {
        let dir = self.pristine_root().join(snapshot.to_hex());
        if dir.is_dir() {
            return Ok(dir);
        }
        static N: AtomicU64 = AtomicU64::new(0);
        let tmp = self.pristine_root().join(format!(
            ".tmp-{}-{}-{}",
            snapshot.to_hex(),
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&tmp)?;
        self.checkout(snapshot, &tmp)?;
        // The index is written before the rename, so a pristine directory
        // that exists always has one.
        write_stat_index(&dir, &index_of(&tmp, None)?)?;
        match fs::rename(&tmp, &dir) {
            Ok(()) => set_mode(&dir, true, true)?,
            // Another task finished the same checkout first.
            Err(_) if dir.is_dir() => remove_tree(&tmp)?,
            Err(err) => return Err(err.into()),
        }
        Ok(dir)
    }

    /// Materialize `snapshot` at `dest` (which must not exist or be an empty
    /// directory) and write its stat index. Returns the mode actually used.
    pub(crate) fn materialize_dir(
        &self,
        snapshot: SnapshotId,
        dest: &Path,
        mode: MaterializeMode,
    ) -> Result<MaterializeMode> {
        let pristine = self.pristine(snapshot)?;
        if dest.is_dir() {
            fs::remove_dir(dest)?;
        }
        let used = match mode {
            MaterializeMode::Clone => match clone_tree(&pristine, dest) {
                Ok(()) => MaterializeMode::Clone,
                Err(_) => {
                    remove_tree(dest)?;
                    copy_tree(&pristine, dest)?;
                    MaterializeMode::Copy
                }
            },
            MaterializeMode::Copy => {
                copy_tree(&pristine, dest)?;
                MaterializeMode::Copy
            }
        };
        // Only the top directory carries the pristine's read-only bit.
        set_mode(dest, true, false)?;
        // A clone keeps sizes and mtimes: reuse the pristine's index. A copy
        // gets fresh mtimes, so it is indexed on its own.
        let index = match (used, load_stat_index(&pristine)) {
            (MaterializeMode::Clone, Some(mut index)) => {
                index.mode = Some(used);
                index
            }
            _ => index_of(dest, Some(used))?,
        };
        write_stat_index(dest, &index)?;
        Ok(used)
    }

    /// Remove pristine checkouts that no workspace in the store uses as its
    /// base. Returns the snapshots removed.
    pub(crate) fn gc_pristine(&self) -> Result<Vec<SnapshotId>> {
        let live: std::collections::BTreeSet<String> = self
            .store
            .list_workspaces()?
            .into_iter()
            .map(|ws| ws.base.to_hex())
            .collect();
        let mut removed = Vec::new();
        let entries = match fs::read_dir(self.pristine_root()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(removed),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") {
                remove_tree(&entry.path())?;
                continue;
            }
            let stem = name.strip_suffix(".stat").unwrap_or(&name);
            if live.contains(stem) {
                continue;
            }
            if name.ends_with(".stat") {
                remove_tree(&entry.path())?;
                continue;
            }
            if let Ok(snapshot) = name.parse::<SnapshotId>() {
                remove_tree(&entry.path())?;
                removed.push(snapshot);
            }
        }
        Ok(removed)
    }
}

/// Clone `from` to `to`: the whole tree in one call where the platform can
/// (APFS), else file by file. Fails if the filesystem cannot clone.
fn clone_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if cfg!(target_vendor = "apple") {
        return reflink_copy::reflink(from, to);
    }
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            clone_tree(&entry.path(), &target)?;
        } else {
            reflink_copy::reflink(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Set or clear the write bits on every entry under `dir` (directories too,
/// so nothing can be added to a pristine checkout).
fn set_readonly_tree(dir: &Path, readonly: bool) -> std::io::Result<()> {
    // Children first when locking, parent first when unlocking.
    if !readonly {
        set_mode(dir, true, false)?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            set_readonly_tree(&entry.path(), readonly)?;
        } else {
            set_mode(&entry.path(), false, readonly)?;
        }
    }
    if readonly {
        set_mode(dir, true, true)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, dir: bool, readonly: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match (dir, readonly) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, _dir: bool, readonly: bool) -> std::io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)
}

fn remove_tree(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    }
    if path.is_dir() {
        set_readonly_tree(path, false)?;
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Visit every regular file under `dir` with its repository path.
pub(crate) fn walk_files(
    dir: &Path,
    prefix: &mut Vec<String>,
    visit: &mut dyn FnMut(RepoPath, &fs::Metadata),
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| Error::InvalidPath(name.to_string_lossy().into_owned()))?;
        let meta = fs::symlink_metadata(entry.path())?;
        prefix.push(name);
        if meta.is_dir() {
            walk_files(&entry.path(), prefix, visit)?;
        } else if meta.is_file() {
            visit(RepoPath::new(prefix.clone()), &meta);
        }
        prefix.pop();
    }
    Ok(())
}

/// Read `path` under `dir`.
pub(crate) fn read_checkout_file(dir: &Path, path: &RepoPath) -> Result<Vec<u8>> {
    Ok(fs::read(fs_path(dir, path))?)
}
