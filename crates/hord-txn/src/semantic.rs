//! Definitions and identity of a snapshot's parsed files (spec §3.4, §4.3).
//!
//! # Identity per snapshot
//!
//! A file's [`NodeId`]s in a snapshot are either a fresh assignment
//! ([`hord_identity::assign_in`], a pure function of the file's path and
//! content) or were
//! carried by a change that wrote the file. Only the second kind is stored:
//! an [`IdentityIndex`] per snapshot maps each such path to a [`FileIdentity`]
//! object, and [`hord_store::Store::identity_index`] points the snapshot at
//! it. Unchanged files share objects between snapshots, so an index costs
//! O(files written so far), not O(definitions).
//!
//! NodeIds are a function of stored objects. The index object names its
//! snapshot, and [`hord_store::Store::set_identity_index`] also stores a
//! binding object that [`hord_store::Store::rebuild_index`] restores the
//! pointer from. A snapshot with no pointer is not read as "all fresh": the
//! empty tree has an empty index, a snapshot produced by a Tier 0 change
//! (only `Blob`/`Tree` ops and no identity deltas: bootstrap, git import)
//! gets its base's index minus the files it changed (ADR 0015: a blob-only
//! write is coarse, so those files are fresh), and anything else is
//! [`Error::MissingIdentity`].
//!
//! # References (ADR 0012)
//!
//! The Rust resolver needs a [`ResolveCtx`] over the whole snapshot. One is
//! built per base snapshot and cached. References are resolved at an
//! [`Anchor`]: the site the text stands for, so identical definitions in two
//! modules each resolve in their own module. Result-side references for a
//! written definition reuse the base context, anchored at the definition's
//! id when the base has it, else at its nearest enclosing definition the
//! base has (a birth), else at the file.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use hord_core::{
    Bytes, ChangeRecord, IdentityMap, LangId, Node, NodeId, NodeKind, NodePath, ObjectId, Op,
    QualifiedName, RepoPath, SnapshotId,
};
use hord_lang::{Anchor, IdentifiedTree, LangAdapter, NodeTree, ResolveCtx, Site};
use hord_lang_rust::{ManifestFile, RustAdapter};
use hord_store::EdgeKind;
use serde::{Deserialize, Serialize};

use crate::repo::{Inner, lock};
use crate::{Error, Result};

/// Files of one snapshot whose [`NodeId`]s differ from a fresh assignment.
///
/// Stored as canonical CBOR; the pairs are sorted by path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct IdentityIndex {
    /// Snapshot this index describes. Set by
    /// [`Inner::put_identity_index`], so an index cloned from another
    /// snapshot is re-labelled when stored.
    pub snapshot: SnapshotId,
    pub files: Vec<(RepoPath, ObjectId)>,
}

impl IdentityIndex {
    /// No carried files: every file of `snapshot` is a fresh assignment.
    pub fn empty(snapshot: SnapshotId) -> Self {
        Self {
            snapshot,
            files: Vec::new(),
        }
    }

    pub fn get(&self, path: &RepoPath) -> Option<ObjectId> {
        self.files
            .binary_search_by(|(p, _)| p.cmp(path))
            .ok()
            .map(|i| self.files[i].1)
    }

    pub fn set(&mut self, path: &RepoPath, identity: Option<ObjectId>) {
        match (self.files.binary_search_by(|(p, _)| p.cmp(path)), identity) {
            (Ok(i), Some(id)) => self.files[i].1 = id,
            (Ok(i), None) => {
                self.files.remove(i);
            }
            (Err(i), Some(id)) => self.files.insert(i, (path.clone(), id)),
            (Err(_), None) => {}
        }
    }
}

/// Carried [`NodeId`]s of one file, bound to the blob they were computed for.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileIdentity {
    pub blob: ObjectId,
    pub ids: IdentityMap,
}

/// One file of a snapshot or workspace view.
#[derive(Clone, Debug)]
pub(crate) struct FileView {
    pub blob: ObjectId,
    pub bytes: Bytes,
    /// Parsed file with definition ids; `None` for blob-tier files and files
    /// that do not parse.
    pub parsed: Option<Parsed>,
}

/// A parsed file: adapter language and identified tree.
#[derive(Clone, Debug)]
pub(crate) struct Parsed {
    pub lang: LangId,
    pub tree: Arc<IdentifiedTree>,
}

/// A definition in a file view, as the workspace API reports it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DefinitionInfo {
    /// Stable identity (spec §3.4).
    pub node: NodeId,
    /// File that contains the definition.
    pub path: RepoPath,
    /// Adapter-defined kind, e.g. `function_item`.
    pub kind: NodeKind,
    /// Qualified name, when the adapter names this kind.
    pub name: Option<QualifiedName>,
    /// Byte range in the file, including attached trivia (spec §3.3).
    pub span: Range<usize>,
    /// Nearest enclosing definition, if any.
    pub parent: Option<NodeId>,
}

pub(crate) struct RustCtx {
    pub ctx: ResolveCtx,
}

impl Inner {
    pub(crate) fn adapter(&self, path: &RepoPath, head: &[u8]) -> Option<&dyn LangAdapter> {
        let head = &head[..head.len().min(512)];
        self.adapters.get(path, head)
    }

    /// The adapter that parsed `path` as `lang`. Two adapters can share a
    /// language (`Cargo.lock` and TOML), so the path decides.
    pub(crate) fn adapter_for(&self, path: &RepoPath, lang: LangId) -> Option<&dyn LangAdapter> {
        self.adapters
            .iter()
            .find(|a| a.lang() == lang && a.matches(path, &[]))
    }

    /// Parse `bytes` (whose blob id is `blob`), sharing the result.
    pub(crate) fn parse(
        &self,
        adapter: &dyn LangAdapter,
        blob: ObjectId,
        bytes: &[u8],
    ) -> Option<Arc<NodeTree>> {
        let key = (blob, adapter.lang());
        if let Some(tree) = lock(&self.parsed).get(&key) {
            return Some(tree);
        }
        let tree = Arc::new(adapter.parse(bytes).ok()?);
        self.cache_parsed(key, Arc::clone(&tree));
        Some(tree)
    }

    pub(crate) fn identity_index(&self, snapshot: SnapshotId) -> Result<Arc<IdentityIndex>> {
        if let Some(index) = lock(&self.indexes).get(&snapshot) {
            return Ok(Arc::clone(index));
        }
        let index = match self.store.identity_index(snapshot)? {
            Some(id) => {
                let index = self.store.get_object::<IdentityIndex>(id)?;
                if index.snapshot != snapshot {
                    return Err(Error::Corrupt {
                        id,
                        reason: format!(
                            "identity index for {} is recorded for {snapshot}",
                            index.snapshot
                        ),
                    });
                }
                index
            }
            None if snapshot == self.empty_tree => IdentityIndex::empty(snapshot),
            None => return self.derive_tier0_index(snapshot),
        };
        let index = Arc::new(index);
        self.cache_index(snapshot, Arc::clone(&index));
        Ok(index)
    }

    fn cache_index(&self, snapshot: SnapshotId, index: Arc<IdentityIndex>) {
        let mut cache = lock(&self.indexes);
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(snapshot, index);
    }

    /// The index of a snapshot with no pointer, when a chain of Tier 0 changes
    /// in the log leads to it from a snapshot that has one (or the empty
    /// tree). Stores the result, so this runs once per snapshot.
    fn derive_tier0_index(&self, snapshot: SnapshotId) -> Result<Arc<IdentityIndex>> {
        let log = self.store.log()?;
        let mut chain = Vec::new();
        let mut target = snapshot;
        let mut end = log.len();
        let start = loop {
            let found = log[..end]
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, id)| match self.change_record(*id) {
                    Ok(record) if record.result == target => Some(Ok((i, record))),
                    Ok(_) | Err(Error::MissingChange(_)) => None,
                    Err(err) => Some(Err(err)),
                })
                .transpose()?;
            let Some((i, record)) = found else {
                return Err(Error::MissingIdentity(target));
            };
            if !is_tier0(&record) {
                return Err(Error::MissingIdentity(target));
            }
            target = record.base;
            end = i;
            chain.push(record);
            if target == self.empty_tree {
                break IdentityIndex::empty(target);
            }
            if lock(&self.indexes).contains_key(&target)
                || self.store.identity_index(target)?.is_some()
            {
                break (*self.identity_index(target)?).clone();
            }
        };
        let mut index = start;
        for record in chain.iter().rev() {
            if !index.files.is_empty() {
                for delta in self.changed_paths(record.base, record.result)? {
                    index.set(&delta.path, None);
                }
            }
        }
        self.put_identity_index(snapshot, index)?;
        self.identity_index(snapshot)
    }

    /// Make `index` the identity of `snapshot` for this process without
    /// storing it: the lander's candidate result, which is validated before
    /// it lands and stored by [`Self::put_identity_index`] only then.
    pub(crate) fn stage_identity_index(&self, snapshot: SnapshotId, mut index: IdentityIndex) {
        index.snapshot = snapshot;
        self.cache_index(snapshot, Arc::new(index));
    }

    pub(crate) fn put_identity_index(
        &self,
        snapshot: SnapshotId,
        mut index: IdentityIndex,
    ) -> Result<()> {
        index.snapshot = snapshot;
        let id = self.store.put_object(&index)?;
        self.store.set_identity_index(snapshot, id)?;
        lock(&self.indexes).insert(snapshot, Arc::new(index));
        Ok(())
    }

    /// `path` in `snapshot`, parsed and identified when an adapter claims it.
    pub(crate) fn file_view(
        &self,
        snapshot: SnapshotId,
        path: &RepoPath,
    ) -> Result<Option<FileView>> {
        let Some(blob) = self.blob_id(snapshot, path)? else {
            return Ok(None);
        };
        let bytes = self.blob_bytes(blob)?;
        let identity = self.identity_index(snapshot)?.get(path);
        let parsed = self.identify_blob(path, blob, &bytes, identity)?;
        Ok(Some(FileView {
            blob,
            bytes,
            parsed,
        }))
    }

    /// Parse and identify `bytes`. With `identity`, ids come from that stored
    /// [`FileIdentity`] (if it was computed for this blob); otherwise from a
    /// fresh assignment.
    pub(crate) fn identify_blob(
        &self,
        path: &RepoPath,
        blob: ObjectId,
        bytes: &[u8],
        identity: Option<ObjectId>,
    ) -> Result<Option<Parsed>> {
        let Some(adapter) = self.adapter(path, bytes) else {
            return Ok(None);
        };
        let lang = adapter.lang();
        // A FileIdentity object names its blob, so (blob, identity) keys are
        // only ever inserted for a matching pair.
        if let Some(tree) = lock(&self.identified).get(&(path.clone(), blob, identity)) {
            return Ok(Some(Parsed { lang, tree }));
        }
        let stored = match identity {
            Some(id) => {
                let file: FileIdentity = self.store.get_object(id)?;
                (file.blob == blob).then_some((id, file))
            }
            None => None,
        };
        let key = (path.clone(), blob, stored.as_ref().map(|(id, _)| *id));
        let Some(tree) = self.parse(adapter, blob, bytes) else {
            return Ok(None);
        };
        let ids = match &stored {
            Some((_, file)) => ids_from_map(&tree, &file.ids),
            None => hord_identity::assign_in(adapter, path, &tree).nodes,
        };
        let identified = Arc::new(IdentifiedTree::new((*tree).clone(), ids));
        self.cache_identified(key, Arc::clone(&identified));
        Ok(Some(Parsed {
            lang,
            tree: identified,
        }))
    }

    /// Store the ids of `tree` (the parse of `blob`) as a [`FileIdentity`].
    pub(crate) fn put_file_identity(
        &self,
        path: &RepoPath,
        blob: ObjectId,
        tree: &Arc<IdentifiedTree>,
    ) -> Result<ObjectId> {
        let ids = IdentityMap {
            nodes: pointers(path, tree),
            deltas: Vec::new(),
        };
        let id = self.store.put_object(&FileIdentity { blob, ids })?;
        self.cache_identified((path.clone(), blob, Some(id)), Arc::clone(tree));
        Ok(id)
    }

    /// Resolution context over every Rust file in `snapshot`, built once.
    pub(crate) fn rust_ctx(&self, snapshot: SnapshotId) -> Result<Arc<RustCtx>> {
        let slot = self.ctx_slot(snapshot);
        slot.get_or_init(|| self.build_rust_ctx(snapshot).map_err(|e| e.to_string()))
            .clone()
            .map_err(|reason| Error::Corrupt {
                id: snapshot,
                reason: format!("resolution context: {reason}"),
            })
    }

    /// Build `snapshot`'s context from the most recently built one: only
    /// files whose blob or carried identity changed (or whose module moved)
    /// are loaded and re-indexed (perf review #1). The contents equal a full
    /// build (`build_rust_ctx_full`, checked by the tests below).
    fn build_rust_ctx(&self, snapshot: SnapshotId) -> Result<Arc<RustCtx>> {
        let rust = RustAdapter;
        let identity = self.identity_index(snapshot)?;
        let mut files = Vec::new();
        let mut manifests = Vec::new();
        for (path, blob) in self.list_files(snapshot)? {
            if rust.matches(&path, &[]) {
                let key = (blob, identity.get(&path));
                files.push((path, key));
            } else if path.components().last().is_some_and(|n| n == "Cargo.toml") {
                manifests.push((path, self.blob_bytes(blob)?));
            }
        }
        let manifest_views: Vec<ManifestFile<'_>> = manifests
            .iter()
            .map(|(path, bytes)| ManifestFile {
                path,
                bytes: bytes.as_slice(),
            })
            .collect();
        let prev = self.latest_rust_ctx();
        let ctx = rust.resolve_context_incremental(
            prev.as_ref().map(|p| &p.ctx),
            &files,
            &manifest_views,
            |path| {
                Ok::<_, Error>(
                    self.file_view(snapshot, path)?
                        .and_then(|view| view.parsed)
                        .filter(|parsed| parsed.lang.as_str() == hord_lang_rust::LANG)
                        .map(|parsed| parsed.tree),
                )
            },
        )?;
        let ctx = Arc::new(RustCtx { ctx });
        self.set_latest_rust_ctx(Arc::clone(&ctx));
        Ok(ctx)
    }

    /// The context as a from-scratch build over every file: the reference
    /// [`Self::build_rust_ctx`] must equal.
    #[cfg(test)]
    pub(crate) fn build_rust_ctx_full(&self, snapshot: SnapshotId) -> Result<ResolveCtx> {
        let rust = RustAdapter;
        let mut trees = Vec::new();
        let mut manifests = Vec::new();
        for (path, blob) in self.list_files(snapshot)? {
            if rust.matches(&path, &[]) {
                if let Some(view) = self.file_view(snapshot, &path)?
                    && let Some(parsed) = view.parsed
                    && parsed.lang.as_str() == hord_lang_rust::LANG
                {
                    trees.push((path, parsed.tree));
                }
            } else if path.components().last().is_some_and(|n| n == "Cargo.toml") {
                manifests.push((path, self.blob_bytes(blob)?));
            }
        }
        let views: Vec<hord_lang_rust::RustFile<'_>> = trees
            .iter()
            .map(|(path, tree)| hord_lang_rust::RustFile {
                path,
                tree: &tree.tree,
                ids: &tree.ids,
            })
            .collect();
        let manifest_views: Vec<ManifestFile<'_>> = manifests
            .iter()
            .map(|(path, bytes)| ManifestFile {
                path,
                bytes: bytes.as_slice(),
            })
            .collect();
        Ok(rust.resolve_context_with(&views, &manifest_views))
    }

    /// Targets of `References` edges leaving `node` (one hop), resolved in the
    /// scope of `anchor`, plus any edges already in the store's edge index for
    /// `source` in `snapshot`.
    pub(crate) fn rust_references(
        &self,
        ctx: &RustCtx,
        snapshot: SnapshotId,
        source: NodeId,
        anchor: &Anchor,
        node: &Node,
        out: &mut BTreeSet<NodeId>,
    ) -> Result<()> {
        let rust = RustAdapter;
        for name in rust.references_at(&ctx.ctx, anchor, node) {
            if let Some(target) = rust.resolve(&ctx.ctx, &name) {
                out.insert(target);
            }
        }
        out.extend(self.store.edges(snapshot, source, EdgeKind::References)?);
        Ok(())
    }
}

/// A change that carries no identity: only `Blob`/`Tree` ops and no identity
/// deltas (bootstrap, git import and sync; ADR 0015 amendment).
fn is_tier0(record: &ChangeRecord) -> bool {
    record.identity_deltas.is_empty()
        && record
            .ops
            .iter()
            .all(|op| matches!(op, Op::Blob { .. } | Op::Tree { .. }))
}

/// [`NodeId`] → location for every identified definition in `tree`. Ids
/// are keyed by site, so the pointer is the site.
fn pointers(path: &RepoPath, tree: &IdentifiedTree) -> BTreeMap<NodeId, NodePath> {
    tree.ids
        .iter()
        .map(|(site, id)| {
            (
                *id,
                NodePath {
                    file: path.clone(),
                    pointer: site.clone(),
                },
            )
        })
        .collect()
}

/// Rebuild the site → [`NodeId`] map from stored locations, keeping only
/// sites that exist in `tree`.
fn ids_from_map(tree: &NodeTree, map: &IdentityMap) -> BTreeMap<Site, NodeId> {
    map.nodes
        .iter()
        .filter(|(_, at)| hord_lang::oid_at(tree, &at.pointer).is_some())
        .map(|(id, at)| (at.pointer.clone(), *id))
        .collect()
}

/// Every identified definition in `tree` with its span, name, and parent.
pub(crate) fn definitions(
    adapter: &dyn LangAdapter,
    path: &RepoPath,
    tree: &IdentifiedTree,
) -> Vec<DefinitionInfo> {
    let mut out = Vec::new();
    if let Some(root) = tree.tree.root() {
        let mut ancestors = Vec::new();
        let mut parents = Vec::new();
        let mut offset = 0usize;
        walk_defs(
            adapter,
            path,
            tree,
            root,
            &mut Vec::new(),
            &mut ancestors,
            &mut parents,
            &mut offset,
            &mut out,
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_defs<'t>(
    adapter: &dyn LangAdapter,
    path: &RepoPath,
    tree: &'t IdentifiedTree,
    oid: ObjectId,
    site: &mut Site,
    ancestors: &mut Vec<&'t Node>,
    parents: &mut Vec<NodeId>,
    offset: &mut usize,
    out: &mut Vec<DefinitionInfo>,
) {
    let Some(node) = tree.tree.get(oid) else {
        return;
    };
    if node.children.is_empty() {
        *offset += node.raw.len();
        return;
    }
    let start = *offset;
    let own = tree.ids.get(site.as_slice()).copied();
    let slot = own.map(|node_id| {
        out.push(DefinitionInfo {
            node: node_id,
            path: path.clone(),
            kind: node.kind,
            name: node
                .name
                .clone()
                .or_else(|| adapter.qualified_name(ancestors, node)),
            span: start..start,
            parent: parents.last().copied(),
        });
        out.len() - 1
    });
    if let Some(node_id) = own {
        parents.push(node_id);
    }
    ancestors.push(node);
    for (i, child) in node.children.iter().enumerate() {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        walk_defs(
            adapter, path, tree, *child, site, ancestors, parents, offset, out,
        );
        site.pop();
    }
    ancestors.pop();
    if own.is_some() {
        parents.pop();
    }
    if let Some(slot) = slot {
        out[slot].span = start..*offset;
    }
}

/// Site of each identified definition, by [`NodeId`].
pub(crate) fn by_node(tree: &IdentifiedTree) -> HashMap<NodeId, Site> {
    tree.ids
        .iter()
        .map(|(site, nid)| (*nid, site.clone()))
        .collect()
}

/// Nearest enclosing identified definition of each identified definition:
/// the longest proper prefix of its site that is a definition site.
pub(crate) fn enclosing(tree: &IdentifiedTree) -> HashMap<Site, Site> {
    let mut out = HashMap::new();
    for site in tree.ids.keys() {
        let parent = (0..site.len())
            .rev()
            .map(|len| &site[..len])
            .find(|prefix| tree.ids.contains_key(*prefix));
        if let Some(parent) = parent {
            out.insert(site.clone(), parent.to_vec());
        }
    }
    out
}

/// Where to resolve the result definition at `site`: its own id when the
/// base context knows it, else its nearest enclosing definition the base
/// knows (a definition born in the change), else the file at `path`.
pub(crate) fn result_anchor(
    base: Option<&IdentifiedTree>,
    result: &IdentifiedTree,
    site: &[u32],
    result_parents: &HashMap<Site, Site>,
    path: &RepoPath,
) -> Anchor {
    if let Some(base) = base {
        let known = by_node(base);
        let mut cursor = Some(site.to_vec());
        while let Some(current) = cursor {
            if let Some(node_id) = result.ids.get(&current)
                && known.contains_key(node_id)
            {
                return Anchor::Definition(*node_id);
            }
            cursor = result_parents.get(&current).cloned();
        }
    }
    Anchor::File(path.clone())
}

#[cfg(test)]
mod tests {
    use hord_core::{Actor, Intent, RepoPath};

    use crate::{BeginOptions, Repo, Workspace};

    fn actor() -> Actor {
        Actor::Agent {
            id: "ctx".into(),
            model: "test".into(),
            model_hash: Bytes::default(),
            harness: "semantic tests".into(),
        }
    }

    fn intent(summary: &str) -> Intent {
        Intent {
            summary: summary.into(),
            body: String::new(),
            refs: Vec::new(),
            acceptance: Vec::new(),
        }
    }

    fn path(p: &str) -> RepoPath {
        p.parse().unwrap()
    }

    use super::*;

    async fn land(repo: &Repo, edit: impl AsyncFnOnce(&mut Workspace)) {
        let mut ws = repo.begin(BeginOptions::at_head(actor())).await.unwrap();
        edit(&mut ws).await;
        let before = repo.head().await.unwrap().snapshot;
        let proposal = ws.propose(intent("step")).await.unwrap();
        repo.submit(proposal.change).await.unwrap();
        repo.land_local().await.unwrap();
        assert_ne!(repo.head().await.unwrap().snapshot, before, "landed");
    }

    /// The head's context, built incrementally from the previous head's,
    /// equals a full rebuild after every landing (perf review #1).
    #[tokio::test]
    async fn incremental_context_equals_full_rebuild_over_landings() {
        let dir = std::env::temp_dir().join(format!("hord-txn-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repo::create(&dir).await.unwrap();
        let files = [
            ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n"),
            ("a/Cargo.toml", "[package]\nname = \"a\"\n"),
            (
                "b/Cargo.toml",
                "[package]\nname = \"b\"\n[dependencies]\na = { path = \"../a\" }\n",
            ),
            (
                "a/src/lib.rs",
                "mod x;\npub use x::helper;\n\npub fn f() -> u32 {\n    1\n}\n",
            ),
            (
                "a/src/x.rs",
                "pub fn helper() -> u32 {\n    crate::f()\n}\n",
            ),
            (
                "b/src/lib.rs",
                "use a::helper;\n\npub fn g() -> u32 {\n    helper() + a::f()\n}\n",
            ),
            ("README.md", "fixture\n"),
        ];
        let files = files
            .iter()
            .map(|(p, s)| (path(p), s.as_bytes().to_vec()))
            .collect();
        repo.bootstrap(files, intent("bootstrap"), actor())
            .await
            .unwrap();

        let check = |step: &str| {
            let inner = &repo.inner;
            let head = inner.head().unwrap().snapshot;
            let full = inner.build_rust_ctx_full(head).unwrap();
            let inc = inner.rust_ctx(head).unwrap();
            assert!(
                inc.ctx.same_contents(&full),
                "{step}: incremental context differs from a full rebuild"
            );
            assert!(full.definition_count() > 0);
        };
        check("bootstrap");

        land(&repo, async |ws| {
            ws.write_file(
                &path("a/src/x.rs"),
                "pub fn helper() -> u32 {\n    crate::f() + 1\n}\n",
            )
            .await
            .unwrap();
        })
        .await;
        assert!(repo.inner.latest_rust_ctx().is_some(), "propose built one");
        check("edit a body");

        land(&repo, async |ws| {
            ws.write_file(
                &path("a/src/lib.rs"),
                "mod x;\nmod y;\npub use x::helper;\n\npub fn f() -> u32 {\n    1\n}\n",
            )
            .await
            .unwrap();
            ws.write_file(&path("a/src/y.rs"), "pub struct Y;\n")
                .await
                .unwrap();
        })
        .await;
        check("add a module");

        land(&repo, async |ws| {
            ws.write_file(
                &path("a/src/x.rs"),
                "pub fn helper_renamed() -> u32 {\n    crate::f() + 1\n}\n",
            )
            .await
            .unwrap();
            ws.write_file(
                &path("a/src/lib.rs"),
                "mod x;\nmod y;\npub use x::helper_renamed as helper;\n\npub fn f() -> u32 {\n    1\n}\n",
            )
            .await
            .unwrap();
        })
        .await;
        check("rename (carried identity)");

        land(&repo, async |ws| {
            ws.delete_file(&path("a/src/y.rs")).await.unwrap();
            ws.write_file(&path("b/Cargo.toml"), "[package]\nname = \"b\"\n")
                .await
                .unwrap();
        })
        .await;
        check("delete a file and a dependency");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
