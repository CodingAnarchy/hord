//! Structural 3-way merge (spec §5.2).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{NodeId, Op};
use hord_lang::{IdentifiedTree, LangAdapter, NodeTree};

use crate::apply::apply;
use crate::diff::diff;
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
    let ours_ops = diff(base, &ours.tree, &ours_map);
    let theirs_ops = diff(base, &theirs.tree, &theirs_map);
    let store = union_trees(&ours.tree, &theirs.tree)
        .map_err(|e| Conflict::hard(Vec::new(), format!("union interned nodes: {e}")))?;
    merge_ops(adapter, base, &ours_ops, &theirs_ops, &store)
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
    let (composed, soft) = compose(ours, theirs, store)?;
    let applied = apply(base, &composed, store)
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
    store: &NodeTree,
) -> Result<(Vec<Op>, Vec<Conflict>), Conflict> {
    let mut soft = Vec::new();
    let mut out = Vec::new();

    let ours_by = index_node_ops(ours);
    let theirs_by = index_node_ops(theirs);
    let mut nids: BTreeSet<NodeId> = BTreeSet::new();
    nids.extend(ours_by.keys().copied());
    nids.extend(theirs_by.keys().copied());

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
    store: &NodeTree,
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
            if !same_normalized(store, o, t) {
                return Err(Conflict::hard(
                    vec![nid],
                    "Replace on the same NodeId with different normalized (spec §5.2 rule 2)",
                ));
            }
            // Same semantic edit: landing order picks ours.
            let mut ops = vec![o.clone()];
            ops.extend(non_replace(ours).cloned());
            ops.extend(non_replace(theirs).cloned());
            // Drop duplicate moves/renames that match ours.
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
