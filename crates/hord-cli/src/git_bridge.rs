//! Git import/export via [`hord_git`] (spec §9).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use hord_api::proto::{GitExportResult, GitImportResult};
use hord_core::{ChangeId, ObjectId, SnapshotId};
use hord_store::Store;

use crate::repo;

/// Whether `path` holds a git worktree or is a bare repository.
fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists() || (path.join("HEAD").exists() && path.join("objects").exists())
}

/// Fail if the git path is not a repository.
pub fn ensure_git_repo(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("git path does not exist: {}", path.display());
    }
    if !is_git_repo(path) {
        bail!("not a git repository: {}", path.display());
    }
    Ok(())
}

/// Import git history at `git_path` into `store`.
///
/// `git_ref == None` means `HEAD` (used by `hord init --from-git`).
pub fn import_git(
    store: &mut Store,
    git_path: &Path,
    git_ref: Option<&str>,
) -> Result<GitImportResult> {
    let git_path_disp = abs_display(git_path)?;
    ensure_git_repo(Path::new(&git_path_disp))?;
    let head = match git_ref {
        None | Some("HEAD") | Some("head") => hord_git::import_git(store, &git_path_disp)?,
        Some(git_ref) => hord_git::import_git_ref(store, &git_path_disp, git_ref)?,
    };
    Ok(GitImportResult {
        git_path: git_path_disp,
        git_ref: git_ref.map(str::to_owned),
        head: Some(head.to_hex()),
        changes: store.log()?.len() as u64,
    })
}

/// Export `hord_ref` from `store` into the git repository at `git_path`.
pub fn export_tree(store: &Store, hord_ref: &str, git_path: &Path) -> Result<GitExportResult> {
    let git_path_disp = abs_display(git_path)?;
    ensure_git_repo(Path::new(&git_path_disp))?;
    match resolve_export_target(store, hord_ref)? {
        ExportTarget::Change(change) => {
            let commit = hord_git::export_change(store, change, &git_path_disp)?;
            Ok(GitExportResult {
                r#ref: hord_ref.to_owned(),
                git_path: git_path_disp,
                git_tree: None,
                git_commit: Some(commit.to_hex()),
            })
        }
        ExportTarget::Tree(snapshot) => {
            let tree = hord_git::export_tree(store, snapshot, &git_path_disp)?;
            Ok(GitExportResult {
                r#ref: hord_ref.to_owned(),
                git_path: git_path_disp,
                git_tree: Some(tree.to_hex()),
                git_commit: None,
            })
        }
    }
}

enum ExportTarget {
    Change(ChangeId),
    Tree(SnapshotId),
}

fn resolve_export_target(store: &Store, hord_ref: &str) -> Result<ExportTarget> {
    if hord_ref == "head" || hord_ref == "HEAD" {
        let id = store
            .head()?
            .ok_or_else(|| anyhow!("empty log; nothing to export"))?;
        return Ok(ExportTarget::Change(id));
    }
    if let Ok(id) = hord_ref.parse::<ObjectId>() {
        return classify_id(store, id);
    }
    match store.get_ref(hord_ref)? {
        Some(id) => classify_id(store, id),
        None => bail!("unknown hord ref {hord_ref:?}"),
    }
}

fn classify_id(store: &Store, id: ObjectId) -> Result<ExportTarget> {
    match repo::try_change(store, id)? {
        Some(_) => Ok(ExportTarget::Change(id)),
        None => Ok(ExportTarget::Tree(id)),
    }
}

fn abs_display(path: &Path) -> Result<String> {
    let abs = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    Ok(abs.display().to_string())
}

/// Git repository sitting next to `.hord/`, or `cwd` if that is a git repo.
pub fn sibling_git(store: &Store) -> Result<PathBuf> {
    let root = store.repo_root();
    if is_git_repo(root) {
        return Ok(root.to_path_buf());
    }
    let cwd = std::env::current_dir().context("current directory")?;
    if is_git_repo(&cwd) {
        return Ok(cwd);
    }
    bail!(
        "no git repository next to {} or in the current directory",
        store.hord_dir().display()
    )
}
