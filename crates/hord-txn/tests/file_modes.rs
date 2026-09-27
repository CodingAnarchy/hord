//! File modes through workspaces and the lander (ADR 0042): exec bits read
//! back and checked out, a mode-only edit, a mode change merged with a
//! concurrent content edit, and a checkout that fails leaving nothing.

mod common;

use std::fs;

use common::*;
use hord_core::{Blob, ObjectId};
use hord_txn::BeginOptions;
// The mode tests need exec bits on disk, so only run on Unix.
#[cfg(unix)]
use hord_core::{FileEntry, FileMode, Op, Snapshot, Tree, TreeEntry};
#[cfg(unix)]
use hord_txn::{Materialization, QueueStatus, Repo, Workspace};

#[cfg(unix)]
fn checkout_of(ws: &Workspace) -> TestResult<std::path::PathBuf> {
    match ws.materialization() {
        Materialization::Directory { path } => Ok(path.clone()),
        Materialization::InMemory => Err("expected a directory workspace".into()),
    }
}

/// The mode and bytes of `file` in head's snapshot.
#[cfg(unix)]
async fn head_file(repo: &Repo, file: &str) -> TestResult<(FileMode, Vec<u8>)> {
    let store = repo.store();
    let snapshot: Snapshot = store.get_object(repo.head().await?.snapshot)?;
    let mut tree: Tree = store.get_object(snapshot.tree)?;
    let path = path(file);
    let (last, dirs) = path.components().split_last().ok_or("a file path")?;
    for dir in dirs {
        match tree.entries.get(dir) {
            Some(TreeEntry::Tree(id)) => tree = store.get_object(*id)?,
            other => return Err(format!("{dir} is {other:?}").into()),
        }
    }
    let Some(TreeEntry::Blob(id)) = tree.entries.get(last) else {
        return Err(format!("no file at {file}").into());
    };
    match FileEntry::decode(&store.get(*id)?)? {
        FileEntry::Blob(blob) => Ok((FileMode::Regular, blob.bytes.as_slice().to_vec())),
        FileEntry::Moded(leaf) => {
            let blob: Blob = store.get_object(leaf.blob)?;
            let mode = leaf.file_mode().ok_or("a known mode")?;
            Ok((mode, blob.bytes.as_slice().to_vec()))
        }
    }
}

#[cfg(unix)]
fn set_exec(path: &std::path::Path, exec: bool) -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(unix)]
fn is_exec(path: &std::path::Path) -> TestResult<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o100 != 0)
}

#[cfg(unix)]
fn landed(repo_entries: &[hord_txn::QueueEntry]) -> bool {
    repo_entries
        .iter()
        .all(|entry| matches!(entry.status, QueueStatus::Landed { .. }))
}

/// `chmod +x` on a parsed file keeps its size and mtime and has no
/// structural diff: the stat index still sees it, and it is a `Blob` op.
#[cfg(unix)]
#[tokio::test]
async fn a_mode_only_edit_is_proposed_and_checked_out() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await?;
    let dir = checkout_of(&ws)?;
    assert!(!is_exec(&dir.join("src/lib.rs"))?);
    set_exec(&dir.join("src/lib.rs"), true)?;
    let proposal = ws.propose(intent("chmod")).await?;
    let blob_ops: Vec<String> = proposal
        .record
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Blob { path, .. } => Some(path.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(blob_ops, ["src/lib.rs"], "{:?}", proposal.record.ops);
    t.repo.submit(proposal.change).await?;
    assert!(landed(&t.repo.land_local().await?));
    assert_eq!(
        head_file(&t.repo, "src/lib.rs").await?,
        (FileMode::Executable, LIB.as_bytes().to_vec())
    );

    // A new checkout carries the bit, and has nothing to propose.
    let mut after = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("b")))
        .await?;
    let after_dir = checkout_of(&after)?;
    assert!(is_exec(&after_dir.join("src/lib.rs"))?);
    assert!(!is_exec(&after_dir.join("README.md"))?);
    assert!(matches!(
        after.preview(intent("nothing")).await,
        Err(hord_txn::Error::NothingToPropose)
    ));
    // Its definitions still parse and read.
    assert!(!after.definitions(&path("src/lib.rs")).await?.is_empty());
    Ok(())
}

/// One change sets the exec bit, another (in memory, which keeps the base's
/// mode) edits the content: the rebased result has both.
#[cfg(unix)]
#[tokio::test]
async fn a_mode_change_merges_with_a_concurrent_edit() -> TestResult {
    let t = stub_repo(&fixture()).await?;
    let mut chmod = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await?;
    set_exec(&checkout_of(&chmod)?.join("README.md"), true)?;
    let mut edit_ws = begin(&t.repo, "b").await?;
    edit(&mut edit_ws, "README.md", README, "two", "2").await?;
    submit(&t.repo, &mut chmod, "chmod").await?;
    submit(&t.repo, &mut edit_ws, "edit").await?;
    assert!(landed(&t.repo.land_local().await?));
    assert_eq!(
        head_file(&t.repo, "README.md").await?,
        (
            FileMode::Executable,
            README.replace("two", "2").into_bytes()
        )
    );

    // An in-memory write keeps the exec bit it found.
    let mut again = begin(&t.repo, "c").await?;
    edit(
        &mut again,
        "README.md",
        &README.replace("two", "2"),
        "three",
        "3",
    )
    .await?;
    submit(&t.repo, &mut again, "again").await?;
    assert!(landed(&t.repo.land_local().await?));
    assert_eq!(
        head_file(&t.repo, "README.md").await?.0,
        FileMode::Executable
    );
    Ok(())
}

/// A checkout that cannot be written leaves no workspace row, directory,
/// stat index, or partial pristine behind.
#[tokio::test]
async fn a_failed_checkout_leaves_no_workspace_behind() -> TestResult {
    let t = repo(&fixture()).await?;
    let readme = ObjectId::of(&Blob::new(README.as_bytes().to_vec()))?;
    remove_loose_object(&t.path, readme)?;
    let failed = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await;
    assert!(failed.is_err(), "the checkout needs the lost blob");
    assert!(t.repo.store().list_workspaces()?.is_empty());
    let hord = t.repo.store().hord_dir();
    for sub in ["ws", "pristine"] {
        let left: Vec<_> = match fs::read_dir(hord.join(sub)) {
            Ok(entries) => entries
                .map(|entry| entry.map(|e| e.file_name()))
                .collect::<Result<_, _>>()?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err.into()),
        };
        assert!(left.is_empty(), "left in .hord/{sub}: {left:?}");
    }
    Ok(())
}
