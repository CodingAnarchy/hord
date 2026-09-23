//! Structural 3-way merge (spec §5.2).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hord_core::{Bytes, NodeId, ObjectId, Op};
use hord_lang::{IdentifiedTree, LangAdapter, NodeTree};

use crate::apply::apply;

use crate::graft::union_trees;

/// Hard vs soft conflict (spec §5.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ConflictKind {
    /// Cannot auto-resolve: same-node Replace with different `normalized`,
    /// Delete vs any other op, blob/binary, or unparseable result.
    Hard,
    /// Auto-resolved by the spec tie-break (landing order, then [`NodeId`]);
    /// flagged so a verifier re-checks (rule 1, same-index inserts).
    Soft,
}

/// Nodes in contention for a 3-way merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    /// Definition [`NodeId`]s involved. Empty for blob-only conflicts.
    pub nodes: Vec<NodeId>,
    /// Hard or soft (rule 1).
    pub kind: ConflictKind,
    /// Why the merge did not (or did, for soft) compose cleanly.
    pub reason: String,
    /// Spec §5.2 rule 3. Must not fall through to an auto-merge.
    pub(crate) delete_vs: bool,
}

impl Conflict {
    pub(crate) fn hard(nodes: Vec<NodeId>, reason: impl Into<String>) -> Self {
        Self {
            nodes,
            kind: ConflictKind::Hard,
            reason: reason.into(),
            delete_vs: false,
        }
    }

    fn delete_vs(nodes: Vec<NodeId>, reason: impl Into<String>) -> Self {
        Self {
            nodes,
            kind: ConflictKind::Hard,
            reason: reason.into(),
            delete_vs: true,
        }
    }

    fn soft(nodes: Vec<NodeId>, reason: impl Into<String>) -> Self {
        Self {
            nodes,
            kind: ConflictKind::Soft,
            reason: reason.into(),
            delete_vs: false,
        }
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            ConflictKind::Hard => "hard",
            ConflictKind::Soft => "soft",
        };
        write!(f, "{kind} conflict: {}", self.reason)
    }
}

/// Successful merge: tree plus any auto-resolved soft conflicts.
#[derive(Clone, Debug)]
pub struct MergeResult {
    /// Merged CST, re-parsed under the adapter (spec §5.2 last paragraph).
    pub tree: IdentifiedTree,
    /// Rule-1 soft conflicts that were tie-broken, not failed.
    pub soft: Vec<Conflict>,
}

/// 3-way merge of identified trees (spec §5.2 rules 1–5).
///
/// `ours` is the landed head; `theirs` is the proposed change. Both are
/// identified against `base`. Blob-tier files use [`crate::merge_blob`];
/// this function is the structural path. An unparseable result is a hard
/// conflict.
pub fn merge<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
) -> Result<MergeResult, Conflict> {
    let ours_map = crate::defs::mapping_between(base, ours);
    let theirs_map = crate::defs::mapping_between(base, theirs);
    let ours_ops = crate::diff::diff_structural(base, &ours.tree, &ours_map);
    let theirs_ops = crate::diff::diff_structural(base, &theirs.tree, &theirs_map);
    let mut store = union_trees(&base.tree, &ours.tree)
        .map_err(|e| Conflict::hard(Vec::new(), format!("union interned nodes: {e}")))?;
    store = union_trees(&store, &theirs.tree)
        .map_err(|e| Conflict::hard(Vec::new(), format!("union interned nodes: {e}")))?;
    match merge_ops(adapter, base, &ours_ops, &theirs_ops, &store) {
        // Delete vs anything else must not fall through to an auto-merge.
        Err(conflict) if conflict.delete_vs => Err(conflict),
        Ok(ok) => accept_composed(adapter, base, ours, theirs, ok),
        Err(structural) => {
            overlap_fallback(adapter, base, ours, theirs, &mut store).ok_or(structural)
        }
    }
}

/// Keep a structural compose unless it invented a line. Landing order is git's.
fn accept_composed<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
    ok: MergeResult,
) -> Result<MergeResult, Conflict> {
    let projected = adapter.project(&ok.tree.tree);
    let src = sources(adapter, base, ours, theirs);
    // A line in none of the three inputs was invented by the merge
    // (a comma glued onto the wrong token, a hand-style rewrite).
    if projection_invents_line(&src, projected.as_slice())
        && let Some(git) = git_ours_result(adapter, base, &src)
    {
        return Ok(git);
    }
    Ok(ok)
}

/// Overlapping edits. Keep non-conflicting edits from both sides and ours'
/// side of each conflict hunk.
fn overlap_fallback<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
    store: &mut NodeTree,
) -> Option<MergeResult> {
    let src = sources(adapter, base, ours, theirs);
    git_ours_result(adapter, base, &src)
        .or_else(|| text_merge_result(adapter, base, &src))
        .or_else(|| cst_file_fallback(adapter, base, ours, theirs, store))
        .or_else(|| blob_file_fallback(adapter, base, &src))
}

struct Sources {
    base: Bytes,
    ours: Bytes,
    theirs: Bytes,
}

fn sources<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
) -> Sources {
    Sources {
        base: adapter.project(&base.tree),
        ours: adapter.project(&ours.tree),
        theirs: adapter.project(&theirs.tree),
    }
}

/// True when `merged` has a non-blank line that occurs in none of base, ours,
/// and theirs. Unchanged context may come from base alone.
fn projection_invents_line(src: &Sources, merged: &[u8]) -> bool {
    let mut allowed: BTreeSet<&[u8]> = BTreeSet::new();
    for side in [
        src.base.as_slice(),
        src.ours.as_slice(),
        src.theirs.as_slice(),
    ] {
        for line in side.split(|byte| *byte == b'\n') {
            if !line.trim_ascii().is_empty() {
                allowed.insert(line);
            }
        }
    }
    merged
        .split(|byte| *byte == b'\n')
        .any(|line| !line.trim_ascii().is_empty() && !allowed.contains(line))
}

fn git_ours_result<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    src: &Sources,
) -> Option<MergeResult> {
    let merged = crate::text_merge::git_merge_ours(
        src.base.as_slice(),
        src.ours.as_slice(),
        src.theirs.as_slice(),
    )?;
    reparse(
        adapter,
        base,
        &merged,
        true,
        vec![Conflict::soft(
            Vec::new(),
            "git auto-merge with landing-order (ours) conflict hunks",
        )],
    )
}

/// Line/token 3-way of the source. Used when it re-parses losslessly, because
/// that is the combined body of overlapping edits inside one definition.
fn text_merge_result<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    src: &Sources,
) -> Option<MergeResult> {
    let merged = crate::text_merge::merge_text(
        std::str::from_utf8(src.base.as_slice()).ok()?,
        std::str::from_utf8(src.ours.as_slice()).ok()?,
        std::str::from_utf8(src.theirs.as_slice()).ok()?,
    )?;
    reparse(
        adapter,
        base,
        merged.as_bytes(),
        true,
        vec![Conflict::soft(
            Vec::new(),
            "line/token 3-way kept both sides' disjoint edits",
        )],
    )
}

/// Positional 3-way of the file CST (spec §3.4) when definition-granularity
/// compose hard-conflicts.
fn cst_file_fallback<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
    store: &mut NodeTree,
) -> Option<MergeResult> {
    let b = base.tree.root()?;
    let o = ours.tree.root()?;
    let t = theirs.tree.root()?;
    let merged = crate::cst_merge::merge_cst(store, b, o, t).ok()?;
    let mut tree = NodeTree::new();
    crate::graft::graft(&mut tree, store, merged).ok()?;
    tree.set_root(merged).ok()?;
    let bytes = adapter.project(&tree);
    reparse(
        adapter,
        base,
        bytes.as_slice(),
        false,
        vec![Conflict::soft(
            Vec::new(),
            "positional CST 3-way of the file (spec §3.4)",
        )],
    )
}

/// When structural compose hard-conflicts, a clean line merge of the whole
/// file still counts as auto-resolve (spec §5.2 rule 5). Git may have
/// conflicted with a different 3-way than `diffy`.
fn blob_file_fallback<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    src: &Sources,
) -> Option<MergeResult> {
    let merged = crate::merge_blob(
        src.base.as_slice(),
        src.ours.as_slice(),
        src.theirs.as_slice(),
    )
    .ok()?;
    reparse(adapter, base, merged.as_slice(), false, Vec::new())
}

fn reparse<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    merged: &[u8],
    lossless: bool,
    soft: Vec<Conflict>,
) -> Option<MergeResult> {
    let parsed = adapter.parse(merged).ok()?;
    if lossless && adapter.project(&parsed).as_slice() != merged {
        return None;
    }
    let mapping = adapter.identify(base, &parsed);
    Some(MergeResult {
        tree: IdentifiedTree::new(parsed, mapping.nodes),
        soft,
    })
}

/// Compose two edit scripts against `base` and apply them.
///
/// `store` must intern every Insert/Replace `to` ObjectId from both sides.
pub fn merge_ops<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &[Op],
    theirs: &[Op],
    store: &NodeTree,
) -> Result<MergeResult, Conflict> {
    let mut store = store.clone();
    let (composed, soft) = compose(ours, theirs, &mut store)?;
    if composed.is_empty() && (!ours.is_empty() || !theirs.is_empty()) {
        return Err(Conflict::hard(
            Vec::new(),
            "structural compose dropped all ops; refusing a silent base (spec §5.2)",
        ));
    }
    let applied = apply(base, &composed, &store)
        .map_err(|e| Conflict::hard(Vec::new(), format!("apply composed ops: {e}")))?;
    let bytes = adapter.project(&applied.tree);
    let parsed = adapter
        .parse(bytes.as_slice())
        .map_err(|e| Conflict::hard(Vec::new(), format!("unparseable merge (spec §5.2): {e}")))?;
    let mapping = adapter.identify(base, &parsed);
    Ok(MergeResult {
        tree: IdentifiedTree::new(parsed, mapping.nodes),
        soft,
    })
}

fn compose(
    ours: &[Op],
    theirs: &[Op],
    store: &mut NodeTree,
) -> Result<(Vec<Op>, Vec<Conflict>), Conflict> {
    if ours
        .iter()
        .chain(theirs)
        .any(|op| matches!(op, Op::Blob { .. } | Op::Tree { .. }))
    {
        return Err(Conflict::hard(
            Vec::new(),
            "blob/tree ops are merged via merge_blob, not structural merge",
        ));
    }

    let mut soft = Vec::new();
    let mut out = Vec::new();

    let ours_by = index_node_ops(ours);
    let theirs_by = index_node_ops(theirs);
    let mut nids: BTreeSet<NodeId> = BTreeSet::new();
    nids.extend(ours_by.keys().copied());
    nids.extend(theirs_by.keys().copied());
    // Content id of the base node, not the random NodeId, so child inserts
    // come out in the same order every run.
    let mut nids: Vec<NodeId> = nids.into_iter().collect();
    nids.sort_by_key(|nid| {
        let o = ours_by.get(nid).map(Vec::as_slice).unwrap_or(&[]);
        let t = theirs_by.get(nid).map(Vec::as_slice).unwrap_or(&[]);
        anchor_oid(o).max(anchor_oid(t))
    });

    for nid in nids {
        let o = ours_by.get(&nid).map(Vec::as_slice).unwrap_or(&[]);
        let t = theirs_by.get(&nid).map(Vec::as_slice).unwrap_or(&[]);
        if o.is_empty() {
            out.extend(t.iter().cloned());
        } else if t.is_empty() {
            out.extend(o.iter().cloned());
        } else {
            out.extend(resolve_same_node(nid, o, t, store)?);
        }
    }

    let (inserts, insert_soft) = compose_inserts(ours, theirs, store);
    soft.extend(insert_soft);
    out.extend(inserts);
    Ok((out, soft))
}

/// Base-content id of a Replace in `ops`, or the all-zero id when there is
/// none. Used only as a sort key.
fn anchor_oid(ops: &[Op]) -> ObjectId {
    ops.iter()
        .find_map(|op| match op {
            Op::Replace { from, .. } => Some(*from),
            _ => None,
        })
        .unwrap_or_else(|| ObjectId::from_bytes([0; 32]))
}

fn index_node_ops(ops: &[Op]) -> BTreeMap<NodeId, Vec<Op>> {
    let mut map: BTreeMap<NodeId, Vec<Op>> = BTreeMap::new();
    for op in ops {
        if let Some(nid) = primary_node(op) {
            map.entry(nid).or_default().push(op.clone());
        }
    }
    map
}

fn primary_node(op: &Op) -> Option<NodeId> {
    match op {
        Op::Delete { node }
        | Op::Replace { node, .. }
        | Op::Move { node, .. }
        | Op::Rename { node, .. } => Some(*node),
        Op::Insert { .. } | Op::Blob { .. } | Op::Tree { .. } => None,
    }
}

fn resolve_same_node(
    nid: NodeId,
    ours: &[Op],
    theirs: &[Op],
    store: &mut NodeTree,
) -> Result<Vec<Op>, Conflict> {
    let o_del = ours.iter().any(|o| matches!(o, Op::Delete { .. }));
    let t_del = theirs.iter().any(|o| matches!(o, Op::Delete { .. }));
    if o_del && t_del && ours.len() == 1 && theirs.len() == 1 {
        return Ok(vec![ours[0].clone()]);
    }
    if o_del || t_del {
        return Err(Conflict::delete_vs(
            vec![nid],
            "Delete vs any other op on the same NodeId (spec §5.2 rule 3)",
        ));
    }

    let o_rep = find_replace(ours);
    let t_rep = find_replace(theirs);
    match (o_rep, t_rep) {
        (Some(o), Some(t)) => {
            if same_normalized(store, o, t) {
                return Ok(assemble(vec![o.clone()], ours, theirs));
            }
            if let Some(merged) = merge_replace_via_cst(nid, o, t, store) {
                return Ok(assemble(vec![merged], ours, theirs));
            }
            // Landing order: keep ours, then insert named child defs that
            // exist only on theirs (fields, methods, uses).
            let mut head = vec![o.clone()];
            head.extend(child_inserts_only_in(nid, o, t, store));
            Ok(assemble(head, ours, theirs))
        }
        (Some(o), None) => {
            let mut ops = vec![o.clone()];
            ops.extend(non_replace(ours).cloned());
            ops.extend(theirs.iter().cloned());
            Ok(ops)
        }
        (None, Some(_)) => {
            let mut ops = theirs.to_vec();
            ops.extend(ours.iter().cloned());
            Ok(ops)
        }
        (None, None) => {
            // Moves and/or renames only.
            compose_move_rename(nid, ours, theirs)
        }
    }
}

fn child_inserts_only_in(parent: NodeId, ours: &Op, theirs: &Op, store: &NodeTree) -> Vec<Op> {
    let (Op::Replace { to: ours_to, .. }, Op::Replace { to: theirs_to, .. }) = (ours, theirs)
    else {
        return Vec::new();
    };
    let ours_names = named_children(store, *ours_to);
    let theirs_kids = named_children(store, *theirs_to);
    let mut extra = Vec::new();
    for (name, (oid, index)) in theirs_kids {
        if ours_names.contains_key(&name) {
            continue;
        }
        extra.push(Op::Insert {
            parent,
            index,
            node: oid,
        });
    }
    extra
}

fn named_children(store: &NodeTree, root: ObjectId) -> BTreeMap<String, (ObjectId, u32)> {
    let mut out = BTreeMap::new();
    let Some(node) = store.get(root) else {
        return out;
    };
    for (i, child) in node.children.iter().enumerate() {
        let index = u32::try_from(i).unwrap_or(u32::MAX);
        record_named(store, *child, index, &mut out);
    }
    out
}

/// A named node is recorded and not descended into. Direct children keep
/// their CST index; a name under an unnamed child is recorded at index 0.
fn record_named(
    store: &NodeTree,
    oid: ObjectId,
    index: u32,
    out: &mut BTreeMap<String, (ObjectId, u32)>,
) {
    let Some(node) = store.get(oid) else {
        return;
    };
    if let Some(name) = &node.name {
        out.entry(name.as_str().to_owned()).or_insert((oid, index));
        return;
    }
    for child in &node.children {
        record_named(store, *child, 0, out);
    }
}

fn merge_replace_via_cst(nid: NodeId, ours: &Op, theirs: &Op, store: &mut NodeTree) -> Option<Op> {
    let (
        Op::Replace {
            from, to: ours_to, ..
        },
        Op::Replace { to: theirs_to, .. },
    ) = (ours, theirs)
    else {
        return None;
    };
    let merged = crate::cst_merge::merge_cst(store, *from, *ours_to, *theirs_to).ok()?;
    Some(Op::Replace {
        node: nid,
        from: *from,
        to: merged,
    })
}

fn find_replace(ops: &[Op]) -> Option<&Op> {
    ops.iter().find(|o| matches!(o, Op::Replace { .. }))
}

fn non_replace(ops: &[Op]) -> impl Iterator<Item = &Op> {
    ops.iter().filter(|o| !matches!(o, Op::Replace { .. }))
}

fn assemble(mut ops: Vec<Op>, ours: &[Op], theirs: &[Op]) -> Vec<Op> {
    ops.extend(non_replace(ours).cloned());
    ops.extend(non_replace(theirs).cloned());
    dedup_tail(ops)
}

fn same_normalized(store: &NodeTree, ours: &Op, theirs: &Op) -> bool {
    let (Op::Replace { to: a, .. }, Op::Replace { to: b, .. }) = (ours, theirs) else {
        return false;
    };
    match (store.get(*a), store.get(*b)) {
        (Some(na), Some(nb)) => na.normalized == nb.normalized,
        _ => a == b,
    }
}

fn compose_move_rename(nid: NodeId, ours: &[Op], theirs: &[Op]) -> Result<Vec<Op>, Conflict> {
    reject_distinct(
        nid,
        ours.iter().find(|o| matches!(o, Op::Move { .. })),
        theirs.iter().find(|o| matches!(o, Op::Move { .. })),
        "Move to different destinations on the same NodeId",
    )?;
    reject_distinct(
        nid,
        ours.iter().find(|o| matches!(o, Op::Rename { .. })),
        theirs.iter().find(|o| matches!(o, Op::Rename { .. })),
        "Rename to different names on the same NodeId",
    )?;
    // Rule 4: Rename composes with Replace (handled above). Move+Rename compose.
    let mut ops = ours.to_vec();
    for t in theirs {
        if !ops.contains(t) {
            ops.push(t.clone());
        }
    }
    Ok(ops)
}

fn reject_distinct(
    nid: NodeId,
    left: Option<&Op>,
    right: Option<&Op>,
    reason: &str,
) -> Result<(), Conflict> {
    if let (Some(left), Some(right)) = (left, right)
        && left != right
    {
        return Err(Conflict::hard(vec![nid], reason));
    }
    Ok(())
}

fn dedup_tail(ops: Vec<Op>) -> Vec<Op> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for op in ops {
        if seen.insert(op.clone()) {
            out.push(op);
        }
    }
    out
}

#[derive(Clone, Copy)]
struct InsertAt {
    parent: NodeId,
    index: u32,
    node: ObjectId,
}

fn compose_inserts(ours: &[Op], theirs: &[Op], store: &NodeTree) -> (Vec<Op>, Vec<Conflict>) {
    let ours_at = insert_ats(ours);
    let theirs_at = insert_ats(theirs);
    let mut soft = Vec::new();
    let mut out: Vec<Op> = ours_at.iter().copied().map(InsertAt::into_op).collect();
    for ins in &theirs_at {
        // Same definition inserted on both sides. An outer attribute is part
        // of the definition (ADR 0011), so the object ids differ when only
        // that attribute differs. Landing order keeps ours. A different body
        // is still a second insert (spec §5.2 rule 1).
        if same_inserted_definition(store, &ours_at, ins) {
            continue;
        }
        let clash = ours_at
            .iter()
            .any(|o| o.parent == ins.parent && o.index == ins.index);
        if clash {
            soft.push(Conflict::soft(
                vec![ins.parent],
                format!(
                    "two inserts at parent {} index {}; landing order then NodeId (spec §5.2 rule 1)",
                    ins.parent, ins.index
                ),
            ));
        }
        // Identical insert (same object) at the same slot: skip duplicate.
        if ours_at
            .iter()
            .any(|o| o.parent == ins.parent && o.index == ins.index && o.node == ins.node)
        {
            continue;
        }
        let shift = ours_at
            .iter()
            .filter(|o| o.parent == ins.parent && o.index <= ins.index)
            .count();
        out.push(
            InsertAt {
                parent: ins.parent,
                index: ins.index.saturating_add(u32::try_from(shift).unwrap_or(0)),
                node: ins.node,
            }
            .into_op(),
        );
    }
    (out, soft)
}

fn same_inserted_definition(store: &NodeTree, ours: &[InsertAt], theirs: &InsertAt) -> bool {
    let Some(theirs_node) = store.get(theirs.node) else {
        return false;
    };
    let Some(name) = theirs_node
        .name
        .as_ref()
        .filter(|name| !name.as_str().is_empty())
    else {
        return false;
    };
    ours.iter().any(|ins| {
        ins.parent == theirs.parent
            && store.get(ins.node).is_some_and(|ours_node| {
                ours_node.name.as_ref().is_some_and(|ours_name| {
                    ours_name.as_str() == name.as_str()
                        && same_after_leading_attrs(
                            ours_node.raw.as_slice(),
                            theirs_node.raw.as_slice(),
                        )
                })
            })
    })
}

fn same_after_leading_attrs(ours: &[u8], theirs: &[u8]) -> bool {
    without_leading_attrs(ours) == without_leading_attrs(theirs)
}

fn without_leading_attrs(raw: &[u8]) -> &[u8] {
    let mut i = 0usize;
    loop {
        while i < raw.len() && raw[i].is_ascii_whitespace() {
            i += 1;
        }
        if raw[i..].starts_with(b"#[")
            && let Some(end) = end_of_attribute(raw, i)
        {
            i = end;
            continue;
        }
        if raw[i..].starts_with(b"///") || raw[i..].starts_with(b"//!") {
            match raw[i..].iter().position(|byte| *byte == b'\n') {
                Some(nl) => {
                    i += nl + 1;
                    continue;
                }
                None => return &raw[i..],
            }
        }
        break;
    }
    &raw[i..]
}

fn end_of_attribute(raw: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = start;
    while i < raw.len() {
        match raw[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn insert_ats(ops: &[Op]) -> Vec<InsertAt> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Insert {
                parent,
                index,
                node,
            } => Some(InsertAt {
                parent: *parent,
                index: *index,
                node: *node,
            }),
            _ => None,
        })
        .collect()
}

impl InsertAt {
    fn into_op(self) -> Op {
        Op::Insert {
            parent: self.parent,
            index: self.index,
            node: self.node,
        }
    }
}
