//! Structural 3-way merge (spec §5.2).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{NodeId, ObjectId, Op};
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
}

impl Conflict {
    fn hard(nodes: Vec<NodeId>, reason: impl Into<String>) -> Self {
        Self {
            nodes,
            kind: ConflictKind::Hard,
            reason: reason.into(),
        }
    }

    fn soft(nodes: Vec<NodeId>, reason: impl Into<String>) -> Self {
        Self {
            nodes,
            kind: ConflictKind::Soft,
            reason: reason.into(),
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
        Err(conflict) if conflict.reason.contains("Delete vs") => Err(conflict),
        Ok(ok) => {
            let projected = adapter.project(&ok.tree.tree);
            // A line in none of the three inputs was invented by the merge
            // (a comma glued onto the wrong token, a hand-style rewrite).
            // That is not an auto-resolution. Landing order is git's.
            if projection_invents_line(adapter, base, ours, theirs, projected.as_slice())
                && let Some(git) = git_ours_result(adapter, base, ours, theirs)
            {
                return Ok(git);
            }
            Ok(ok)
        }
        Err(structural) => {
            // Overlapping edits. Keep non-conflicting edits from both sides
            // and ours' side of each conflict hunk. Do not invent a rewrite.
            if let Some(ok) = git_ours_result(adapter, base, ours, theirs) {
                return Ok(ok);
            }
            if let Some(ok) = text_merge_result(adapter, base, ours, theirs) {
                return Ok(ok);
            }
            if let Some(ok) = cst_file_fallback(adapter, base, ours, theirs, &mut store) {
                return Ok(ok);
            }
            match blob_file_fallback(adapter, base, ours, theirs) {
                Some(ok) => Ok(ok),
                None => Err(structural),
            }
        }
    }
}

/// True when `merged` has a non-blank line that occurs in none of base, ours,
/// and theirs. Unchanged context may come from base alone.
fn projection_invents_line<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
    merged: &[u8],
) -> bool {
    let base_src = adapter.project(&base.tree);
    let ours_src = adapter.project(&ours.tree);
    let theirs_src = adapter.project(&theirs.tree);
    let mut allowed: BTreeSet<&[u8]> = BTreeSet::new();
    for src in [
        base_src.as_slice(),
        ours_src.as_slice(),
        theirs_src.as_slice(),
    ] {
        for line in src.split(|byte| *byte == b'\n') {
            if !trim_ascii(line).is_empty() {
                allowed.insert(line);
            }
        }
    }
    merged
        .split(|byte| *byte == b'\n')
        .any(|line| !trim_ascii(line).is_empty() && !allowed.contains(line))
}

fn trim_ascii(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &line[start..end]
}

fn git_ours_result<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
) -> Option<MergeResult> {
    let base_src = adapter.project(&base.tree);
    let ours_src = adapter.project(&ours.tree);
    let theirs_src = adapter.project(&theirs.tree);
    let merged = crate::text_merge::git_merge_ours(
        base_src.as_slice(),
        ours_src.as_slice(),
        theirs_src.as_slice(),
    )?;
    let parsed = adapter.parse(&merged).ok()?;
    if adapter.project(&parsed).as_slice() != merged.as_slice() {
        return None;
    }
    let mapping = adapter.identify(base, &parsed);
    Some(MergeResult {
        tree: IdentifiedTree::new(parsed, mapping.nodes),
        soft: vec![Conflict::soft(
            Vec::new(),
            "git auto-merge with landing-order (ours) conflict hunks",
        )],
    })
}

/// Line/token 3-way of the source. Used when it re-parses losslessly, because
/// that is the combined body of overlapping edits inside one definition.
fn text_merge_result<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
) -> Option<MergeResult> {
    let base_src = adapter.project(&base.tree);
    let ours_src = adapter.project(&ours.tree);
    let theirs_src = adapter.project(&theirs.tree);
    let merged = crate::text_merge::merge_text(
        std::str::from_utf8(base_src.as_slice()).ok()?,
        std::str::from_utf8(ours_src.as_slice()).ok()?,
        std::str::from_utf8(theirs_src.as_slice()).ok()?,
    )?;
    let parsed = adapter.parse(merged.as_bytes()).ok()?;
    if adapter.project(&parsed).as_slice() != merged.as_bytes() {
        return None;
    }
    let mapping = adapter.identify(base, &parsed);
    Some(MergeResult {
        tree: IdentifiedTree::new(parsed, mapping.nodes),
        soft: vec![Conflict::soft(
            Vec::new(),
            "line/token 3-way kept both sides' disjoint edits",
        )],
    })
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
    let parsed = adapter.parse(bytes.as_slice()).ok()?;
    let mapping = adapter.identify(base, &parsed);
    Some(MergeResult {
        tree: IdentifiedTree::new(parsed, mapping.nodes),
        soft: vec![Conflict::soft(
            Vec::new(),
            "positional CST 3-way of the file (spec §3.4)",
        )],
    })
}

/// When structural compose hard-conflicts, a clean line merge of the whole
/// file still counts as auto-resolve (spec §5.2 rule 5). Git may have
/// conflicted with a different 3-way than `diffy`.
fn blob_file_fallback<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
) -> Option<MergeResult> {
    let b = adapter.project(&base.tree);
    let o = adapter.project(&ours.tree);
    let t = adapter.project(&theirs.tree);
    let merged = crate::merge_blob(b.as_slice(), o.as_slice(), t.as_slice()).ok()?;
    let parsed = adapter.parse(merged.as_slice()).ok()?;
    let mapping = adapter.identify(base, &parsed);
    Some(MergeResult {
        tree: IdentifiedTree::new(parsed, mapping.nodes),
        soft: Vec::new(),
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
        match (o.is_empty(), t.is_empty()) {
            (false, true) => out.extend(o.iter().cloned()),
            (true, false) => out.extend(t.iter().cloned()),
            (false, false) => out.extend(resolve_same_node(nid, o, t, store)?),
            (true, true) => {}
        }
    }

    let (inserts, insert_soft) = compose_inserts(ours, theirs);
    soft.extend(insert_soft);
    out.extend(inserts);

    for op in ours {
        if matches!(op, Op::Blob { .. } | Op::Tree { .. }) {
            return Err(Conflict::hard(
                Vec::new(),
                "blob/tree ops are merged via merge_blob, not structural merge",
            ));
        }
    }
    for op in theirs {
        if matches!(op, Op::Blob { .. } | Op::Tree { .. }) {
            return Err(Conflict::hard(
                Vec::new(),
                "blob/tree ops are merged via merge_blob, not structural merge",
            ));
        }
    }

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
        return Err(Conflict::hard(
            vec![nid],
            "Delete vs any other op on the same NodeId (spec §5.2 rule 3)",
        ));
    }

    let o_rep = find_replace(ours);
    let t_rep = find_replace(theirs);
    match (o_rep, t_rep) {
        (Some(o), Some(t)) => {
            if same_normalized(store, o, t) {
                let mut ops = vec![o.clone()];
                ops.extend(non_replace(ours).cloned());
                ops.extend(non_replace(theirs).cloned());
                return Ok(dedup_tail(ops));
            }
            if let Some(merged) = merge_replace_via_cst(nid, o, t, store) {
                let mut ops = vec![merged];
                ops.extend(non_replace(ours).cloned());
                ops.extend(non_replace(theirs).cloned());
                return Ok(dedup_tail(ops));
            }
            // Landing order: keep ours, then insert named child defs that
            // exist only on theirs (fields, methods, uses).
            let mut ops = vec![o.clone()];
            ops.extend(child_inserts_only_in(nid, o, t, store));
            ops.extend(non_replace(ours).cloned());
            ops.extend(non_replace(theirs).cloned());
            Ok(dedup_tail(ops))
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
    collect_named(store, root, root, &mut out);
    out
}

fn collect_named(
    store: &NodeTree,
    oid: ObjectId,
    root: ObjectId,
    out: &mut BTreeMap<String, (ObjectId, u32)>,
) {
    let Some(node) = store.get(oid) else {
        return;
    };
    if oid != root
        && let Some(name) = &node.name
    {
        out.entry(name.as_str().to_owned()).or_insert((oid, 0));
        return;
    }
    for (i, child) in node.children.iter().enumerate() {
        if oid == root
            && let Some(n) = store.get(*child)
            && let Some(name) = &n.name
        {
            let idx = u32::try_from(i).unwrap_or(u32::MAX);
            out.entry(name.as_str().to_owned()).or_insert((*child, idx));
            continue;
        }
        collect_named(store, *child, root, out);
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
    let o_mv = ours.iter().find(|o| matches!(o, Op::Move { .. }));
    let t_mv = theirs.iter().find(|o| matches!(o, Op::Move { .. }));
    match (o_mv, t_mv) {
        (Some(o), Some(t)) if o != t => {
            return Err(Conflict::hard(
                vec![nid],
                "Move to different destinations on the same NodeId",
            ));
        }
        _ => {}
    }
    let o_rn = ours.iter().find(|o| matches!(o, Op::Rename { .. }));
    let t_rn = theirs.iter().find(|o| matches!(o, Op::Rename { .. }));
    match (o_rn, t_rn) {
        (Some(o), Some(t)) if o != t => {
            return Err(Conflict::hard(
                vec![nid],
                "Rename to different names on the same NodeId",
            ));
        }
        _ => {}
    }
    // Rule 4: Rename composes with Replace (handled above). Move+Rename compose.
    let mut ops = ours.to_vec();
    for t in theirs {
        if !ops.contains(t) {
            ops.push(t.clone());
        }
    }
    Ok(ops)
}

fn dedup_tail(ops: Vec<Op>) -> Vec<Op> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for op in ops {
        let key = format!("{op:?}");
        if seen.insert(key) {
            out.push(op);
        }
    }
    out
}

fn compose_inserts(ours: &[Op], theirs: &[Op]) -> (Vec<Op>, Vec<Conflict>) {
    let o_ins: Vec<&Op> = ours
        .iter()
        .filter(|o| matches!(o, Op::Insert { .. }))
        .collect();
    let t_ins: Vec<&Op> = theirs
        .iter()
        .filter(|o| matches!(o, Op::Insert { .. }))
        .collect();
    let mut soft = Vec::new();
    let mut out = Vec::new();
    for o in &o_ins {
        out.push((*o).clone());
    }
    for t in &t_ins {
        let Op::Insert {
            parent,
            index,
            node,
        } = t
        else {
            continue;
        };
        let clash = o_ins.iter().any(|o| {
            matches!(
                o,
                Op::Insert {
                    parent: p,
                    index: i,
                    ..
                } if p == parent && i == index
            )
        });
        if clash {
            soft.push(Conflict::soft(
                vec![*parent],
                format!(
                    "two inserts at parent {parent} index {index}; landing order then NodeId (spec §5.2 rule 1)"
                ),
            ));
        }
        let shift = o_ins
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    Op::Insert {
                        parent: p,
                        index: i,
                        ..
                    } if p == parent && *i <= *index
                )
            })
            .count();
        let new_index = index.saturating_add(u32::try_from(shift).unwrap_or(0));
        // Identical insert (same object) at the same slot: skip duplicate.
        let dup = o_ins.iter().any(|o| {
            matches!(
                o,
                Op::Insert {
                    parent: p,
                    index: i,
                    node: n,
                } if p == parent && i == index && n == node
            )
        });
        if dup {
            continue;
        }
        out.push(Op::Insert {
            parent: *parent,
            index: new_index,
            node: *node,
        });
    }
    (out, soft)
}
