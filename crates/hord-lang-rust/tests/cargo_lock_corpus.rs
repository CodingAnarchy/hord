//! Optional `Cargo.lock` checks against the cargo.git M0 corpus.
//!
//! Requires the bare clone at `~/.cache/hord/corpora/cargo.git`; skipped if
//! absent. Ignored by default (slow); run with
//! `cargo test --release -p hord-lang-rust --test cargo_lock_corpus -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::process::Command;

use hord_lang::LangAdapter;
use hord_lang_rust::{CargoLockAdapter, CargoLockMergeError, merge_cargo_lock};

const CARGO_GIT: &str = ".cache/hord/corpora/cargo.git";

fn cargo_git() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("HOME")?).join(CARGO_GIT);
    path.exists().then_some(path)
}

fn git(git_dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn lock_at(git_dir: &Path, rev: &str) -> Option<Vec<u8>> {
    git(git_dir, &["show", &format!("{rev}:Cargo.lock")])
}

/// Committed lockfiles Cargo did not write: a hand-resolved merge spells
/// `home 0.5.5` both with and without its source. Cargo (and the merge)
/// re-spell it the short way.
const NON_CANONICAL: &[&str] = &["2f06c80bd02c655730a05890f32b5281b8c9eccd"];

/// Lockfiles Cargo wrote in the current format (`version = N` header).
fn is_modern(bytes: &[u8]) -> bool {
    bytes
        .split(|b| *b == b'\n')
        .take(4)
        .any(|l| l.starts_with(b"version = ") && !l.contains(&b'"'))
}

#[test]
#[ignore = "walks cargo.git history; run with --ignored when the corpus is present"]
fn every_modern_lockfile_is_lossless_and_reemits_byte_identical() {
    let Some(git_dir) = cargo_git() else {
        return;
    };
    let revs = git(&git_dir, &["log", "--format=%H", "--", "Cargo.lock"]).expect("log");
    let mut checked = 0usize;
    for rev in String::from_utf8_lossy(&revs).lines() {
        let Some(bytes) = lock_at(&git_dir, rev) else {
            continue;
        };
        if !is_modern(&bytes) || NON_CANONICAL.contains(&rev) {
            continue;
        }
        let tree = CargoLockAdapter
            .parse(&bytes)
            .unwrap_or_else(|e| panic!("parse {rev}: {e}"));
        assert_eq!(
            CargoLockAdapter.project(&tree).as_slice(),
            bytes,
            "lossless {rev}"
        );
        let merged =
            merge_cargo_lock(&bytes, &bytes, &bytes).unwrap_or_else(|e| panic!("merge {rev}: {e}"));
        assert!(merged == bytes, "re-emission differs at {rev}");
        checked += 1;
    }
    println!("checked {checked} lockfiles");
    assert!(checked > 0);
}

/// Replays every two-parent merge commit that touched `Cargo.lock` where both
/// parents changed it relative to the merge base. A clean result must equal
/// the committed lockfile; conflicts are counted, not failed.
#[test]
#[ignore = "walks cargo.git history; run with --ignored when the corpus is present"]
fn real_merge_commits_reproduce_committed_lockfile() {
    let Some(git_dir) = cargo_git() else {
        return;
    };
    let merges = git(
        &git_dir,
        &[
            "rev-list",
            "--merges",
            "--parents",
            "HEAD",
            "--",
            "Cargo.lock",
        ],
    )
    .expect("rev-list");
    let (mut clean, mut conflicted, mut mismatched) = (0usize, 0usize, Vec::new());
    for line in String::from_utf8_lossy(&merges).lines() {
        let ids: Vec<&str> = line.split_whitespace().collect();
        let [merge, p1, p2] = ids[..] else {
            continue;
        };
        let Some(base_rev) = git(&git_dir, &["merge-base", p1, p2]) else {
            continue;
        };
        let base_rev = String::from_utf8_lossy(&base_rev).trim().to_owned();
        let (Some(base), Some(ours), Some(theirs), Some(want)) = (
            lock_at(&git_dir, &base_rev),
            lock_at(&git_dir, p1),
            lock_at(&git_dir, p2),
            lock_at(&git_dir, merge),
        ) else {
            continue;
        };
        if ![&base, &ours, &theirs, &want].iter().all(|b| is_modern(b))
            || base == ours
            || base == theirs
            || ours == theirs
        {
            continue;
        }
        match merge_cargo_lock(&base, &ours, &theirs) {
            Ok(got) => {
                let flipped = merge_cargo_lock(&base, &theirs, &ours).expect("commutative");
                assert!(got == flipped, "not commutative at {merge}");
                if got == want {
                    clean += 1;
                } else {
                    mismatched.push(merge.to_owned());
                }
            }
            Err(CargoLockMergeError::Conflict(c)) => {
                println!("{merge}: conflict {c:?}");
                conflicted += 1;
            }
            Err(e) => panic!("{merge}: {e}"),
        }
    }
    println!(
        "clean and equal: {clean}, conflicted: {conflicted}, clean but different: {}",
        mismatched.len()
    );
    for m in &mismatched {
        println!("  differs: {m}");
    }
    assert!(
        mismatched
            .iter()
            .all(|m| NON_CANONICAL.contains(&m.as_str())),
        "clean merges must reproduce Cargo's bytes"
    );
}
