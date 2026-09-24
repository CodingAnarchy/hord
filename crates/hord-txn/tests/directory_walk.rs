//! What a `Directory` propose walks: ignore rules, symlinks, and racily
//! clean stat index entries (ADR 0016; M3 review quality #3, #4, #8).

mod common;

use std::fs;

use common::*;
use hord_core::{Op, RepoPath};
use hord_txn::{BeginOptions, Materialization, MaterializeMode, Workspace};

fn checkout_of(ws: &Workspace) -> std::path::PathBuf {
    match ws.materialization() {
        Materialization::Directory { path } => path.clone(),
        Materialization::InMemory => panic!("directory workspace"),
    }
}

/// Paths of the blob ops of a proposal.
fn blob_paths(ops: &[Op]) -> Vec<String> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Blob { path, .. } => Some(path.to_string()),
            _ => None,
        })
        .collect()
}

fn strings(paths: &[RepoPath]) -> Vec<String> {
    paths.iter().map(ToString::to_string).collect()
}

fn write(dir: &std::path::Path, rel: &str, bytes: &[u8]) {
    let target = dir.join(rel);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, bytes).unwrap();
}

const GITIGNORE: &str = "/target\n.DS_Store\n*.log\n!keep.log\n/build/\n# comment\n[\n";

fn ignoring_fixture() -> Vec<(&'static str, &'static str)> {
    let mut files = fixture();
    files.extend([
        (".gitignore", GITIGNORE),
        // Tracked, though `*.log` matches it: stays tracked, as in git.
        ("tracked.log", "old\n"),
        // Tracked inside an ignored directory.
        ("build/keep.txt", "keep\n"),
        ("docs/.gitignore", "!local.log\ngen/\n"),
    ]);
    files
}

#[tokio::test]
async fn propose_skips_ignored_untracked_files_and_keeps_tracked_ones() {
    let t = repo(&ignoring_fixture()).await;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await
        .unwrap();
    let dir = checkout_of(&ws);
    fs::write(dir.join("README.md"), README.replace("two", "2")).unwrap();
    // Build output and editor droppings.
    write(&dir, "target/debug/fixture", &[0, 159, 146, 150, 0, 1]);
    write(&dir, "target/.rustc_info.json", b"{}");
    write(&dir, ".DS_Store", &[0; 8]);
    write(&dir, "src/.DS_Store", &[0; 8]);
    write(&dir, "run.log", b"log\n");
    write(&dir, "build/out.o", b"obj");
    write(&dir, "docs/gen/page.html", b"<p>");
    write(&dir, ".hord/stray", b"x");
    write(&dir, "nested/.git/HEAD", b"ref: refs/heads/main\n");
    // Not ignored: a negated pattern, a deeper `.gitignore` re-including a
    // file, and a tracked file that matches a pattern.
    write(&dir, "keep.log", b"kept\n");
    write(&dir, "docs/local.log", b"local\n");
    write(&dir, "tracked.log", b"new\n");
    write(&dir, "build/keep.txt", b"changed\n");
    write(&dir, "notes.txt", b"notes\n");

    let proposal = ws.propose(intent("edit + build")).await.unwrap();
    assert_eq!(
        blob_paths(&proposal.record.ops),
        [
            "README.md",
            "build/keep.txt",
            "docs/local.log",
            "keep.log",
            "notes.txt",
            "tracked.log",
        ]
    );
    assert_eq!(
        strings(ws.skipped()),
        [
            ".DS_Store",
            ".hord",
            "build/out.o",
            "docs/gen",
            "nested/.git",
            "run.log",
            "src/.DS_Store",
            "target",
        ]
    );
    let listed = strings(&ws.list_files().await.unwrap());
    assert!(listed.contains(&"tracked.log".to_owned()));
    assert!(listed.contains(&"build/keep.txt".to_owned()));
    assert!(
        !listed.iter().any(|p| p.starts_with("target")),
        "{listed:?}"
    );
    assert!(!listed.contains(&"run.log".to_owned()));
}

#[tokio::test]
async fn build_output_is_proposed_when_no_ignore_rule_names_it() {
    // Nothing language-specific is built in: `target/` is ordinary without
    // a `.gitignore` that says otherwise.
    let t = repo(&fixture()).await;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await
        .unwrap();
    let dir = checkout_of(&ws);
    write(&dir, "target/out.txt", b"out\n");
    write(&dir, ".hord/stray", b"x");
    let proposal = ws.propose(intent("target")).await.unwrap();
    assert_eq!(blob_paths(&proposal.record.ops), ["target/out.txt"]);
    assert_eq!(strings(ws.skipped()), [".hord"]);
}

#[tokio::test]
async fn ignored_files_alone_are_nothing_to_propose() {
    let t = repo(&ignoring_fixture()).await;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await
        .unwrap();
    write(&checkout_of(&ws), "target/debug/x", b"x");
    assert!(matches!(
        ws.preview(intent("x")).await,
        Err(hord_txn::Error::NothingToPropose)
    ));
    assert_eq!(strings(ws.skipped()), ["target"]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_tracked_file_replaced_by_a_symlink_is_an_error_not_a_deletion() {
    let t = repo(&fixture()).await;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await
        .unwrap();
    let dir = checkout_of(&ws);
    fs::rename(dir.join("README.md"), dir.join("docs-README.md")).unwrap();
    std::os::unix::fs::symlink("docs-README.md", dir.join("README.md")).unwrap();
    match ws.propose(intent("symlink")).await {
        Err(hord_txn::Error::UnsupportedEntry { path, kind }) => {
            assert_eq!(path.to_string(), "README.md");
            assert_eq!(kind, "symlink");
        }
        other => panic!("expected UnsupportedEntry, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_tracked_directory_replaced_by_a_symlink_is_an_error() {
    let t = repo(&fixture()).await;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await
        .unwrap();
    let dir = checkout_of(&ws);
    fs::rename(dir.join("src"), dir.join("src2")).unwrap();
    std::os::unix::fs::symlink("src2", dir.join("src")).unwrap();
    match ws.preview(intent("symlink dir")).await {
        Err(hord_txn::Error::UnsupportedEntry { path, kind }) => {
            assert_eq!(path.to_string(), "src");
            assert_eq!(kind, "symlink");
        }
        other => panic!("expected UnsupportedEntry, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_untracked_symlink_is_skipped_and_reported() {
    let t = repo(&fixture()).await;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await
        .unwrap();
    let dir = checkout_of(&ws);
    std::os::unix::fs::symlink("README.md", dir.join("link.md")).unwrap();
    fs::write(dir.join("README.md"), README.replace("two", "2")).unwrap();
    let proposal = ws.propose(intent("edit")).await.unwrap();
    assert_eq!(blob_paths(&proposal.record.ops), ["README.md"]);
    assert_eq!(strings(ws.skipped()), ["link.md"]);
}

/// Git's racily clean rule: an edit that keeps the size and lands in the
/// same timestamp tick as the index was written is not trusted to the stat
/// index. Simulated by giving the index file the file's recorded mtime.
#[tokio::test]
async fn racily_clean_entries_are_rehashed() {
    for mode in [MaterializeMode::Clone, MaterializeMode::Copy] {
        let t = repo(&fixture()).await;
        let mut ws = t
            .repo
            .begin_directory_with(BeginOptions::at_head(actor("a")), mode)
            .await
            .unwrap();
        let dir = checkout_of(&ws);
        let lib = dir.join("README.md");
        let recorded = fs::metadata(&lib).unwrap().modified().unwrap();
        let sneaky = README.replace("one", "1-1");
        assert_eq!(sneaky.len(), README.len());
        fs::write(&lib, &sneaky).unwrap();
        let set = |path: &std::path::Path| {
            let file = fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_modified(recorded).unwrap();
        };
        set(&lib);
        let mut stat_file = dir.clone().into_os_string();
        stat_file.push(".stat");
        set(std::path::Path::new(&stat_file));
        let found = ws.preview(intent("x")).await.unwrap();
        assert_eq!(blob_paths(&found.record.ops), ["README.md"], "{mode:?}");
    }
}

/// Fresh checkouts are backdated so that their index entries are not racily
/// clean: an untouched workspace reads no file at propose. Observed through
/// a file the walk would fail to read.
#[cfg(unix)]
#[tokio::test]
async fn fresh_checkout_entries_are_trusted() {
    use std::os::unix::fs::PermissionsExt;
    for mode in [MaterializeMode::Clone, MaterializeMode::Copy] {
        let t = repo(&fixture()).await;
        let mut ws = t
            .repo
            .begin_directory_with(BeginOptions::at_head(actor("a")), mode)
            .await
            .unwrap();
        let other = checkout_of(&ws).join("src/other.rs");
        let before = fs::metadata(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(fs::metadata(&other).unwrap().len(), before.len());
        let result = ws.preview(intent("x")).await;
        fs::set_permissions(&other, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(result, Err(hord_txn::Error::NothingToPropose)),
            "{mode:?}: {result:?}"
        );
    }
}
