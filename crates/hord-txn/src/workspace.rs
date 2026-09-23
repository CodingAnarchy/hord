//! [`Workspace`]: a snapshot pointer, an overlay, and an access log (spec §6.1).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hord_core::{
    Actor, Bytes, ChangeId, ChangeRecord, Intent, NodeId, ObjectId, RepoPath, SnapshotId,
};
use hord_lang::IdentifiedTree;
use hord_store::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::materialize::{MaterializeMode, Stat, load_stat_index, read_checkout_file, walk_files};
use crate::propose::{ProposeInput, ReadDeclaration};
use crate::repo::{Inner, Repo, blocking, fs_path};
use crate::semantic::{DefinitionInfo, definitions};
use crate::snapshot::blob_object_id;
use crate::{Error, Result};

/// How a workspace's files are presented (spec §6.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Materialization {
    /// Files live in the workspace overlay and are reached only through the
    /// library API. Every read is observed.
    InMemory,
    /// Files are checked out under `path` (`.hord/ws/<id>/`). The API reads
    /// and writes that directory. Reads by other tools are not observed.
    Directory {
        /// Absolute path of the checkout.
        path: PathBuf,
    },
}

/// Reads and writes a workspace has made (spec §6.1, ADR 0012).
///
/// Reads are at definition granularity: reading a definition records its
/// [`NodeId`] (and those of definitions nested in it); reading a file or a
/// byte range records every definition the read overlaps plus the path.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccessLog {
    /// Definitions whose content was read.
    pub reads: BTreeSet<NodeId>,
    /// Files that were read in whole or in part.
    pub read_paths: BTreeSet<RepoPath>,
    /// Definitions written through [`Workspace::write_definition`].
    pub writes: BTreeSet<NodeId>,
    /// Files written or deleted (for a `Directory` workspace, also those
    /// found changed at propose time).
    pub written_paths: BTreeSet<RepoPath>,
    /// Whether every read was observed. `false` for a `Directory`
    /// workspace: tools other than this API read the checkout unseen.
    pub reads_observed: bool,
}

/// A proposed change: its id and the stored record (spec §3.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    /// The record's [`hord_core::ObjectId`].
    pub change: ChangeId,
    /// The stored record.
    pub record: ChangeRecord,
}

/// An agent's working state on top of a base snapshot (spec §6.1).
///
/// Created by [`Repo::begin`] (in memory, O(1)) or
/// [`Repo::begin_directory`]. Methods that read content record the read in
/// the [`AccessLog`]; [`Workspace::propose`] turns the overlay and the log
/// into a [`ChangeRecord`].
#[derive(Debug)]
pub struct Workspace {
    repo: Repo,
    id: WorkspaceId,
    base: SnapshotId,
    base_change: Option<ChangeId>,
    actor: Actor,
    session: Option<String>,
    materialization: Materialization,
    access_log: AccessLog,
    /// In-memory writes: `None` is a deletion.
    overlay: BTreeMap<RepoPath, Option<Bytes>>,
    declared: Vec<ReadDeclaration>,
    /// Parsed view of edited files, keyed by content.
    views: HashMap<RepoPath, (ObjectId, Option<View>)>,
    /// How a `Directory` checkout was made, when known (ADR 0016).
    materialized_as: Option<MaterializeMode>,
    /// Re-hash every file at propose instead of trusting the stat index.
    paranoid: bool,
}

#[derive(Clone, Debug)]
struct View {
    defs: Arc<Vec<DefinitionInfo>>,
}

impl Workspace {
    pub(crate) fn new(
        repo: Repo,
        id: WorkspaceId,
        base: SnapshotId,
        base_change: Option<ChangeId>,
        actor: Actor,
        session: Option<String>,
        materialization: Materialization,
    ) -> Self {
        let reads_observed = materialization == Materialization::InMemory;
        Self {
            repo,
            id,
            base,
            base_change,
            actor,
            session,
            materialization,
            access_log: AccessLog {
                reads_observed,
                ..AccessLog::default()
            },
            overlay: BTreeMap::new(),
            declared: Vec::new(),
            views: HashMap::new(),
            materialized_as: None,
            paranoid: false,
        }
    }

    pub(crate) fn set_materialized_as(&mut self, mode: Option<MaterializeMode>) {
        self.materialized_as = mode;
    }

    /// How a `Directory` workspace was checked out: a copy-on-write clone or
    /// a copy (ADR 0016). `None` for `InMemory`, or when unknown (a checkout
    /// with no stat index).
    #[must_use]
    pub fn materialized_as(&self) -> Option<MaterializeMode> {
        self.materialized_as
    }

    /// Re-hash every file of a `Directory` workspace at propose and preview
    /// instead of skipping files whose size, mtime, and inode are unchanged
    /// (`hord status --paranoid`, ADR 0016).
    pub fn set_paranoid(&mut self, paranoid: bool) {
        self.paranoid = paranoid;
    }

    /// Workspace id (ULID).
    #[must_use]
    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    /// Snapshot this workspace started from.
    #[must_use]
    pub fn base(&self) -> SnapshotId {
        self.base
    }

    /// Landed change whose result is [`Self::base`], if known. It becomes the
    /// proposal's parent.
    #[must_use]
    pub fn base_change(&self) -> Option<ChangeId> {
        self.base_change
    }

    /// Who works here.
    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// How files are presented.
    #[must_use]
    pub fn materialization(&self) -> &Materialization {
        &self.materialization
    }

    /// Reads and writes so far.
    #[must_use]
    pub fn access_log(&self) -> &AccessLog {
        &self.access_log
    }

    /// Declare a read the log cannot see (ADR 0012, optional). Names are
    /// resolved at [`Self::propose`]; an unresolvable one is an error there.
    pub fn declare_read(&mut self, declaration: ReadDeclaration) {
        self.declared.push(declaration);
    }

    /// Current content of `path`, or `None` if it does not exist. Records the
    /// path (even when it is missing: its absence was read) and every
    /// definition in the file as read.
    pub async fn read_file(&mut self, path: &RepoPath) -> Result<Option<Bytes>> {
        let current = self.current(path).await?;
        self.access_log.read_paths.insert(path.clone());
        let Some(bytes) = current else {
            return Ok(None);
        };
        if let Some(view) = self.view(path, &bytes).await? {
            self.access_log
                .reads
                .extend(view.defs.iter().map(|d| d.node));
        }
        Ok(Some(bytes))
    }

    /// Bytes `range` of `path` (clamped to the file). Records the path (even
    /// when it is missing) and every definition whose span overlaps the range.
    pub async fn read_range(
        &mut self,
        path: &RepoPath,
        range: Range<usize>,
    ) -> Result<Option<Bytes>> {
        let current = self.current(path).await?;
        self.access_log.read_paths.insert(path.clone());
        let Some(bytes) = current else {
            return Ok(None);
        };
        let end = range.end.min(bytes.len());
        let start = range.start.min(end);
        if let Some(view) = self.view(path, &bytes).await? {
            self.access_log.reads.extend(
                view.defs
                    .iter()
                    .filter(|d| overlaps(&d.span, start, end))
                    .map(|d| d.node),
            );
        }
        Ok(Some(Bytes::new(&bytes.as_slice()[start..end])))
    }

    /// Create or replace `path`.
    pub async fn write_file(&mut self, path: &RepoPath, bytes: impl Into<Vec<u8>>) -> Result<()> {
        let bytes = Bytes::new(bytes.into());
        match &self.materialization {
            Materialization::InMemory => {
                self.overlay.insert(path.clone(), Some(bytes));
            }
            Materialization::Directory { path: dir } => {
                let target = fs_path(dir, path);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&target, bytes.as_slice()).await?;
            }
        }
        self.access_log.written_paths.insert(path.clone());
        Ok(())
    }

    /// Delete `path`. Deleting a missing file is not an error.
    pub async fn delete_file(&mut self, path: &RepoPath) -> Result<()> {
        match &self.materialization {
            Materialization::InMemory => {
                self.overlay.insert(path.clone(), None);
            }
            Materialization::Directory { path: dir } => {
                match tokio::fs::remove_file(fs_path(dir, path)).await {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
            }
        }
        self.access_log.written_paths.insert(path.clone());
        Ok(())
    }

    /// Every file in the workspace view. Listing is not a content read.
    pub async fn list_files(&mut self) -> Result<Vec<RepoPath>> {
        match &self.materialization {
            Materialization::InMemory => {
                let base = self.base;
                let mut files: BTreeSet<RepoPath> =
                    blocking(&self.repo.inner, move |inner| inner.list_files(base))
                        .await?
                        .into_iter()
                        .map(|(path, _)| path)
                        .collect();
                for (path, bytes) in &self.overlay {
                    if bytes.is_some() {
                        files.insert(path.clone());
                    } else {
                        files.remove(path);
                    }
                }
                Ok(files.into_iter().collect())
            }
            Materialization::Directory { path } => {
                let dir = path.clone();
                let files = tokio::task::spawn_blocking(move || scan_dir(&dir))
                    .await
                    .map_err(|err| Error::Task(err.to_string()))??;
                Ok(files.into_iter().map(|(path, _)| path).collect())
            }
        }
    }

    /// Definitions in `path` with their ids, names, and spans. Listing
    /// definitions is not a content read.
    pub async fn definitions(&mut self, path: &RepoPath) -> Result<Vec<DefinitionInfo>> {
        let bytes = self
            .current(path)
            .await?
            .ok_or_else(|| Error::MissingFile(path.clone()))?;
        match self.view(path, &bytes).await? {
            Some(view) => Ok(view.defs.as_ref().clone()),
            None => Err(Error::NotParsed(path.clone())),
        }
    }

    /// Definitions named `name` in any parsed file of the workspace view.
    ///
    /// This parses every file the first time it runs on a snapshot. Not a
    /// content read.
    pub async fn find_definitions(&mut self, name: &str) -> Result<Vec<DefinitionInfo>> {
        let mut out = Vec::new();
        for path in self.list_files().await? {
            let Some(bytes) = self.current(&path).await? else {
                continue;
            };
            if let Some(view) = self.view(&path, &bytes).await? {
                out.extend(
                    view.defs
                        .iter()
                        .filter(|d| d.name.as_ref().is_some_and(|n| n.as_str() == name))
                        .cloned(),
                );
            }
        }
        Ok(out)
    }

    /// Source text of definition `node` in `path`. Records `node` and every
    /// definition nested in it as read.
    pub async fn read_definition(&mut self, path: &RepoPath, node: NodeId) -> Result<Bytes> {
        let bytes = self
            .current(path)
            .await?
            .ok_or_else(|| Error::MissingFile(path.clone()))?;
        let view = self
            .view(path, &bytes)
            .await?
            .ok_or_else(|| Error::NotParsed(path.clone()))?;
        let def =
            view.defs
                .iter()
                .find(|d| d.node == node)
                .ok_or_else(|| Error::UnknownDefinition {
                    path: path.clone(),
                    node,
                })?;
        let span = def.span.clone();
        self.access_log.reads.extend(
            view.defs
                .iter()
                .filter(|d| d.span.start >= span.start && d.span.end <= span.end)
                .map(|d| d.node),
        );
        Ok(Bytes::new(&bytes.as_slice()[span]))
    }

    /// Replace the text of definition `node` in `path` with `text`.
    ///
    /// The definition keeps its [`NodeId`] when the new text still names it
    /// (spec §3.4 carrying rules). Records the write.
    pub async fn write_definition(
        &mut self,
        path: &RepoPath,
        node: NodeId,
        text: impl AsRef<[u8]>,
    ) -> Result<()> {
        let bytes = self
            .current(path)
            .await?
            .ok_or_else(|| Error::MissingFile(path.clone()))?;
        let view = self
            .view(path, &bytes)
            .await?
            .ok_or_else(|| Error::NotParsed(path.clone()))?;
        let span = view
            .defs
            .iter()
            .find(|d| d.node == node)
            .map(|d| d.span.clone())
            .ok_or_else(|| Error::UnknownDefinition {
                path: path.clone(),
                node,
            })?;
        let old = bytes.as_slice();
        let mut new = Vec::with_capacity(old.len() + text.as_ref().len());
        new.extend_from_slice(&old[..span.start]);
        new.extend_from_slice(text.as_ref());
        new.extend_from_slice(&old[span.end..]);
        self.write_file(path, new).await?;
        self.access_log.writes.insert(node);
        Ok(())
    }

    /// Check a `Directory` workspace's base out into its directory again.
    /// Overwrites files the base has; leaves other files alone.
    pub async fn materialize(&self) -> Result<()> {
        let Materialization::Directory { path } = &self.materialization else {
            return Ok(());
        };
        let dir = path.clone();
        let base = self.base;
        blocking(&self.repo.inner, move |inner| inner.checkout(base, &dir)).await
    }

    /// Diff the workspace against its base, identify, and store a
    /// [`ChangeRecord`] (spec §6.2 `propose`).
    ///
    /// The record's ops are checked to reproduce the result from the base
    /// before it is stored (spec §3.5). The workspace stays usable; proposing
    /// again yields a new record for the cumulative edits.
    pub async fn propose(&mut self, intent: Intent) -> Result<Proposal> {
        self.build(intent, true).await
    }

    /// What [`Self::propose`] would build now, without storing the record
    /// (`hord status`). The returned id is the one the record would have.
    /// Fails with [`Error::NothingToPropose`] when nothing changed.
    pub async fn preview(&mut self, intent: Intent) -> Result<Proposal> {
        self.build(intent, false).await
    }

    async fn build(&mut self, intent: Intent, store: bool) -> Result<Proposal> {
        let changes = self.changed_files().await?;
        self.access_log
            .written_paths
            .extend(changes.keys().cloned());
        let input = ProposeInput {
            base: self.base,
            parents: self.base_change.into_iter().collect(),
            changes,
            access: self.access_log.clone(),
            declared: self.declared.clone(),
            intent,
            actor: self.actor.clone(),
            session: self.session.clone(),
        };
        blocking(&self.repo.inner, move |inner| {
            crate::propose::propose(inner, input, store)
        })
        .await
    }

    /// Paths whose content differs from the base, with the new content.
    async fn changed_files(&self) -> Result<BTreeMap<RepoPath, Option<Bytes>>> {
        match &self.materialization {
            Materialization::InMemory => {
                let base = self.base;
                let overlay = self.overlay.clone();
                blocking(&self.repo.inner, move |inner| {
                    let mut out = BTreeMap::new();
                    for (path, bytes) in overlay {
                        let before = inner.blob_id(base, &path)?;
                        let after = match &bytes {
                            Some(b) => Some(blob_object_id(b.as_slice())?),
                            None => None,
                        };
                        if before != after {
                            out.insert(path, bytes);
                        }
                    }
                    Ok(out)
                })
                .await
            }
            Materialization::Directory { path } => {
                let dir = path.clone();
                let base = self.base;
                let paranoid = self.paranoid;
                blocking(&self.repo.inner, move |inner| {
                    directory_changes(inner, base, &dir, paranoid)
                })
                .await
            }
        }
    }

    /// Current bytes of `path` in the workspace view.
    async fn current(&self, path: &RepoPath) -> Result<Option<Bytes>> {
        match &self.materialization {
            Materialization::InMemory => {
                if let Some(bytes) = self.overlay.get(path) {
                    return Ok(bytes.clone());
                }
                let base = self.base;
                let path = path.clone();
                blocking(&self.repo.inner, move |inner| inner.file_bytes(base, &path)).await
            }
            Materialization::Directory { path: dir } => {
                match tokio::fs::read(fs_path(dir, path)).await {
                    Ok(bytes) => Ok(Some(Bytes::new(bytes))),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err.into()),
                }
            }
        }
    }

    /// Parsed, identified view of `path` with content `bytes`. Unedited files
    /// use the base snapshot's ids; edited files carry ids from the base.
    async fn view(&mut self, path: &RepoPath, bytes: &Bytes) -> Result<Option<View>> {
        let content = blob_object_id(bytes.as_slice())?;
        if let Some((cached, view)) = self.views.get(path)
            && *cached == content
        {
            return Ok(view.clone());
        }
        let base = self.base;
        let path_owned = path.clone();
        let bytes = bytes.clone();
        let view = blocking(&self.repo.inner, move |inner| {
            view_of(inner, base, &path_owned, content, &bytes)
        })
        .await?;
        self.views.insert(path.clone(), (content, view.clone()));
        Ok(view)
    }
}

fn view_of(
    inner: &Inner,
    base: SnapshotId,
    path: &RepoPath,
    content: ObjectId,
    bytes: &Bytes,
) -> Result<Option<View>> {
    let base_view = inner.file_view(base, path)?;
    let (lang, tree) = match &base_view {
        Some(view) if view.blob == content => match &view.parsed {
            Some(parsed) => (parsed.lang, Arc::clone(&parsed.tree)),
            None => return Ok(None),
        },
        _ => {
            let Some(adapter) = inner.adapter(path, bytes.as_slice()) else {
                return Ok(None);
            };
            let Some(parsed) = inner.parse(adapter, content, bytes.as_slice()) else {
                return Ok(None);
            };
            let empty = IdentifiedTree::default();
            let base_tree = base_view
                .as_ref()
                .and_then(|v| v.parsed.as_ref())
                .map(|p| Arc::clone(&p.tree));
            let base_ref = base_tree.as_deref().unwrap_or(&empty);
            let mapping = hord_identity::carry_in(adapter, path, base_ref, &parsed, &[]).map_err(
                |source| Error::Identity {
                    path: path.clone(),
                    source,
                },
            )?;
            (
                adapter.lang(),
                Arc::new(IdentifiedTree::new((*parsed).clone(), mapping.nodes)),
            )
        }
    };
    let adapter = inner
        .adapter_for(path, lang)
        .ok_or_else(|| Error::NotParsed(path.clone()))?;
    let defs = Arc::new(definitions(adapter, path, &tree));
    Ok(Some(View { defs }))
}

/// Files of a `Directory` checkout that differ from `base`. With a stat
/// index and not `paranoid`, a file whose size, mtime, and inode match the
/// checkout is unchanged without being read (ADR 0016).
fn directory_changes(
    inner: &Inner,
    base: SnapshotId,
    dir: &Path,
    paranoid: bool,
) -> Result<BTreeMap<RepoPath, Option<Bytes>>> {
    let index = if paranoid { None } else { load_stat_index(dir) };
    let mut base_files: BTreeMap<RepoPath, ObjectId> =
        inner.list_files(base)?.into_iter().collect();
    let mut seen = Vec::new();
    walk_files(dir, &mut Vec::new(), &mut |path, meta| {
        seen.push((path, Stat::of(meta)));
    })?;
    let mut out = BTreeMap::new();
    for (path, stat) in seen {
        let base_blob = base_files.remove(&path);
        let unchanged = base_blob.is_some()
            && index
                .as_ref()
                .and_then(|i| i.files.get(&path))
                .is_some_and(|at| *at == stat);
        if unchanged {
            continue;
        }
        let bytes = read_checkout_file(dir, &path)?;
        if base_blob != Some(blob_object_id(&bytes)?) {
            out.insert(path, Some(Bytes::new(bytes)));
        }
    }
    for path in base_files.into_keys() {
        out.insert(path, None);
    }
    Ok(out)
}

fn overlaps(span: &Range<usize>, start: usize, end: usize) -> bool {
    if start == end {
        return span.start <= start && start < span.end;
    }
    span.start < end && start < span.end
}

/// Every regular file under `dir`, as repository paths with contents.
fn scan_dir(dir: &Path) -> Result<Vec<(RepoPath, Vec<u8>)>> {
    fn walk(
        dir: &Path,
        prefix: &mut Vec<String>,
        out: &mut Vec<(RepoPath, Vec<u8>)>,
    ) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|name| Error::InvalidPath(name.to_string_lossy().into_owned()))?;
            let kind = entry.file_type()?;
            prefix.push(name);
            if kind.is_dir() {
                walk(&entry.path(), prefix, out)?;
            } else if kind.is_file() {
                out.push((RepoPath::new(prefix.clone()), std::fs::read(entry.path())?));
            }
            prefix.pop();
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut Vec::new(), &mut out)?;
    Ok(out)
}
