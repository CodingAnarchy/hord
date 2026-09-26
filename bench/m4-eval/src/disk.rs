//! Bounded disk use for a chain, and disk telemetry.
//!
//! A cargo target directory never forgets. Every commit whose manifest
//! changes a unit's metadata hash leaves the previous build of that unit
//! behind: the cargo corpus's `libcargo-*.rlib` (about 400 MB
//! instrumented), its test binaries, and an incremental cache of about
//! 1 GB each. Cargo's own test suite also keeps a scratch project per test
//! under `CARGO_TARGET_TMPDIR/cit` (about 3 GB per full run). After about
//! 30 commits, one instrumented target directory held 27 GB, of which about
//! 20 GB was stale. [`prune_target`] keeps what the next build needs: the
//! newest [`KEEP_GENERATIONS`] generations of each unit, and no scratch
//! trees.
//!
//! [`Monitor`] logs `df` for `/` and the work directory at each step, and
//! keeps the peak in `chain/disk.json` for the report.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Generations of each unit a target directory keeps: the current build
/// and the one before it (a grader's scoped builds and its whole-package
/// builds can unify features differently, so two hashes of one crate can
/// both be live).
pub(crate) const KEEP_GENERATIONS: usize = 2;

/// Unit directories of a cargo profile directory that hold one entry per
/// unit and metadata hash.
const UNIT_DIRS: [&str; 4] = ["deps", ".fingerprint", "build", "incremental"];

/// Remove stale unit generations and cargo's per-test scratch trees from
/// `target`, and return the bytes freed.
///
/// A unit's entries are grouped by name and extension without the hash. A
/// group is pruned only when a member was written since `since` (the unit
/// was rebuilt with a new hash); then members beyond the newest `keep` that
/// predate `since` are removed. Groups nothing rebuilt are left alone, so
/// several live versions of one dependency survive. Removing a live
/// artifact by mistake costs a rebuild, never a wrong result: cargo
/// rebuilds a unit whose outputs are missing.
pub(crate) fn prune_target(target: &Path, since: SystemTime, keep: usize) -> u64 {
    // Mtime granularity: anything written within two seconds of `since`
    // counts as current.
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let mut freed = 0;
    for profile in profile_dirs(target) {
        for unit_dir in UNIT_DIRS {
            freed += prune_units(&profile.join(unit_dir), since, keep);
        }
    }
    let cit = scratch_dir(target);
    restore_permissions(&cit);
    freed + remove(&cit)
}

/// Cargo's per-test scratch trees in `target` (`CARGO_TARGET_TMPDIR/cit`).
pub(crate) fn scratch_dir(target: &Path) -> PathBuf {
    target.join("tmp").join("cit")
}

/// `chmod -R u+rwx` (best effort): cargo's read-only tests
/// (`registry::readonly_registry_still_works*`,
/// `registry::inaccessible_registry_cache_still_works`, ...) make parts of
/// their scratch tree read-only or `000` and restore them only if they
/// reach the end. A failed run leaves a tree that neither the next run's
/// setup nor [`remove`] can clear, so every later run of those tests in the
/// same target fails (gate run 36189604585). A directory is opened up before
/// it is read, so `000` directories are handled.
pub(crate) fn restore_permissions(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let _ = fs::set_permissions(path, owner_rwx(meta.permissions()));
    if meta.is_dir() {
        for child in read_dir(path) {
            restore_permissions(&child);
        }
    }
}

#[cfg(unix)]
fn owner_rwx(perms: fs::Permissions) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(perms.mode() | 0o700)
}

#[cfg(not(unix))]
fn owner_rwx(mut perms: fs::Permissions) -> fs::Permissions {
    perms.set_readonly(false);
    perms
}

/// The profile directories of a target directory (`debug`, `release`, and
/// `<triple>/debug` for cross builds): those holding a `deps` directory.
fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in read_dir(target) {
        if !entry.is_dir() {
            continue;
        }
        if entry.join("deps").is_dir() {
            out.push(entry);
        } else {
            out.extend(
                read_dir(&entry)
                    .into_iter()
                    .filter(|p| p.join("deps").is_dir()),
            );
        }
    }
    out
}

fn prune_units(dir: &Path, since: SystemTime, keep: usize) -> u64 {
    let mut groups: BTreeMap<(String, String), Vec<(SystemTime, PathBuf)>> = BTreeMap::new();
    for path in read_dir(dir) {
        let Some(key) = path.file_name().and_then(|n| n.to_str()).and_then(unit_key) else {
            continue;
        };
        groups
            .entry(key)
            .or_default()
            .push((freshness(&path), path));
    }
    let mut freed = 0;
    for mut members in groups.into_values() {
        if !members.iter().any(|(t, _)| *t >= since) {
            continue;
        }
        members.sort_by_key(|m| std::cmp::Reverse(m.0));
        for (t, path) in members.into_iter().skip(keep) {
            if t < since {
                freed += remove(&path);
            }
        }
    }
    freed
}

/// `(name, extension)` of a unit entry without its hash: `libcargo-
/// 5b07db85211ebf9f.rlib` is `("libcargo", ".rlib")`, `cargo-2ktbmux74gpf3`
/// (incremental) is `("cargo", "")`. `None` for names without a hash.
fn unit_key(name: &str) -> Option<(String, String)> {
    let (stem, rest) = name.rsplit_once('-')?;
    let (hash, ext) = match rest.find('.') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let is_hash = hash.len() >= 8
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
    (is_hash && !stem.is_empty()).then(|| (stem.to_owned(), ext.to_owned()))
}

/// When an entry was last written: its own mtime, or for a directory the
/// newest of it and its direct children (cargo rewrites fingerprint files
/// in place, and incremental compilation adds a session directory).
fn freshness(path: &Path) -> SystemTime {
    let own = mtime(path);
    if !path.is_dir() {
        return own;
    }
    read_dir(path)
        .iter()
        .map(|p| mtime(p))
        .fold(own, SystemTime::max)
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn read_dir(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// Remove a file or directory tree (best effort), returning its size.
pub(crate) fn remove(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let size = if meta.is_dir() {
        tree_size(path)
    } else {
        meta.len()
    };
    let removed = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    if removed.is_ok() { size } else { 0 }
}

fn tree_size(dir: &Path) -> u64 {
    read_dir(dir)
        .iter()
        .map(|p| match fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => tree_size(p),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Remove every file in `dir` (raw profiles a worker's uninstrumented runs
/// might leave), keeping the directory.
pub(crate) fn clear_dir(dir: &Path) -> u64 {
    read_dir(dir).iter().map(|p| remove(p)).sum()
}

/// One mounted filesystem, from `df -Pk`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mount {
    pub filesystem: String,
    pub mount: String,
    pub size: u64,
    pub used: u64,
    pub free: u64,
}

/// `df -Pk` of `paths`, one entry per distinct filesystem, in order.
pub(crate) fn df(paths: &[&Path]) -> Result<Vec<Mount>> {
    let out = Command::new("df")
        .arg("-Pk")
        .args(paths)
        .output()
        .context("run df")?;
    let mut mounts = parse_df(&String::from_utf8_lossy(&out.stdout));
    let mut seen = std::collections::BTreeSet::new();
    mounts.retain(|m| seen.insert(m.mount.clone()));
    Ok(mounts)
}

/// Parse `df -Pk` output. The numbers are found from the capacity column
/// (`NN%`), so filesystem names and mount points may contain spaces.
fn parse_df(text: &str) -> Vec<Mount> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let p = (4..tokens.len()).find(|&p| {
                tokens[p].ends_with('%')
                    && tokens[p - 3..p].iter().all(|t| t.parse::<u64>().is_ok())
            })?;
            let kib = |t: &str| t.parse::<u64>().ok().map(|k| k * 1024);
            Some(Mount {
                filesystem: tokens[..p - 3].join(" "),
                mount: tokens[p + 1..].join(" "),
                size: kib(tokens[p - 3])?,
                used: kib(tokens[p - 2])?,
                free: kib(tokens[p - 1])?,
            })
        })
        .collect()
}

/// The largest use seen on one filesystem.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MountPeak {
    pub filesystem: String,
    pub mount: String,
    pub size: u64,
    pub peak_used: u64,
    pub min_free: u64,
    /// The step at the peak.
    pub at: String,
}

/// Disk samples of one run: `chain/disk.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct DiskLog {
    pub samples: usize,
    pub peaks: Vec<MountPeak>,
    /// The mount holding the work directory.
    pub work_mount: String,
}

impl DiskLog {
    fn record(&mut self, label: &str, mounts: &[Mount]) {
        self.samples += 1;
        for m in mounts {
            let i = match self.peaks.iter().position(|p| p.mount == m.mount) {
                Some(i) => i,
                None => {
                    self.peaks.push(MountPeak {
                        filesystem: m.filesystem.clone(),
                        mount: m.mount.clone(),
                        size: m.size,
                        min_free: u64::MAX,
                        ..MountPeak::default()
                    });
                    self.peaks.len() - 1
                }
            };
            let peak = &mut self.peaks[i];
            peak.size = m.size;
            peak.min_free = peak.min_free.min(m.free);
            if m.used >= peak.peak_used {
                peak.peak_used = m.used;
                peak.at = label.to_owned();
            }
        }
    }
}

/// Bytes as gigabytes with one decimal.
pub(crate) fn gb(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / 1e9)
}

/// One line per sample: `/ (/dev/root) 61.2G used, 10.8G free of 72.0G`.
fn describe(mounts: &[Mount]) -> String {
    mounts
        .iter()
        .map(|m| {
            format!(
                "{} ({}) {} used, {} free of {}",
                m.mount,
                m.filesystem,
                gb(m.used),
                gb(m.free),
                gb(m.size)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Samples disk use of `/` and the work directory, logs it, and keeps the
/// peaks in a JSON file (reloaded, so a resumed run keeps its peak).
/// Telemetry never fails the run: errors are logged.
pub(crate) struct Monitor {
    work: PathBuf,
    path: Option<PathBuf>,
    log: Mutex<DiskLog>,
}

impl Monitor {
    /// `path`: where to keep the peaks, or `None` to only log.
    pub(crate) fn new(work: &Path, path: Option<PathBuf>) -> Self {
        let log = path
            .as_deref()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            work: work.to_path_buf(),
            path,
            log: Mutex::new(log),
        }
    }

    pub(crate) fn sample(&self, label: &str) {
        let mounts = match df(&[Path::new("/"), &self.work]) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("[disk] {label}: {err:#}");
                return;
            }
        };
        let work_mount = df(&[&self.work])
            .ok()
            .and_then(|m| m.into_iter().next())
            .map(|m| m.mount)
            .unwrap_or_default();
        eprintln!(
            "[disk] {label}: {} (work dir on {work_mount})",
            describe(&mounts)
        );
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.work_mount = work_mount;
        log.record(label, &mounts);
        if let Some(path) = &self.path {
            let written = path
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| {
                    fs::write(path, serde_json::to_vec_pretty(&*log).unwrap_or_default())
                });
            if let Err(err) = written {
                eprintln!("[disk] write {}: {err}", path.display());
            }
        }
    }
}

/// Prune `target` after a step and log what it freed.
pub(crate) fn prune_and_log(label: &str, target: &Path, since: SystemTime) {
    let freed = prune_target(target, since, KEEP_GENERATIONS);
    if freed > 0 {
        eprintln!(
            "[disk] {label}: pruned {} from {}",
            gb(freed),
            target.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn touch(path: &Path, when: SystemTime) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, vec![0u8; 1000])?;
        fs::File::options()
            .write(true)
            .open(path)?
            .set_modified(when)
    }

    #[test]
    fn unit_keys_drop_the_hash() {
        let key = |n| unit_key(n).map(|(s, e)| format!("{s}|{e}"));
        assert_eq!(
            key("libcargo-5b07db85211ebf9f.rlib").as_deref(),
            Some("libcargo|.rlib")
        );
        assert_eq!(
            key("testsuite-137b88cd2e0c914c").as_deref(),
            Some("testsuite|")
        );
        assert_eq!(
            key("cargo-test-support-0bce4a41277184b9").as_deref(),
            Some("cargo-test-support|")
        );
        assert_eq!(key("cargo-2ktbmux74gpf3").as_deref(), Some("cargo|"));
        assert_eq!(
            key("serde-1.0.2").as_deref(),
            None,
            "a version is not a hash"
        );
        assert_eq!(key("Cargo-ABCDEF123456").as_deref(), None);
        assert_eq!(key("nohash"), None);
    }

    #[test]
    fn prunes_stale_generations_and_scratch_trees_only() -> TestResult {
        let root = std::env::temp_dir().join(format!("hord-m4-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let now = SystemTime::now();
        let hour = Duration::from_secs(3600);
        // The step's build started an hour from now: everything written
        // "now" predates it, and "fresh" files are written after it.
        let since = now + hour;
        let fresh = now + 2 * hour;
        let old = now - hour;
        let deps = root.join("debug/deps");
        // Three generations of the workspace crate; the newest is current.
        touch(&deps.join("libcargo-aaaaaaaaaaaaaaaa.rlib"), old)?;
        touch(&deps.join("libcargo-bbbbbbbbbbbbbbbb.rlib"), now)?;
        touch(&deps.join("libcargo-cccccccccccccccc.rlib"), fresh)?;
        // Three live versions of a dependency nothing rebuilt: kept.
        for h in ["1111111111111111", "2222222222222222", "3333333333333333"] {
            touch(&deps.join(format!("liblibc-{h}.rlib")), old)?;
        }
        // Incremental caches: a directory is as fresh as its sessions.
        let inc = root.join("debug/incremental");
        touch(&inc.join("cargo-2ktbmux74gpf3/s-old/x.bin"), old)?;
        touch(&inc.join("cargo-3prpncm85kvl8/s-old/x.bin"), old)?;
        touch(&inc.join("cargo-1dp1yeyyhavza/s-new"), fresh)?;
        // Cargo's per-test scratch projects.
        touch(
            &root.join("tmp/cit/testsuite/build/simple/target/a.out"),
            now,
        )?;

        let freed = prune_target(&root, since, 2);

        let exists = |p: &str| root.join(p).exists();
        assert!(!exists("debug/deps/libcargo-aaaaaaaaaaaaaaaa.rlib"));
        assert!(exists("debug/deps/libcargo-bbbbbbbbbbbbbbbb.rlib"));
        assert!(exists("debug/deps/libcargo-cccccccccccccccc.rlib"));
        assert!(exists("debug/deps/liblibc-1111111111111111.rlib"));
        assert!(exists("debug/deps/liblibc-2222222222222222.rlib"));
        assert!(exists("debug/deps/liblibc-3333333333333333.rlib"));
        assert!(exists("debug/incremental/cargo-1dp1yeyyhavza"));
        let incremental_left = read_dir(&inc).len();
        assert_eq!(incremental_left, 2, "one stale incremental cache removed");
        assert!(!exists("tmp/cit"));
        assert!(exists("tmp"), "CARGO_TARGET_TMPDIR itself stays");
        assert!(freed >= 3000, "freed {freed}");
        // Nothing new was built: a second prune removes nothing.
        assert_eq!(prune_target(&root, since, 2), 0);
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn restores_permissions_a_failed_read_only_test_left() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("hord-m4-perms-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cit = scratch_dir(&root);
        let home = cit.join("testsuite/registry/readonly/home/.cargo");
        let locked = home.join("registry/index/x/.cache/3/f");
        fs::create_dir_all(&locked)?;
        fs::write(locked.join("foo"), "cache")?;
        fs::write(home.join("config.toml"), "")?;
        // What the tests leave behind when they fail before restoring:
        // read-only files and directories, and a `000` directory.
        fs::set_permissions(home.join("config.toml"), fs::Permissions::from_mode(0o444))?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o555))?;
        restore_permissions(&cit);
        let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o700);
        assert_eq!(mode(&locked)?, 0o700);
        assert_eq!(mode(&home.join("config.toml"))?, 0o700);
        fs::remove_dir_all(&cit)?;
        assert!(!cit.exists());

        // Pruning restores permissions itself before removing the tree.
        fs::create_dir_all(&locked)?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        prune_target(&root, SystemTime::now(), KEEP_GENERATIONS);
        assert!(
            !cit.exists(),
            "prune removes a scratch tree with a 000 directory"
        );
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn parses_df_and_keeps_peaks() {
        let text = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                    /dev/root         75085112 52012345  23056767      70% /\n\
                    map auto_home            0        0         0     100% /System/Volumes/Data/home\n\
                    /dev/sdb1         76829444  4200000  68679444       6% /mnt\n";
        let mounts = parse_df(text);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].mount, "/");
        assert_eq!(mounts[0].used, 52_012_345 * 1024);
        assert_eq!(mounts[1].filesystem, "map auto_home");
        assert_eq!(mounts[2].mount, "/mnt");
        let mut log = DiskLog::default();
        log.record("start", &mounts[..1]);
        let mut fuller = mounts[0].clone();
        fuller.used += 10;
        fuller.free -= 10;
        log.record("lander 3", &[fuller.clone()]);
        log.record("lander 4", &mounts[..1]);
        assert_eq!(log.samples, 3);
        assert_eq!(log.peaks.len(), 1);
        assert_eq!(log.peaks[0].peak_used, fuller.used);
        assert_eq!(log.peaks[0].min_free, fuller.free);
        assert_eq!(log.peaks[0].at, "lander 3");
    }
}
