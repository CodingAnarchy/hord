//! Discover and open a local [`hord_store::Store`], plus CLI-level resolution
//! of `--base` and `-w`.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use hord_core::{ChangeRecord, IdentityTree, ObjectId, Snapshot, SnapshotId, Tree};
use hord_store::{Store, WorkspaceId, WorkspaceMeta};

const CURRENT_WORKSPACE: &str = "current-workspace";

/// Create `<cwd>/.hord/` via [`Store::create`].
pub fn create(repo_root: &Path) -> Result<Store> {
    Store::create(repo_root).map_err(Into::into)
}

/// The repository root: the nearest directory, from the current one up,
/// that holds `.hord/`. Does not open the store.
pub fn discover_root() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("current directory")?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join(hord_store::HORD_DIR).is_dir() {
            return Ok(dir.to_path_buf());
        }
        dir = dir
            .parent()
            .ok_or_else(|| anyhow!("no .hord directory found; run `hord init`"))?;
    }
}

/// Walk from the current directory toward the filesystem root looking for `.hord/`.
pub fn discover() -> Result<Store> {
    let cwd = std::env::current_dir().context("current directory")?;
    let mut dir = cwd.as_path();
    loop {
        let hord_dir = dir.join(hord_store::HORD_DIR);
        if hord_dir.is_dir() {
            return Store::open(dir).map_err(Into::into);
        }
        dir = dir
            .parent()
            .ok_or_else(|| anyhow!("no .hord directory found; run `hord init`"))?;
    }
}

/// Resolve `--base <snap|ref>` to a snapshot id (a [`Snapshot`] object's
/// id, ADR 0017).
///
/// With no spec (or `head`), uses the result snapshot of `head`, or the
/// empty snapshot if nothing has landed.
pub fn resolve_base(store: &Store, spec: Option<&str>) -> Result<SnapshotId> {
    match spec {
        None | Some("head") | Some("HEAD") => match store.head()? {
            Some(change) => snapshot_of(store, change),
            None => empty_snapshot(store),
        },
        Some(spec) => {
            if let Ok(id) = spec.parse::<ObjectId>() {
                return snapshot_of(store, id);
            }
            match store.get_ref(spec)? {
                Some(id) => snapshot_of(store, id),
                None => bail!("unknown snapshot or ref {spec:?}"),
            }
        }
    }
}

/// Workspace selected by `-w`, else the current workspace, else the only one.
pub fn resolve_workspace(store: &Store, id: Option<&str>) -> Result<WorkspaceMeta> {
    if let Some(id) = id {
        let id: WorkspaceId = id
            .parse()
            .with_context(|| format!("invalid workspace id {id:?}"))?;
        return store
            .get_workspace(id)?
            .ok_or_else(|| anyhow!("unknown workspace {id}"));
    }
    if let Some(current) = current_workspace_id(store)? {
        return store
            .get_workspace(current)?
            .ok_or_else(|| anyhow!("current workspace {current} is missing; pass `-w <id>`"));
    }
    let mut list = store.list_workspaces()?;
    match list.len() {
        0 => bail!("no workspace; run `hord ws new`"),
        1 => Ok(list.remove(0)),
        n => bail!("workspace is ambiguous ({n} exist); pass `-w <id>`"),
    }
}

/// Remember `id` as the default workspace for `hord status` without `-w`.
pub fn set_current_workspace(store: &Store, id: WorkspaceId) -> Result<()> {
    fs::write(
        store.hord_dir().join(CURRENT_WORKSPACE),
        id.to_string().as_bytes(),
    )
    .context("write current-workspace")?;
    Ok(())
}

pub fn current_workspace_id(store: &Store) -> Result<Option<WorkspaceId>> {
    let path = store.hord_dir().join(CURRENT_WORKSPACE);
    match fs::read_to_string(&path) {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() {
                Ok(None)
            } else {
                s.parse()
                    .map(Some)
                    .with_context(|| format!("invalid workspace id in {}", path.display()))
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

/// Decode `id` as a [`ChangeRecord`], or `None` if it is missing or another kind.
pub fn try_change(store: &Store, id: ObjectId) -> Result<Option<ChangeRecord>> {
    match store.get_object::<ChangeRecord>(id) {
        Ok(change) => Ok(Some(change)),
        Err(hord_store::Error::MissingObject(_)) | Err(hord_store::Error::Encoding(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn snapshot_of(store: &Store, id: ObjectId) -> Result<SnapshotId> {
    Ok(try_change(store, id)?.map(|c| c.result).unwrap_or(id))
}

fn empty_snapshot(store: &Store) -> Result<SnapshotId> {
    store.put_object(&Tree::default())?;
    store.put_object(&IdentityTree::default())?;
    store.put_object(&Snapshot::empty()).map_err(Into::into)
}
