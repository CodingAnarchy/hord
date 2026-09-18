//! Git import/export via [`hord_git`] (spec §9).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use hord_core::{ChangeId, ChangeRecord, ObjectId, SnapshotId};
use hord_store::Store;
use serde::Serialize;

/// Result of importing git history into a hord store.
#[derive(Clone, Debug, Serialize)]
pub struct ImportReport {
    /// Git repository that was imported.
    pub git_path: String,
    /// Git ref imported, if the import was ref-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Resulting hord head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Number of change records in the log after import.
    pub changes: u64,
}

/// Result of exporting a hord ref to a git tree or commit.
#[derive(Clone, Debug, Serialize)]
pub struct ExportReport {
    /// Hord ref or snapshot that was exported.
    #[serde(rename = "ref")]
    pub hord_ref: String,
    /// Destination git repository.
    pub git_path: String,
    /// Exported git tree SHA, if the exporter produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_tree: Option<String>,
    /// Exported git commit SHA, if the exporter produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
}

/// Adapter so [`hord_store::Store`] can be passed to [`hord_git`] (orphan rule).
struct GitAdapter<'a>(&'a Store);

impl hord_git::Store for GitAdapter<'_> {
    fn put(&mut self, id: ObjectId, bytes: Vec<u8>) -> hord_git::Result<()> {
        let got = self.0.put(&bytes).map_err(store_err)?;
        if got != id {
            return Err(hord_git::Error::Git(format!(
                "object id mismatch: store computed {got}, importer supplied {id}"
            )));
        }
        Ok(())
    }

    fn get(&self, id: ObjectId) -> hord_git::Result<Vec<u8>> {
        self.0.get(id).map_err(store_err)
    }

    fn append_log(&mut self, change: ChangeId) -> hord_git::Result<()> {
        self.0.append_log(change).map_err(store_err)
    }

    fn log(&self) -> hord_git::Result<Vec<ChangeId>> {
        self.0.log().map_err(store_err)
    }

    fn set_head(&mut self, change: ChangeId) -> hord_git::Result<()> {
        self.0.set_head(change).map_err(store_err)
    }

    fn head(&self) -> hord_git::Result<Option<ChangeId>> {
        self.0.head().map_err(store_err)
    }

    fn set_ref(&mut self, name: &str, id: ObjectId) -> hord_git::Result<()> {
        self.0.set_ref(name, id).map_err(store_err)
    }

    fn get_ref(&self, name: &str) -> hord_git::Result<Option<ObjectId>> {
        self.0.get_ref(name).map_err(store_err)
    }
}

fn store_err(err: hord_store::Error) -> hord_git::Error {
    match err {
        hord_store::Error::MissingObject(id) => hord_git::Error::Missing(id),
        hord_store::Error::Encoding(e) => hord_git::Error::Encoding(e),
        other => hord_git::Error::Git(other.to_string()),
    }
}

/// Fail if the git path is not a repository.
pub fn ensure_git_repo(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("git path does not exist: {}", path.display());
    }
    let worktree = path.join(".git").exists();
    let bare = path.join("HEAD").exists() && path.join("objects").exists();
    if !worktree && !bare {
        bail!("not a git repository: {}", path.display());
    }
    Ok(())
}

/// Import git history at `git_path` into `store`.
///
/// `git_ref == None` means `HEAD` (used by `hord init --from-git`).
pub fn import_git(store: &Store, git_path: &Path, git_ref: Option<&str>) -> Result<ImportReport> {
    let git_path_disp = abs_display(git_path)?;
    ensure_git_repo(Path::new(&git_path_disp))?;
    let mut adapter = GitAdapter(store);
    let head = match git_ref {
        None | Some("HEAD") | Some("head") => hord_git::import_git(&mut adapter, &git_path_disp)?,
        Some(git_ref) => hord_git::import_git_ref(&mut adapter, &git_path_disp, git_ref)?,
    };
    Ok(ImportReport {
        git_path: git_path_disp,
        git_ref: git_ref.map(str::to_owned),
        head: Some(head.to_hex()),
        changes: store.log()?.len() as u64,
    })
}

/// Export `hord_ref` from `store` into the git repository at `git_path`.
pub fn export_tree(store: &Store, hord_ref: &str, git_path: &Path) -> Result<ExportReport> {
    let git_path_disp = abs_display(git_path)?;
    ensure_git_repo(Path::new(&git_path_disp))?;
    let adapter = GitAdapter(store);
    match resolve_export_target(store, hord_ref)? {
        ExportTarget::Change(change) => {
            let commit = hord_git::export_change(&adapter, change, &git_path_disp)?;
            Ok(ExportReport {
                hord_ref: hord_ref.to_owned(),
                git_path: git_path_disp,
                git_tree: None,
                git_commit: Some(commit.to_hex()),
            })
        }
        ExportTarget::Tree(snapshot) => {
            let tree = hord_git::export_tree(&adapter, snapshot, &git_path_disp)?;
            Ok(ExportReport {
                hord_ref: hord_ref.to_owned(),
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
    match store.get_object::<ChangeRecord>(id) {
        Ok(_) => Ok(ExportTarget::Change(id)),
        Err(hord_store::Error::MissingObject(_)) | Err(hord_store::Error::Encoding(_)) => {
            Ok(ExportTarget::Tree(id))
        }
        Err(err) => Err(err.into()),
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
    if root.join(".git").exists() || (root.join("HEAD").exists() && root.join("objects").exists()) {
        return Ok(root.to_path_buf());
    }
    let cwd = std::env::current_dir().context("current directory")?;
    if cwd.join(".git").exists() || (cwd.join("HEAD").exists() && cwd.join("objects").exists()) {
        return Ok(cwd);
    }
    bail!(
        "no git repository next to {} or in the current directory",
        store.hord_dir().display()
    )
}
