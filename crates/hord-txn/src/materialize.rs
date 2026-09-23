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
//! mtime, so every clone of that pristine gets a byte copy of it, without
//! walking or re-encoding. Inodes are not compared (a clone gets new ones), as
//! with git's `core.checkStat=minimal`. A copy does not keep mtimes on every
//! platform, so a copied checkout is walked once for its own index.
//!
//! Racily clean entries (git's rule): an entry whose recorded mtime is within
//! [`RACY_WINDOW_NS`] of the index file's own mtime could share a clock tick
//! with a later same-size write, so it is re-hashed instead of trusted. So
//! that fresh checkouts are not racy on every file, checkout files are
//! backdated below that window before they are indexed.
//!
//! Walking a checkout (`propose`, `list_files`) runs in parallel and honors
//! the base snapshot's `.gitignore` files plus a built-in default (`.hord/`,
//! `.git`): an untracked path an ignore rule matches is skipped and reported,
//! and an ignored directory with no tracked file under it is not descended.
//! A tracked path is never skipped, as in git.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use hord_core::{ObjectId, RepoPath, SnapshotId};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{WalkBuilder, WalkState};
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

/// How close to the index's own write time a recorded mtime must be to count
/// as racily clean. Two seconds covers the coarsest common timestamps (FAT,
/// HFS+ at 1 s) as well as Linux's jiffy-granular coarse clock.
pub(crate) const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// How far below the start of a checkout its files' mtimes are set, so no
/// entry of a fresh index is racily clean.
const BACKDATE: Duration = Duration::from_secs(4);

/// Stat index of a checkout, stored as canonical CBOR in `<dir>.stat`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StatIndex {
    /// How the checkout was made. A pristine's index records
    /// [`MaterializeMode::Clone`], since its clones share it byte for byte.
    pub mode: Option<MaterializeMode>,
    pub files: BTreeMap<RepoPath, Stat>,
    /// Mtime of the index file itself, read when it is loaded (not stored).
    #[serde(skip)]
    pub written_ns: i128,
}

impl StatIndex {
    /// Whether `path` is known unchanged: its stat matches the index and the
    /// entry is not racily clean (its recorded mtime is not within
    /// [`RACY_WINDOW_NS`] of the index's write time).
    pub fn is_clean(&self, path: &RepoPath, stat: Stat) -> bool {
        self.files.get(path).is_some_and(|at| {
            *at == stat && at.mtime_ns < self.written_ns.saturating_sub(RACY_WINDOW_NS)
        })
    }
}

pub(crate) fn stat_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".stat");
    dir.with_file_name(name)
}

pub(crate) fn load_stat_index(dir: &Path) -> Option<StatIndex> {
    let mut file = fs::File::open(stat_path(dir)).ok()?;
    let written_ns = Stat::of(&file.metadata().ok()?).mtime_ns;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let mut index: StatIndex = hord_encoding::decode(&bytes).ok()?;
    index.written_ns = written_ns;
    Some(index)
}

fn write_stat_index(dir: &Path, index: &StatIndex) -> Result<()> {
    fs::write(stat_path(dir), hord_encoding::encode(index)?)?;
    Ok(())
}

/// Stat every file under `dir`, first setting any mtime later than
/// `backdate` to it.
fn index_of(
    dir: &Path,
    mode: Option<MaterializeMode>,
    backdate: Option<SystemTime>,
) -> Result<StatIndex> {
    Ok(StatIndex {
        mode,
        files: walk_checkout(dir, None, backdate)?
            .files
            .into_iter()
            .collect(),
        written_ns: 0,
    })
}

/// The time checkout files written from now on are backdated to.
fn backdate_from_now() -> Option<SystemTime> {
    SystemTime::now().checked_sub(BACKDATE)
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
        let backdate = backdate_from_now();
        self.checkout(snapshot, &tmp)?;
        // The index is written before the rename, so a pristine directory
        // that exists always has one.
        let index = index_of(&tmp, Some(MaterializeMode::Clone), backdate)?;
        write_stat_index(&dir, &index)?;
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
        // A clone keeps sizes and mtimes: it shares the pristine's index,
        // copied byte for byte (it already records `Clone`). A copy may get
        // fresh mtimes, so it is indexed on its own.
        let shared = used == MaterializeMode::Clone
            && fs::copy(stat_path(&pristine), stat_path(dest)).is_ok();
        if !shared {
            write_stat_index(dest, &index_of(dest, Some(used), backdate_from_now())?)?;
        }
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

/// What a walk of a `Directory` checkout found.
#[derive(Debug, Default)]
pub(crate) struct Walked {
    /// Regular files that are tracked or not ignored, with their stat.
    pub files: Vec<(RepoPath, Stat)>,
    /// Untracked paths left out, sorted: those an ignore rule matches (an
    /// ignored directory is listed once, not its contents) and entries that
    /// are neither regular files nor directories (a tree has no symlinks).
    pub skipped: Vec<RepoPath>,
}

/// Which paths of a checkout a walk skips: the base snapshot's `.gitignore`
/// files plus a built-in default, never a tracked path.
#[derive(Debug)]
pub(crate) struct CheckoutFilter {
    /// `.hord/` and `.git`, at any depth.
    builtin: Gitignore,
    /// Matchers of the base's `.gitignore` files, by the directory holding
    /// each one.
    gitignores: HashMap<RepoPath, Gitignore>,
    /// Files of the base snapshot, with their blobs.
    tracked: HashMap<RepoPath, ObjectId>,
    /// Directories that hold a tracked file, and whether an ignore rule
    /// covers each (its untracked contents are then ignored too).
    tracked_dirs: HashMap<RepoPath, bool>,
}

impl CheckoutFilter {
    /// Built-in ignore rules. Build output is left to the `.gitignore` files:
    /// nothing language-specific is hard-coded.
    const BUILTIN: [&str; 2] = [".hord/", ".git"];

    /// `files` in path order, as `list_files` gives them.
    fn new(
        root: &Path,
        files: Vec<(RepoPath, ObjectId)>,
        gitignores: &[(RepoPath, Vec<u8>)],
    ) -> Self {
        let matcher = |dir: &RepoPath, lines: &mut dyn Iterator<Item = &str>| {
            let mut builder = GitignoreBuilder::new(fs_path(root, dir));
            for line in lines {
                // Git skips a pattern it cannot parse; so does this.
                let _ = builder.add_line(None, line);
            }
            builder.build().unwrap_or_else(|_| Gitignore::empty())
        };
        let builtin = matcher(&RepoPath::default(), &mut Self::BUILTIN.into_iter());
        let gitignores = gitignores
            .iter()
            .map(|(dir, bytes)| {
                let text = String::from_utf8_lossy(bytes);
                (dir.clone(), matcher(dir, &mut text.lines()))
            })
            .collect();
        let mut filter = Self {
            builtin,
            gitignores,
            tracked: HashMap::with_capacity(files.len()),
            tracked_dirs: HashMap::new(),
        };
        // Files in path order share their directory with the previous file
        // most of the time; only a new directory's prefixes are looked at.
        // Prefixes go shortest first, so each directory's parent is decided
        // before it.
        let mut last_parent: &[String] = &[];
        for (path, _) in &files {
            let parts = path.components();
            let parent = &parts[..parts.len().saturating_sub(1)];
            if parent == last_parent {
                continue;
            }
            last_parent = parent;
            for n in 1..=parent.len() {
                let dir = RepoPath::new(parent[..n].to_vec());
                if !filter.tracked_dirs.contains_key(&dir) {
                    let ignored = filter.ignored(&fs_path(root, &dir), &dir, true);
                    filter.tracked_dirs.insert(dir, ignored);
                }
            }
        }
        filter.tracked.extend(files);
        filter
    }

    /// Files of the base snapshot, with their blobs.
    pub fn tracked(&self) -> &HashMap<RepoPath, ObjectId> {
        &self.tracked
    }

    /// Whether an ignore rule covers `path` (at `abs` in the checkout), or a
    /// directory above it. Tracked status is not considered here.
    fn ignored(&self, abs: &Path, path: &RepoPath, is_dir: bool) -> bool {
        let parts = path.components();
        let Some((_, parent)) = parts.split_last() else {
            return false;
        };
        // An ignored directory with no tracked file is not descended, so
        // only a tracked directory can have an ignored parent here.
        if self
            .tracked_dirs
            .get(&RepoPath::new(parent.to_vec()))
            .copied()
            .unwrap_or(false)
        {
            return true;
        }
        if self.builtin.matched(abs, is_dir).is_ignore() {
            return true;
        }
        // The deepest `.gitignore` with an opinion decides, as in git.
        for depth in (0..parts.len()).rev() {
            if let Some(gitignore) = self.gitignores.get(&RepoPath::new(parts[..depth].to_vec())) {
                let found = gitignore.matched(abs, is_dir);
                if !found.is_none() {
                    return found.is_ignore();
                }
            }
        }
        false
    }
}

impl Inner {
    /// The filter for walking a checkout of a snapshot with `files` at
    /// `root`: reads the snapshot's `.gitignore` files.
    pub(crate) fn checkout_filter(
        &self,
        root: &Path,
        files: Vec<(RepoPath, ObjectId)>,
    ) -> Result<CheckoutFilter> {
        let mut gitignores = Vec::new();
        for (path, blob) in &files {
            if let Some((name, dir)) = path.components().split_last()
                && name == ".gitignore"
            {
                let bytes = self.blob_bytes(*blob)?;
                gitignores.push((RepoPath::new(dir.to_vec()), bytes.as_slice().to_vec()));
            }
        }
        Ok(CheckoutFilter::new(root, files, &gitignores))
    }
}

/// What the walk does with one entry.
enum Visit {
    Descend,
    File(RepoPath, Stat),
    /// Left out; `true` also prunes a directory.
    Skip(RepoPath, bool),
    Nothing,
}

/// Walk the checkout at `dir` in parallel. With a `filter`, untracked
/// ignored paths and untracked non-files are skipped (and reported), and a
/// tracked path that is now a symlink or other non-file is
/// [`Error::UnsupportedEntry`]: the tree cannot hold it, and it must not be
/// taken for a deletion. With `backdate`, any file mtime later than it is set
/// to it before the file is stat'ed.
pub(crate) fn walk_checkout(
    dir: &Path,
    filter: Option<&CheckoutFilter>,
    backdate: Option<SystemTime>,
) -> Result<Walked> {
    let walked = Mutex::new(Walked::default());
    let failed = Mutex::new(None);
    // `lstat` on APFS scales poorly past a few threads (cargo checkout:
    // 33 ms on one thread, 21 ms on four, no better on eight), and several
    // proposes may walk at once.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    WalkBuilder::new(dir)
        .standard_filters(false)
        .follow_links(false)
        .threads(threads)
        .build_parallel()
        .run(|| {
            Box::new(|entry| match visit(dir, filter, backdate, entry) {
                Ok(Visit::Descend | Visit::Nothing) => WalkState::Continue,
                Ok(Visit::File(path, stat)) => {
                    lock(&walked).files.push((path, stat));
                    WalkState::Continue
                }
                Ok(Visit::Skip(path, prune)) => {
                    lock(&walked).skipped.push(path);
                    if prune {
                        WalkState::Skip
                    } else {
                        WalkState::Continue
                    }
                }
                Err(err) => {
                    lock(&failed).get_or_insert(err);
                    WalkState::Quit
                }
            })
        });
    if let Some(err) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(err);
    }
    let mut walked = walked.into_inner().unwrap_or_else(PoisonError::into_inner);
    walked.files.sort_by(|a, b| a.0.cmp(&b.0));
    walked.skipped.sort();
    Ok(walked)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn visit(
    dir: &Path,
    filter: Option<&CheckoutFilter>,
    backdate: Option<SystemTime>,
    entry: std::result::Result<ignore::DirEntry, ignore::Error>,
) -> Result<Visit> {
    let entry = entry.map_err(|err| {
        let message = err.to_string();
        Error::Io(
            err.into_io_error()
                .unwrap_or_else(|| std::io::Error::other(message)),
        )
    })?;
    if entry.depth() == 0 {
        return Ok(Visit::Descend);
    }
    let rel = entry.path().strip_prefix(dir).unwrap_or(entry.path());
    let parts = rel
        .iter()
        .map(|part| {
            part.to_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidPath(part.to_string_lossy().into_owned()))
        })
        .collect::<Result<Vec<_>>>()?;
    let path = RepoPath::new(parts);
    let kind = entry.file_type();
    let is_dir = kind.is_some_and(|k| k.is_dir());
    let is_file = kind.is_some_and(|k| k.is_file());
    let tracked = filter.is_none_or(|f| f.tracked.contains_key(&path));
    if is_dir {
        let prune = filter.is_some_and(|f| {
            !f.tracked_dirs.contains_key(&path) && f.ignored(entry.path(), &path, true)
        });
        return Ok(if prune {
            Visit::Skip(path, true)
        } else {
            Visit::Descend
        });
    }
    if let Some(f) = filter
        && !tracked
        && f.ignored(entry.path(), &path, false)
    {
        return Ok(Visit::Skip(path, false));
    }
    if is_file {
        let mut meta = fs::symlink_metadata(entry.path())?;
        if let Some(to) = backdate
            && meta.modified().is_ok_and(|at| at > to)
        {
            let file = fs::OpenOptions::new().write(true).open(entry.path())?;
            file.set_modified(to)?;
            meta = file.metadata()?;
        }
        return Ok(Visit::File(path, Stat::of(&meta)));
    }
    let Some(f) = filter else {
        return Ok(Visit::Nothing);
    };
    if tracked || f.tracked_dirs.contains_key(&path) {
        let kind = if kind.is_some_and(|k| k.is_symlink()) {
            "symlink"
        } else {
            "special file"
        };
        return Err(Error::UnsupportedEntry { path, kind });
    }
    Ok(Visit::Skip(path, false))
}

/// Read `path` under `dir`.
pub(crate) fn read_checkout_file(dir: &Path, path: &RepoPath) -> Result<Vec<u8>> {
    Ok(fs::read(fs_path(dir, path))?)
}
