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
    Bytes, IdentityMap, LangId, Node, NodeId, NodeKind, NodePath, ObjectId, QualifiedName,
    RepoPath, SnapshotId,
};
use hord_lang::{Anchor, IdentifiedTree, LangAdapter, NodeTree, ResolveCtx, Site};
use hord_lang_rust::{ManifestFile, RustAdapter, RustFile};
use hord_store::EdgeKind;
use serde::{Deserialize, Serialize};

use crate::repo::{Inner, lock};
use crate::{Error, Result};

/// Files of one snapshot whose [`NodeId`]s differ from a fresh assignment.
///
/// Stored as canonical CBOR; the pairs are sorted by path.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct IdentityIndex {
    pub files: Vec<(RepoPath, ObjectId)>,
}

impl IdentityIndex {
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
            return Some(Arc::clone(tree));
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
            Some(id) => self.store.get_object::<IdentityIndex>(id)?,
            None => IdentityIndex::default(),
        };
        let index = Arc::new(index);
        let mut cache = lock(&self.indexes);
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(snapshot, Arc::clone(&index));
        Ok(index)
    }

    pub(crate) fn put_identity_index(
        &self,
        snapshot: SnapshotId,
        index: IdentityIndex,
    ) -> Result<()> {
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
            return Ok(Some(Parsed {
                lang,
                tree: Arc::clone(tree),
            }));
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

    fn build_rust_ctx(&self, snapshot: SnapshotId) -> Result<Arc<RustCtx>> {
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
        let views: Vec<RustFile<'_>> = trees
            .iter()
            .map(|(path, tree)| RustFile {
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
        let ctx = rust.resolve_context_with(&views, &manifest_views);
        Ok(Arc::new(RustCtx { ctx }))
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
