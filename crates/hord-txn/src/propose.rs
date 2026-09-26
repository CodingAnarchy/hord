//! `propose()`: diff, identify, build and store a [`ChangeRecord`] (spec §6.2).
//!
//! # Op layout (ADR 0015)
//!
//! A parsed file's edit is its structural ops ([`hord_diff::diff`]). They
//! name their file through [`NodeId::file_root`]`(path)`, the parent of
//! file-level definitions and the node a glue edit replaces, so they do not
//! depend on their position in `ops`. [`Op::Blob`] appears only for new or
//! deleted content (with [`Op::Tree`] `CreateFile` before / `Delete` after,
//! as in `hord-git`) and for files without a structural diff: no adapter, or
//! a result that does not parse. A `Blob` op whose file root no structural
//! op names is a coarse (whole-file) write.
//!
//! # File moves (ADR 0020)
//!
//! A parsed file the change deletes is paired with a file it creates, by
//! blob equality first and then by definition overlap at ADR 0007's 0.8.
//! Identity carries from the old file to the new one; the ops are
//! `Op::Tree { path: old, kind: Rename { to: new } }`, a `Move` of each
//! carried top-level definition from the old file root to the new one, and
//! the structural diff of the old content to the new. Births in the new
//! file are derived from its path.
//!
//! # Sets
//!
//! `write_set` and `identity_deltas` come from comparing the base and result
//! snapshots ([`crate::sets::sets_between`], shared with the lander's rebased
//! records): every definition whose own content or parent changed, births,
//! deaths, derivations, moved definitions, and the path id
//! ([`crate::NodeId::file_root`], equal to the file root id) for created,
//! deleted, moved, blob-tier, and coarsely written files and for glue
//! edits.
//!
//! `read_set` follows ADR 0012: the access log (definitions, and path ids of
//! files read), one hop of outgoing `References` from every written
//! definition in the base and in the result, and declarations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hord_core::{
    Actor, Bytes, ChangeId, ChangeRecord, IdentityDelta, Intent, LangId, NodeId, ObjectId, Op,
    Provenance, RepoPath, SnapshotId, Timestamp, TreeOpKind,
};
use hord_lang::{Anchor, IdentifiedTree, NodeTree, Site, enclosing_site};
use serde::{Deserialize, Serialize};

use crate::repo::Inner;
use crate::semantic::{FileView, Parsed, RustCtx, carry, enclosing, result_anchor};
use crate::sets::sets_between;
use crate::snapshot::IdentityEdits;
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
}

/// One changed path: its base view and new content.
struct Edit {
    path: RepoPath,
    base: Option<FileView>,
    result: Option<Bytes>,
    blob: Option<ObjectId>,
}

impl Edit {
    fn base_blob(&self) -> Option<ObjectId> {
        self.base.as_ref().map(|v| v.blob)
    }

    fn base_parsed(&self) -> Option<&Parsed> {
        self.base.as_ref().and_then(|v| v.parsed.as_ref())
    }
}

/// Everything `propose` accumulates across files.
#[derive(Default)]
struct Build<'a> {
    ops: Vec<Op>,
    /// Derivations carrying recorded (births and deaths come from
    /// [`sets_between`]).
    declared: Vec<IdentityDelta>,
    tree: BTreeMap<RepoPath, Option<ObjectId>>,
    identity: IdentityEdits,
    written: Vec<Written>,
    /// Parsed results whose identity is stored once every file is
    /// identified: a definition moved between files keeps its id
    /// ([`carry_across_files`], ADR 0033).
    pending: Vec<Pending<'a>>,
}

/// A parsed result file, identified on its own, before ADR 0033's pass.
struct Pending<'a> {
    adapter: &'a dyn hord_lang::LangAdapter,
    path: RepoPath,
    blob: ObjectId,
    tree: IdentifiedTree,
    /// Its entry in [`Build::written`].
    written: usize,
}

/// Build the record. With `store`, store it and remember it as checked;
/// without, return the id it would have (blobs, trees, snapshots, and
/// identity objects are still written: they are content-addressed).
pub(crate) fn propose(inner: &Inner, input: ProposeInput, store: bool) -> Result<Proposal> {
    let base = input.base;
    let mut edits = Vec::new();
    for (path, result) in &input.changes {
        let base_view = inner.file_view(base, path)?;
        let blob = match result {
            Some(bytes) => Some(inner.put_blob(bytes.as_slice())?),
            None => None,
        };
        if base_view.as_ref().map(|v| v.blob) == blob {
            continue;
        }
        edits.push(Edit {
            path: path.clone(),
            base: base_view,
            result: result.clone(),
            blob,
        });
    }
    if edits.is_empty() {
        return Err(Error::NothingToPropose);
    }
    let moves = pair_moves(inner, &edits)?;
    let mut build = Build::default();
    for (i, edit) in edits.iter().enumerate() {
        match moves.get(&i) {
            Some(Pairing::From) => {}
            Some(Pairing::To(from)) => propose_move(inner, base, &edits[*from], edit, &mut build)?,
            None => propose_file(inner, base, edit, &mut build)?,
        }
    }
    // A moved file's source is deleted by the rename.
    for (i, pairing) in &moves {
        if matches!(pairing, Pairing::From) {
            build.tree.insert(edits[*i].path.clone(), None);
            build.identity.insert(edits[*i].path.clone(), None);
        }
    }
    let bases: Vec<(RepoPath, Arc<IdentifiedTree>)> = edits
        .iter()
        .filter_map(|e| {
            e.base_parsed()
                .map(|p| (e.path.clone(), Arc::clone(&p.tree)))
        })
        .collect();
    carry_across_files(&mut build, &bases);
    for pending in std::mem::take(&mut build.pending) {
        let tree = Arc::new(pending.tree);
        let identity =
            inner.put_file_identity(pending.adapter, &pending.path, pending.blob, &tree)?;
        build.identity.insert(pending.path.clone(), identity);
        build.written[pending.written].result = Some(tree);
    }

    let result = inner.commit_snapshot(base, &build.tree, &build.identity)?;
    let (write_set, deltas) = sets_between(inner, base, result, &build.ops, &build.declared)?;

    let mut read_set = BTreeSet::new();
    read_set.extend(input.access.reads.iter().copied());
    read_set.extend(input.access.read_paths.iter().map(NodeId::file_root));
    references(inner, base, &build.written, &write_set, &mut read_set)?;
    declarations(inner, base, result, &input.declared, &mut read_set)?;
    read_set.remove(&NodeId::nil());

    let record = ChangeRecord {
        base,
        result,
        parents: input.parents,
        ops: build.ops,
        intent: input.intent,
        provenance: Provenance {
            actor: input.actor,
            toolchain: inner.toolchain,
            created_at: Timestamp::now(),
            session: input.session,
            parent_intent: None,
            voucher: None,
        },
        read_set,
        write_set,
        identity_deltas: deltas,
        evidence: Vec::new(),
        signature: None,
        rebased_from: None,
    };
    let change = if store {
        let change = inner.store.put_object(&record)?;
        inner.note_proposed(change);
        change
    } else {
        ObjectId::of(&record)?
    };
    Ok(Proposal { change, record })
}

/// One changed file that is not part of a move.
fn propose_file<'a>(
    inner: &'a Inner,
    base: SnapshotId,
    edit: &Edit,
    build: &mut Build<'a>,
) -> Result<()> {
    let path = &edit.path;
    let (base_blob, result_blob) = (edit.base_blob(), edit.blob);
    build.tree.insert(path.clone(), result_blob);
    // Op::Blob only for new or deleted content and for files without a
    // structural diff (ADR 0015); a parsed edit is its structural ops.
    let blob_op = Op::Blob {
        path: path.clone(),
        from: base_blob,
        to: result_blob,
    };
    if base_blob.is_none() {
        build.ops.push(Op::Tree {
            path: path.clone(),
            kind: TreeOpKind::CreateFile,
        });
        build.ops.push(blob_op.clone());
    } else if result_blob.is_none() {
        build.ops.push(blob_op.clone());
    }
    let base_parsed = edit.base_parsed();
    let head = match (&edit.result, &edit.base) {
        (Some(bytes), _) => bytes.clone(),
        (None, Some(view)) => view.bytes.clone(),
        (None, None) => Bytes::default(),
    };
    let adapter = inner.adapter(path, head.as_slice());
    // A parsed base must parse for a structural diff; a new file needs none.
    let structural = match (adapter, &edit.result, result_blob) {
        (Some(adapter), Some(bytes), Some(blob))
            if edit.base.is_none() || base_parsed.is_some() =>
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
            let base_tree = base_parsed.map(|p| Arc::clone(&p.tree));
            let base_ref = base_tree.as_deref().unwrap_or(&empty);
            let mapping = carry(adapter, path, base, base_ref, &result_tree)?;
            let file_ops = hord_diff::diff(path, base_ref, &result_tree, &mapping);
            check_reproduces(adapter, path, base_ref, &file_ops, &result_tree, bytes)?;
            build.declared.extend(declared(&mapping.deltas));
            build.ops.extend(file_ops);
            build.pending.push(Pending {
                adapter,
                path: path.clone(),
                blob,
                tree: IdentifiedTree::new((*result_tree).clone(), mapping.nodes.clone()),
                written: build.written.len(),
            });
            build.written.push(Written {
                path: path.clone(),
                rust: adapter.lang().as_str() == hord_lang_rust::LANG,
                base: base_tree,
                result: None,
            });
        }
        None => {
            if base_blob.is_some() && result_blob.is_some() {
                build.ops.push(blob_op);
            }
            build.identity.insert(path.clone(), None);
            if let Some(parsed) = base_parsed {
                build.written.push(Written {
                    path: path.clone(),
                    rust: parsed.lang.as_str() == hord_lang_rust::LANG,
                    base: Some(Arc::clone(&parsed.tree)),
                    result: None,
                });
            }
        }
    }
    if result_blob.is_none() {
        build.ops.push(Op::Tree {
            path: path.clone(),
            kind: TreeOpKind::Delete,
        });
    }
    Ok(())
}

/// A deleted file paired with a created one (ADR 0020): identity carries
/// from the old file's definitions, the path moves with `Op::Tree`
/// `Rename`, and each carried top-level definition `Move`s from the old
/// file root to the new one. The remaining ops are the structural diff of
/// the old file's content to the new file's.
fn propose_move<'a>(
    inner: &'a Inner,
    base: SnapshotId,
    from: &Edit,
    to: &Edit,
    build: &mut Build<'a>,
) -> Result<()> {
    let (Some(old), Some(bytes), Some(blob)) = (from.base_parsed(), &to.result, to.blob) else {
        return Err(Error::NotParsed(to.path.clone()));
    };
    let path = &to.path;
    let adapter = inner
        .adapter_for(&from.path, old.lang)
        .ok_or_else(|| Error::NotParsed(from.path.clone()))?;
    let result_tree = inner
        .parse(adapter, blob, bytes.as_slice())
        .ok_or_else(|| Error::NotParsed(path.clone()))?;
    let mapping = carry(adapter, path, base, &old.tree, &result_tree)?;
    let file_ops = hord_diff::diff(path, &old.tree, &result_tree, &mapping);
    check_reproduces(adapter, path, &old.tree, &file_ops, &result_tree, bytes)?;
    let (old_root, new_root) = (NodeId::file_root(&from.path), NodeId::file_root(path));
    build.ops.push(Op::Tree {
        path: from.path.clone(),
        kind: TreeOpKind::Rename { to: path.clone() },
    });
    let result_tree = Arc::new(IdentifiedTree::new(
        (*result_tree).clone(),
        mapping.nodes.clone(),
    ));
    let carried: BTreeSet<NodeId> = old.tree.ids.values().copied().collect();
    for (site, node) in &result_tree.ids {
        if enclosing_site(&result_tree.ids, site).is_some() || !carried.contains(node) {
            continue;
        }
        build.ops.push(Op::Move {
            node: *node,
            from_parent: old_root,
            to_parent: new_root,
            index: site.last().copied().unwrap_or(0),
        });
    }
    build.ops.extend(file_ops);
    build.declared.extend(declared(&mapping.deltas));
    build.tree.insert(path.clone(), Some(blob));
    build.pending.push(Pending {
        adapter,
        path: path.clone(),
        blob,
        tree: (*result_tree).clone(),
        written: build.written.len(),
    });
    build.written.push(Written {
        path: path.clone(),
        rust: adapter.lang().as_str() == hord_lang_rust::LANG,
        base: Some(Arc::clone(&old.tree)),
        result: None,
    });
    Ok(())
}

/// A definition identified in one file of the change that no longer holds
/// it, or born in one: a candidate end of a cross-file move (ADR 0033).
struct End {
    node: NodeId,
    /// Its parent: the enclosing definition, or the file root.
    parent: NodeId,
}

/// Per `normalized` hash: the deaths, and the births as (pending file
/// index, site).
type Ends = (Vec<End>, Vec<(usize, Site)>);

/// ADR 0033: a definition moved unchanged to another file keeps its id.
/// Among the definitions this change removed from one file (`bases`: every
/// changed file's base tree) and added to another, pair a death with a
/// birth when they have the same `normalized` hash and no other death or
/// birth shares it. The birth takes the dead id, and a `Move` from the old
/// parent to the new one is emitted unless the parent moved with it. The
/// files' own `Delete` and `Insert` ops stay: each file still reproduces
/// on its own (the `Move` is left out of per-file replay).
fn carry_across_files(build: &mut Build<'_>, bases: &[(RepoPath, Arc<IdentifiedTree>)]) {
    let result_ids: BTreeSet<NodeId> = build
        .pending
        .iter()
        .flat_map(|p| p.tree.ids.values().copied())
        .collect();
    let base_ids: BTreeSet<NodeId> = bases
        .iter()
        .flat_map(|(_, t)| t.ids.values().copied())
        .collect();
    let hash = |tree: &IdentifiedTree, site: &Site| {
        hord_lang::oid_at(&tree.tree, site)
            .and_then(|oid| tree.tree.get(oid))
            .map(|node| node.normalized)
    };
    let mut by_hash: BTreeMap<ObjectId, Ends> = BTreeMap::new();
    for (path, tree) in bases {
        for (site, node) in &tree.ids {
            if result_ids.contains(node) {
                continue;
            }
            let Some(h) = hash(tree, site) else {
                continue;
            };
            let parent = enclosing_site(&tree.ids, site)
                .and_then(|s| tree.ids.get(s).copied())
                .unwrap_or_else(|| NodeId::file_root(path));
            by_hash.entry(h).or_default().0.push(End {
                node: *node,
                parent,
            });
        }
    }
    for (i, pending) in build.pending.iter().enumerate() {
        for (site, node) in &pending.tree.ids {
            if base_ids.contains(node) {
                continue;
            }
            if let Some(h) = hash(&pending.tree, site) {
                by_hash.entry(h).or_default().1.push((i, site.clone()));
            }
        }
    }
    // Unique pairs only: duplicated bodies stay births and deaths.
    let mut moved: Vec<(usize, Site, End)> = Vec::new();
    for (_, (mut deaths, mut births)) in by_hash {
        if let ([_], [_]) = (deaths.as_slice(), births.as_slice())
            && let (Some(death), Some((i, site))) = (deaths.pop(), births.pop())
        {
            moved.push((i, site, death));
        }
    }
    for (i, site, death) in &moved {
        build.pending[*i].tree.ids.insert(site.clone(), death.node);
    }
    for (i, site, death) in moved {
        let pending = &build.pending[i];
        let to_parent = enclosing_site(&pending.tree.ids, &site)
            .and_then(|s| pending.tree.ids.get(s).copied())
            .unwrap_or_else(|| NodeId::file_root(&pending.path));
        if to_parent != death.parent {
            build.ops.push(Op::Move {
                node: death.node,
                from_parent: death.parent,
                to_parent,
                index: site.last().copied().unwrap_or(0),
            });
        }
    }
}

/// Deltas carrying recorded besides births and deaths.
pub(crate) fn declared(deltas: &[IdentityDelta]) -> impl Iterator<Item = IdentityDelta> + '_ {
    deltas
        .iter()
        .filter(|d| !matches!(d, IdentityDelta::Birth { .. } | IdentityDelta::Death { .. }))
        .cloned()
}

/// Which side of a file move an edit is.
enum Pairing {
    /// The deleted source.
    From,
    /// The created target, with the index of its source.
    To(usize),
}

/// Pair the parsed files this change deletes with the files it creates
/// (ADR 0020): by blob equality first, then, among files still unpaired, by
/// the Dice overlap of their definitions' `normalized` hashes, best pair
/// first, at or above ADR 0007's 0.8. Both files must be parsed by the same
/// language.
fn pair_moves(inner: &Inner, edits: &[Edit]) -> Result<BTreeMap<usize, Pairing>> {
    let deleted: Vec<usize> = (0..edits.len())
        .filter(|i| edits[*i].blob.is_none() && edits[*i].base_parsed().is_some())
        .collect();
    let created: Vec<usize> = (0..edits.len())
        .filter(|i| edits[*i].base.is_none() && edits[*i].blob.is_some())
        .collect();
    let mut out = BTreeMap::new();
    if deleted.is_empty() || created.is_empty() {
        return Ok(out);
    }
    // Normalized hashes of each created file's definitions, if it parses in
    // the deleted file's language.
    let mut created_defs: BTreeMap<usize, (LangId, Vec<ObjectId>)> = BTreeMap::new();
    for &i in &created {
        let (Some(bytes), Some(blob)) = (&edits[i].result, edits[i].blob) else {
            continue;
        };
        let Some(adapter) = inner.adapter(&edits[i].path, bytes.as_slice()) else {
            continue;
        };
        let Some(tree) = inner.parse(adapter, blob, bytes.as_slice()) else {
            continue;
        };
        let fresh = hord_identity::assign(adapter, &edits[i].path, None, &tree);
        created_defs.insert(i, (adapter.lang(), normalized(&tree, fresh.nodes.keys())));
    }
    let mut taken: BTreeSet<usize> = BTreeSet::new();
    let pair = |out: &mut BTreeMap<usize, Pairing>, from: usize, to: usize| {
        out.insert(from, Pairing::From);
        out.insert(to, Pairing::To(from));
    };
    for &d in &deleted {
        let old = edits[d].base_parsed().map(|p| p.lang);
        let same = created.iter().copied().find(|c| {
            !taken.contains(c)
                && edits[*c].blob == edits[d].base_blob()
                && created_defs.get(c).map(|(lang, _)| Some(*lang)) == Some(old)
        });
        if let Some(c) = same {
            taken.insert(c);
            taken.insert(d);
            pair(&mut out, d, c);
        }
    }
    let mut scored = Vec::new();
    for &d in deleted.iter().filter(|d| !taken.contains(d)) {
        let Some(old) = edits[d].base_parsed() else {
            continue;
        };
        let old_defs = normalized(&old.tree.tree, old.tree.ids.keys());
        for (&c, (lang, new_defs)) in &created_defs {
            if taken.contains(&c) || *lang != old.lang {
                continue;
            }
            if let Some(score) = dice(&old_defs, new_defs) {
                scored.push((score, d, c));
            }
        }
    }
    // Best first; ties by path order of the source, then the target.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (score, d, c) in scored {
        if score < MOVE_THRESHOLD_PER_MILLE || taken.contains(&d) || taken.contains(&c) {
            continue;
        }
        taken.insert(d);
        taken.insert(c);
        pair(&mut out, d, c);
    }
    Ok(out)
}

/// ADR 0007's similarity threshold, 0.8, in thousandths.
const MOVE_THRESHOLD_PER_MILLE: u32 = 800;

/// The `normalized` hash of the node at each site, sorted.
fn normalized<'a>(tree: &NodeTree, sites: impl Iterator<Item = &'a Site>) -> Vec<ObjectId> {
    let mut out: Vec<ObjectId> = sites
        .filter_map(|site| tree.get(hord_lang::oid_at(tree, site)?))
        .map(|node| node.normalized)
        .collect();
    out.sort();
    out
}

/// Dice similarity of two sorted multisets, in thousandths. `None` when
/// both are empty.
fn dice(a: &[ObjectId], b: &[ObjectId]) -> Option<u32> {
    let total = a.len() + b.len();
    if total == 0 {
        return None;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    u32::try_from(2000 * common / total).ok()
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

/// One hop of outgoing `References` from written Rust definitions, in the
/// base and in the result (ADR 0012).
fn references(
    inner: &Inner,
    base: SnapshotId,
    written: &[Written],
    writes: &BTreeSet<NodeId>,
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
            for (site, node_id) in base_tree.ids.iter().filter(|(_, id)| writes.contains(id)) {
                if let Some(node) = base_tree.node_at(site) {
                    let anchor = Anchor::Definition(*node_id);
                    inner.rust_references(&ctx, base, *node_id, &anchor, node, out)?;
                }
            }
        }
        if let Some(result_tree) = &file.result {
            let parents = enclosing(result_tree);
            for (site, node_id) in result_tree.ids.iter().filter(|(_, id)| writes.contains(id)) {
                if let Some(node) = result_tree.node_at(site) {
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
                out.insert(NodeId::file_root(path));
                for snapshot in [base, result] {
                    if let Some(parsed) = inner.parsed_at(snapshot, path)? {
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
            for def in inner.definitions_at(snapshot, &path)? {
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
