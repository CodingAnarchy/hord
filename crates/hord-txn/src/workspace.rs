//! [`Workspace`]: a snapshot pointer, an overlay, and an access log (spec §6.1).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hord_core::{
    Actor, Bytes, ChangeId, ChangeRecord, FileMode, Intent, NodeId, ObjectId, RepoPath, SnapshotId,
};
use hord_lang::IdentifiedTree;
use hord_store::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::materialize::{MaterializeMode, load_stat_index, read_checkout_entry, walk_checkout};
use crate::propose::{FileContent, ProposeInput, ReadDeclaration};
use crate::repo::{Inner, Repo, blocking, fs_path};
use crate::semantic::{DefinitionInfo, carry, definitions};
use crate::snapshot::file_object_id;
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
    /// In-memory writes: `None` is a deletion. A written file keeps the
    /// mode it has in the base (a new one is regular).
    overlay: BTreeMap<RepoPath, Option<Bytes>>,
    declared: Vec<ReadDeclaration>,
    /// Parsed view of edited files, keyed by content.
    views: HashMap<RepoPath, (ObjectId, Option<View>)>,
    /// How a `Directory` checkout was made, when known (ADR 0016).
    materialized_as: Option<MaterializeMode>,
    /// Re-hash every file at propose instead of trusting the stat index.
    paranoid: bool,
    /// Untracked paths of a `Directory` checkout the last build left out.
    skipped: Vec<RepoPath>,
}

/// Changed paths with their new content (`None`: deleted), and the untracked
/// paths a `Directory` walk skipped.
type Changes = (BTreeMap<RepoPath, Option<FileContent>>, Vec<RepoPath>);

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
            skipped: Vec::new(),
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
    /// instead of skipping files whose size and mtime are unchanged
    /// (`hord status --paranoid`, ADR 0016).
    pub fn set_paranoid(&mut self, paranoid: bool) {
        self.paranoid = paranoid;
    }

    /// Untracked paths of a `Directory` checkout that the last
    /// [`Self::propose`] or [`Self::preview`] left out, sorted: those an
    /// ignore rule matches (the base snapshot's `.gitignore` files, plus
    /// `.hord/` and `.git`), with an ignored directory listed once rather than
    /// its contents, and special files (fifos, sockets), which a tree cannot
    /// hold. A tracked file is never skipped. Empty for `InMemory`.
    #[must_use]
    pub fn skipped(&self) -> &[RepoPath] {
        &self.skipped
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

    /// Where this workspace reads objects from: its repository's
    /// [`ObjectSource`](crate::ObjectSource) (ADR 0024), the local store
    /// or a remote source that fetches on demand.
    #[must_use]
    pub fn objects(&self) -> &dyn crate::ObjectSource {
        &self.repo
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
    ///
    /// A symlink's content is its target (ADR 0042).
    pub async fn read_file(&mut self, path: &RepoPath) -> Result<Option<Bytes>> {
        let current = self.current(path).await?;
        self.access_log.read_paths.insert(path.clone());
        let Some(content) = current else {
            return Ok(None);
        };
        if let Some(view) = self.view(path, &content).await? {
            self.access_log
                .reads
                .extend(view.defs.iter().map(|d| d.node));
        }
        Ok(Some(content.bytes))
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
        let Some(content) = current else {
            return Ok(None);
        };
        let bytes = &content.bytes;
        let end = range.end.min(bytes.len());
        let start = range.start.min(end);
        if let Some(view) = self.view(path, &content).await? {
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

    /// Every file in the workspace view. Listing is not a content read. For
    /// a `Directory` workspace, untracked files an ignore rule matches are
    /// not in the view ([`Self::skipped`]).
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
                let base = self.base;
                let walked = blocking(&self.repo.inner, move |inner| {
                    let filter = inner.checkout_filter(&dir, inner.list_files(base)?)?;
                    walk_checkout(&dir, Some(&filter), None)
                })
                .await?;
                Ok(walked.files.into_iter().map(|(path, _)| path).collect())
            }
        }
    }

    /// Definitions in `path` with their ids, names, and spans. Listing
    /// definitions is not a content read.
    pub async fn definitions(&mut self, path: &RepoPath) -> Result<Vec<DefinitionInfo>> {
        let content = self
            .current(path)
            .await?
            .ok_or_else(|| Error::MissingFile(path.clone()))?;
        match self.view(path, &content).await? {
            Some(view) => Ok(view.defs.as_ref().clone()),
            None => Err(Error::NotParsed(path.clone())),
        }
    }

    /// Source text of definition `node` in `path`. Records `node` and every
    /// definition nested in it as read.
    pub async fn read_definition(&mut self, path: &RepoPath, node: NodeId) -> Result<Bytes> {
        let content = self
            .current(path)
            .await?
            .ok_or_else(|| Error::MissingFile(path.clone()))?;
        let bytes = &content.bytes;
        let view = self
            .view(path, &content)
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
        let content = self
            .current(path)
            .await?
            .ok_or_else(|| Error::MissingFile(path.clone()))?;
        let bytes = &content.bytes;
        let view = self
            .view(path, &content)
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
    /// [`ChangeRecord`] (spec §6.2 `propose`). A `.hord-policy.toml` that
    /// does not parse is [`Error::Policy`] (ADR 0026 amendment: the lander
    /// would reject the change).
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
        let (changes, skipped) = self.changed_files().await?;
        self.skipped = skipped;
        // ADR 0026 amendment: the lander rejects a change whose
        // `.hord-policy.toml` does not parse; say so before proposing it.
        if store
            && let Some(Some(content)) = hord_policy::POLICY_PATH
                .parse::<RepoPath>()
                .ok()
                .and_then(|p| changes.get(&p))
        {
            crate::gate::parse_policy_file(content.bytes.as_slice()).map_err(Error::Policy)?;
        }
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

    /// Paths whose content differs from the base, with the new content, and
    /// the untracked paths a `Directory` walk skipped.
    async fn changed_files(&self) -> Result<Changes> {
        match &self.materialization {
            Materialization::InMemory => {
                let base = self.base;
                let overlay = self.overlay.clone();
                blocking(&self.repo.inner, move |inner| {
                    let mut out = BTreeMap::new();
                    for (path, bytes) in overlay {
                        let before = inner.blob_id(base, &path)?;
                        let content = match bytes {
                            Some(bytes) => Some(FileContent {
                                mode: base_mode(inner, before)?,
                                bytes,
                            }),
                            None => None,
                        };
                        let after = match &content {
                            Some(c) => Some(file_object_id(c.mode, c.bytes.as_slice())?),
                            None => None,
                        };
                        if before != after {
                            out.insert(path, content);
                        }
                    }
                    Ok((out, Vec::new()))
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

    /// Current content of `path` in the workspace view, with its mode.
    async fn current(&self, path: &RepoPath) -> Result<Option<FileContent>> {
        let base = self.base;
        let path = path.clone();
        match &self.materialization {
            Materialization::InMemory => {
                let written = self.overlay.get(&path).cloned();
                blocking(&self.repo.inner, move |inner| {
                    let entry = inner.blob_id(base, &path)?;
                    match written {
                        Some(Some(bytes)) => Ok(Some(FileContent {
                            mode: base_mode(inner, entry)?,
                            bytes,
                        })),
                        Some(None) => Ok(None),
                        None => match entry {
                            Some(id) => {
                                let (mode, bytes) = inner.file_content(id)?;
                                Ok(Some(FileContent { bytes, mode }))
                            }
                            None => Ok(None),
                        },
                    }
                })
                .await
            }
            Materialization::Directory { path: dir } => {
                let dir = dir.clone();
                blocking(&self.repo.inner, move |inner| {
                    let base_mode = match inner.blob_id(base, &path)? {
                        Some(id) => Some(inner.file_mode(id)?),
                        None => None,
                    };
                    read_checkout_entry(&dir, &path, base_mode)
                })
                .await
            }
        }
    }

    /// Parsed, identified view of `path` with `content`. Unedited files use
    /// the base snapshot's ids; edited files carry ids from the base. A
    /// symlink or gitlink is never parsed (ADR 0042).
    async fn view(&mut self, path: &RepoPath, content: &FileContent) -> Result<Option<View>> {
        if !content.mode.holds_contents() {
            return Ok(None);
        }
        let id = file_object_id(content.mode, content.bytes.as_slice())?;
        if let Some((cached, view)) = self.views.get(path)
            && *cached == id
        {
            return Ok(view.clone());
        }
        let base = self.base;
        let path_owned = path.clone();
        let bytes = content.bytes.clone();
        let view = blocking(&self.repo.inner, move |inner| {
            view_of(inner, base, &path_owned, id, &bytes)
        })
        .await?;
        self.views.insert(path.clone(), (id, view.clone()));
        Ok(view)
    }
}

/// The mode a file written through the API takes: its base entry's, or
/// regular for a new file.
fn base_mode(inner: &Inner, entry: Option<ObjectId>) -> Result<FileMode> {
    match entry {
        Some(id) => inner.file_mode(id),
        None => Ok(FileMode::Regular),
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
            let mapping = carry(adapter, path, base, base_ref, &parsed)?;
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

/// Files of a `Directory` checkout that differ from `base`, and the
/// untracked paths the walk skipped. With a stat index and not `paranoid`, a
/// file whose size and mtime match the index, and whose entry is not racily
/// clean, is unchanged without being read (ADR 0016).
fn directory_changes(
    inner: &Inner,
    base: SnapshotId,
    dir: &Path,
    paranoid: bool,
) -> Result<Changes> {
    let index = if paranoid { None } else { load_stat_index(dir) };
    let filter = inner.checkout_filter(dir, inner.list_files(base)?)?;
    let walked = walk_checkout(dir, Some(&filter), None)?;
    let base_files = filter.tracked();
    let mut seen = HashSet::with_capacity(base_files.len());
    let mut out = BTreeMap::new();
    for (path, stat) in walked.files {
        let base_blob = base_files.get_key_value(&path).map(|(key, blob)| {
            seen.insert(key);
            *blob
        });
        let unchanged =
            base_blob.is_some() && index.as_ref().is_some_and(|i| i.is_clean(&path, stat));
        if unchanged {
            continue;
        }
        // Only a platform without exec bits or symlinks needs the base's
        // mode (see `materialize`); reading it costs an object read.
        let base_mode = match base_blob {
            Some(id) if cfg!(not(unix)) => Some(inner.file_mode(id)?),
            _ => None,
        };
        // Gone since the walk: a deletion, found below.
        let Some(content) = read_checkout_entry(dir, &path, base_mode)? else {
            seen.remove(&path);
            continue;
        };
        if base_blob != Some(file_object_id(content.mode, content.bytes.as_slice())?) {
            out.insert(path, Some(content));
        }
    }
    for (path, id) in base_files {
        if seen.contains(path) {
            continue;
        }
        // A gitlink is checked out as a directory, never walked as a file.
        let gitlink = fs_path(dir, path).is_dir() && inner.file_mode(*id)? == FileMode::Gitlink;
        if !gitlink {
            out.insert(path.clone(), None);
        }
    }
    Ok((out, walked.skipped))
}

fn overlaps(span: &Range<usize>, start: usize, end: usize) -> bool {
    if start == end {
        return span.start <= start && start < span.end;
    }
    span.start < end && start < span.end
}
