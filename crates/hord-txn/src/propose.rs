//! `propose()`: diff, identify, build and store a [`ChangeRecord`] (spec §6.2).
//!
//! # Op layout (ADR 0015)
//!
//! A parsed file's edit is its structural ops ([`hord_diff::diff`]). They
//! name their file through [`hord_diff::file_parent`]`(path)`, the parent of
//! file-level definitions and the node a glue edit replaces, so they do not
//! depend on their position in `ops`. [`Op::Blob`] appears only for new or
//! deleted content (with [`Op::Tree`] `CreateFile` before / `Delete` after,
//! as in `hord-git`) and for files without a structural diff: no adapter, or
//! a result that does not parse. A `Blob` op whose file root no structural
//! op names is a coarse (whole-file) write.
//!
//! # Sets
//!
//! `write_set` is every definition the ops touch (the file root for a glue
//! edit), plus births, deaths, and derivations, and the path id
//! ([`crate::path_node_id`], equal to the file root id) for created files,
//! blob-tier and coarse writes, with every base definition of a coarsely
//! written parsed file.
//!
//! `read_set` follows ADR 0012: the access log (definitions, and path ids of
//! files read), one hop of outgoing `References` from every written
//! definition in the base and in the result, and declarations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hord_core::{
    Actor, Bytes, ChangeId, ChangeRecord, IdentityDelta, Intent, NodeId, ObjectId, Op, Provenance,
    RepoPath, SnapshotId, TreeOpKind,
};
use hord_lang::{Anchor, IdentifiedTree, IdentityMapping};
use serde::{Deserialize, Serialize};

use crate::ids::path_node_id;
use crate::repo::{Inner, lock, now};
use crate::semantic::{RustCtx, by_node, definitions, enclosing, result_anchor};
use crate::workspace::{AccessLog, Proposal};
use crate::{Error, Result};

/// A read the access log cannot see, declared by the author (ADR 0012).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ReadDeclaration {
    /// A definition by identity.
    Node(NodeId),
    /// Definitions by qualified name, as the adapter names them. Resolved
    /// against the base and the result at propose time.
    Name(String),
    /// A whole file: its path and every definition in it.
    Path(RepoPath),
}

pub(crate) struct ProposeInput {
    pub base: SnapshotId,
    pub parents: Vec<ChangeId>,
    /// Changed paths and their new content; `None` is a deletion.
    pub changes: BTreeMap<RepoPath, Option<Bytes>>,
    pub access: AccessLog,
    pub declared: Vec<ReadDeclaration>,
    pub intent: Intent,
    pub actor: Actor,
    pub session: Option<String>,
}

/// A parsed file this change wrote, kept for the reference hop.
struct Written {
    path: RepoPath,
    rust: bool,
    base: Option<Arc<IdentifiedTree>>,
    result: Option<Arc<IdentifiedTree>>,
    nodes: BTreeSet<NodeId>,
}

/// Build the record. With `store`, store it and remember it as checked;
/// without, return the id it would have (blobs, trees, and identity objects
/// are still written: they are content-addressed).
pub(crate) fn propose(inner: &Inner, input: ProposeInput, store: bool) -> Result<Proposal> {
    let base = input.base;
    let mut ops = Vec::new();
    let mut write_set = BTreeSet::new();
    let mut deltas = Vec::new();
    let mut tree_changes = BTreeMap::new();
    let mut index = (*inner.identity_index(base)?).clone();
    let mut written = Vec::new();
    let mut read_set = BTreeSet::new();

    for (path, result) in &input.changes {
        let base_view = inner.file_view(base, path)?;
        let base_blob = base_view.as_ref().map(|v| v.blob);
        let result_blob = match result {
            Some(bytes) => Some(inner.put_blob(bytes.as_slice())?),
            None => None,
        };
        if base_blob == result_blob {
            continue;
        }
        tree_changes.insert(path.clone(), result_blob);
        if base_blob.is_none() {
            // Creating a file writes its path (the file root, ADR 0015): a
            // read of the missing path, or another creation of it, overlaps.
            write_set.insert(path_node_id(path));
        }
        // Op::Blob only for new or deleted content and for files without a
        // structural diff (ADR 0015); a parsed edit is its structural ops.
        let blob_op = Op::Blob {
            path: path.clone(),
            from: base_blob,
            to: result_blob,
        };
        if base_blob.is_none() {
            ops.push(Op::Tree {
                path: path.clone(),
                kind: TreeOpKind::CreateFile,
            });
            ops.push(blob_op.clone());
        } else if result_blob.is_none() {
            ops.push(blob_op.clone());
        }
        let base_parsed = base_view.as_ref().and_then(|v| v.parsed.clone());
        let head = match (result, &base_view) {
            (Some(bytes), _) => bytes.clone(),
            (None, Some(view)) => view.bytes.clone(),
            (None, None) => Bytes::default(),
        };
        let adapter = inner.adapter(path, head.as_slice());
        // A parsed base must parse for a structural diff; a new file needs none.
        let structural = match (adapter, result, result_blob) {
            (Some(adapter), Some(bytes), Some(blob))
                if base_view.is_none() || base_parsed.is_some() =>
            {
                inner
                    .parse(adapter, blob, bytes.as_slice())
                    .map(|tree| (adapter, tree, bytes, blob))
            }
            _ => None,
        };
        match structural {
            Some((adapter, result_tree, bytes, blob)) => {
                let empty = IdentifiedTree::default();
                let base_tree = base_parsed.as_ref().map(|p| Arc::clone(&p.tree));
                let base_ref = base_tree.as_deref().unwrap_or(&empty);
                let mapping = hord_identity::carry_in(adapter, path, base_ref, &result_tree, &[])
                    .map_err(|source| Error::Identity {
                    path: path.clone(),
                    source,
                })?;
                let file_ops = hord_diff::diff(path, base_ref, &result_tree, &mapping);
                check_reproduces(adapter, path, base_ref, &file_ops, &result_tree, bytes)?;
                deltas.extend(mapping.deltas.iter().cloned());
                ops.extend(file_ops);
                let result_tree = Arc::new(IdentifiedTree::new(
                    (*result_tree).clone(),
                    mapping.nodes.clone(),
                ));
                let nodes = written_nodes(base_ref, &result_tree, &mapping, path_node_id(path));
                write_set.extend(nodes.iter().copied());
                let identity = inner.put_file_identity(path, blob, &result_tree)?;
                index.set(path, Some(identity));
                written.push(Written {
                    path: path.clone(),
                    rust: adapter.lang().as_str() == hord_lang_rust::LANG,
                    base: base_tree,
                    result: Some(result_tree),
                    nodes,
                });
            }
            None => {
                if base_blob.is_some() && result_blob.is_some() {
                    ops.push(blob_op);
                }
                write_set.insert(path_node_id(path));
                index.set(path, None);
                if let Some(parsed) = &base_parsed {
                    let nodes: BTreeSet<NodeId> = parsed.tree.ids.values().copied().collect();
                    write_set.extend(nodes.iter().copied());
                    deltas.extend(
                        nodes
                            .iter()
                            .map(|node| IdentityDelta::Death { node: *node }),
                    );
                    written.push(Written {
                        path: path.clone(),
                        rust: parsed.lang.as_str() == hord_lang_rust::LANG,
                        base: Some(Arc::clone(&parsed.tree)),
                        result: None,
                        nodes,
                    });
                }
            }
        }
        if result_blob.is_none() {
            ops.push(Op::Tree {
                path: path.clone(),
                kind: TreeOpKind::Delete,
            });
        }
    }
    if tree_changes.is_empty() {
        return Err(Error::NothingToPropose);
    }
    for delta in &deltas {
        write_set.extend(delta_nodes(delta));
    }
    write_set.remove(&NodeId::nil());

    let result = inner.update_tree(base, &tree_changes)?;
    inner.put_identity_index(result, index)?;

    read_set.extend(input.access.reads.iter().copied());
    read_set.extend(input.access.read_paths.iter().map(path_node_id));
    references(inner, base, &written, &mut read_set)?;
    declarations(inner, base, result, &input.declared, &mut read_set)?;
    read_set.remove(&NodeId::nil());

    let record = ChangeRecord {
        base,
        result,
        parents: input.parents,
        ops,
        intent: input.intent,
        provenance: Provenance {
            actor: input.actor,
            toolchain: inner.toolchain,
            created_at: now(),
            session: input.session,
            parent_intent: None,
        },
        read_set,
        write_set,
        identity_deltas: deltas,
        evidence: Vec::new(),
        signature: None,
    };
    let change = if store {
        let change = inner.store.put_object(&record)?;
        lock(&inner.proposed).insert(change);
        change
    } else {
        ObjectId::of(&record)?
    };
    Ok(Proposal { change, record })
}

/// `apply(base, ops)` must project to exactly `bytes` (spec §3.5, §5.1).
pub(crate) fn check_reproduces(
    adapter: &dyn hord_lang::LangAdapter,
    path: &RepoPath,
    base: &IdentifiedTree,
    ops: &[Op],
    store: &hord_lang::NodeTree,
    bytes: &Bytes,
) -> Result<()> {
    let applied =
        hord_diff::apply(path, base, ops, store).map_err(|err| Error::OpsDoNotReproduce {
            path: path.clone(),
            reason: err.to_string(),
        })?;
    if adapter.project(&applied.tree).as_slice() != bytes.as_slice() {
        return Err(Error::OpsDoNotReproduce {
            path: path.clone(),
            reason: "projection differs from the result bytes".into(),
        });
    }
    Ok(())
}

/// Definitions a file's change writes (ADR 0015 amendment: write sets do
/// not come from ops). A carried definition is written when its own
/// content (the leaves outside nested definitions) or its parent changed.
/// Births, deaths, and derivations are writes, and so is the file root
/// (`root`) when the file's glue changed.
pub(crate) fn written_nodes(
    base: &IdentifiedTree,
    result: &IdentifiedTree,
    mapping: &IdentityMapping,
    root: NodeId,
) -> BTreeSet<NodeId> {
    let before = own_contents(base);
    let after = own_contents(result);
    let mut out = BTreeSet::new();
    for (node, content) in &after {
        if before.get(node).is_some_and(|was| was != content) {
            out.insert(*node);
        }
    }
    if root_glue(base) != root_glue(result) {
        out.insert(root);
    }
    for delta in &mapping.deltas {
        out.extend(delta_nodes(delta));
    }
    out.remove(&NodeId::nil());
    out
}

/// Each definition's own content (leaf content ids outside nested
/// definitions, list separators skipped) and its parent's id.
fn own_contents(tree: &IdentifiedTree) -> BTreeMap<NodeId, (Vec<ObjectId>, Option<NodeId>)> {
    let parents = enclosing(tree);
    tree.ids
        .iter()
        .map(|(site, id)| {
            let parent = parents.get(site).and_then(|p| tree.ids.get(p).copied());
            (*id, (own_leaves(tree, site), parent))
        })
        .collect()
}

/// Leaves of the file root outside every definition.
fn root_glue(tree: &IdentifiedTree) -> Vec<ObjectId> {
    if tree.tree.root().is_none() {
        return Vec::new();
    }
    own_leaves(tree, &[])
}

/// Leaf content ids under `site`, skipping nested definition sites and the
/// `,`/`;` separators between child definitions (adding a field is a birth,
/// not a write of the struct). A skipped separator that carries a comment
/// still counts: the comment is content of the enclosing definition.
fn own_leaves(tree: &IdentifiedTree, site: &[u32]) -> Vec<ObjectId> {
    fn walk(
        tree: &IdentifiedTree,
        oid: ObjectId,
        site: &mut Vec<u32>,
        top: bool,
        separator: bool,
        out: &mut Vec<ObjectId>,
    ) {
        if !top && tree.ids.contains_key(site.as_slice()) {
            return;
        }
        let Some(node) = tree.tree.get(oid) else {
            return;
        };
        if node.children.is_empty() {
            if !separator || has_comment(&node.raw) {
                out.push(oid);
            }
            return;
        }
        for (i, child) in node.children.iter().enumerate() {
            let separator = is_separator(tree, &node.children, site, i);
            site.push(u32::try_from(i).unwrap_or(u32::MAX));
            walk(tree, *child, site, false, separator, out);
            site.pop();
        }
    }
    /// Child `i` of the node at `site` is a `,`/`;` leaf next to a child
    /// definition.
    fn is_separator(tree: &IdentifiedTree, children: &[ObjectId], site: &[u32], i: usize) -> bool {
        let Some(child) = tree.tree.get(children[i]) else {
            return false;
        };
        if !child.children.is_empty() || !matches!(child.kind.as_str(), "," | ";") {
            return false;
        }
        let is_def = |j: usize| {
            let mut at = site.to_vec();
            at.push(u32::try_from(j).unwrap_or(u32::MAX));
            tree.ids.contains_key(at.as_slice())
        };
        (i > 0 && is_def(i - 1)) || (i + 1 < children.len() && is_def(i + 1))
    }
    /// A separator's raw bytes hold more than the token and whitespace.
    fn has_comment(raw: &hord_core::Bytes) -> bool {
        raw.as_slice()
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            .nth(1)
            .is_some()
    }
    let mut out = Vec::new();
    if let Some(oid) = tree.oid_at(site) {
        walk(tree, oid, &mut site.to_vec(), true, false, &mut out);
    }
    out
}

pub(crate) fn delta_nodes(delta: &IdentityDelta) -> Vec<NodeId> {
    match delta {
        IdentityDelta::Birth { node } | IdentityDelta::Death { node } => vec![*node],
        IdentityDelta::DerivedFrom { node, from } => vec![*node, *from],
        IdentityDelta::SplitInto { node, into } => {
            let mut out = vec![*node];
            out.extend(into.iter().copied());
            out
        }
        IdentityDelta::MergedFrom { node, from } => {
            let mut out = vec![*node];
            out.extend(from.iter().copied());
            out
        }
    }
}

/// One hop of outgoing `References` from written Rust definitions, in the
/// base and in the result (ADR 0012).
fn references(
    inner: &Inner,
    base: SnapshotId,
    written: &[Written],
    out: &mut BTreeSet<NodeId>,
) -> Result<()> {
    let mut ctx: Option<Arc<RustCtx>> = None;
    for file in written.iter().filter(|w| w.rust) {
        let ctx = match &ctx {
            Some(ctx) => Arc::clone(ctx),
            None => {
                let built = inner.rust_ctx(base)?;
                ctx = Some(Arc::clone(&built));
                built
            }
        };
        if let Some(base_tree) = &file.base {
            let by = by_node(base_tree);
            for node_id in &file.nodes {
                if let Some(site) = by.get(node_id)
                    && let Some(node) = base_tree.node_at(site)
                {
                    let anchor = Anchor::Definition(*node_id);
                    inner.rust_references(&ctx, base, *node_id, &anchor, node, out)?;
                }
            }
        }
        if let Some(result_tree) = &file.result {
            let by = by_node(result_tree);
            let parents = enclosing(result_tree);
            for node_id in &file.nodes {
                if let Some(site) = by.get(node_id)
                    && let Some(node) = result_tree.node_at(site)
                {
                    let anchor = result_anchor(
                        file.base.as_deref(),
                        result_tree,
                        site,
                        &parents,
                        &file.path,
                    );
                    inner.rust_references(&ctx, base, *node_id, &anchor, node, out)?;
                }
            }
        }
    }
    Ok(())
}

/// Resolve declared reads into the read set.
fn declarations(
    inner: &Inner,
    base: SnapshotId,
    result: SnapshotId,
    declared: &[ReadDeclaration],
    out: &mut BTreeSet<NodeId>,
) -> Result<()> {
    let mut names: BTreeMap<&str, bool> = BTreeMap::new();
    for declaration in declared {
        match declaration {
            ReadDeclaration::Node(node) => {
                out.insert(*node);
            }
            ReadDeclaration::Path(path) => {
                out.insert(path_node_id(path));
                for snapshot in [base, result] {
                    if let Some(view) = inner.file_view(snapshot, path)?
                        && let Some(parsed) = view.parsed
                    {
                        out.extend(parsed.tree.ids.values().copied());
                    }
                }
            }
            ReadDeclaration::Name(name) => {
                names.insert(name.as_str(), false);
            }
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    for snapshot in [base, result] {
        for (path, _) in inner.list_files(snapshot)? {
            let Some(view) = inner.file_view(snapshot, &path)? else {
                continue;
            };
            let Some(parsed) = view.parsed else {
                continue;
            };
            let Some(adapter) = inner.adapter_for(&path, parsed.lang) else {
                continue;
            };
            for def in definitions(adapter, &path, &parsed.tree) {
                if let Some(name) = &def.name
                    && let Some(found) = names.get_mut(name.as_str())
                {
                    *found = true;
                    out.insert(def.node);
                }
            }
        }
    }
    match names.into_iter().find(|(_, found)| !found) {
        Some((name, _)) => Err(Error::UnresolvedDeclaration(name.to_owned())),
        None => Ok(()),
    }
}
