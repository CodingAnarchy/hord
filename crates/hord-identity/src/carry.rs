//! Identity carrying (spec §3.4 steps 1–3, 5, and 6).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, Op, RepoPath, SnapshotId};
use hord_lang::{
    IdentifiedTree, IdentityMapping, LangAdapter, NodeTree, Site, def_sites, default_identify,
    oid_at,
};

use crate::Result;
use crate::error::Error;

/// Explicit identity relation (spec §3.4 step 5).
///
/// Result definitions are named by content [`ObjectId`] because a heuristic
/// birth has no stable [`NodeId`] yet. Applied declarations are recorded as
/// [`IdentityDelta`] values on the mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Declaration {
    /// Result definition `result` continues or copies base identity `from`.
    DerivedFrom {
        /// Result definition content id.
        result: ObjectId,
        /// Base identity it continues or copies.
        from: NodeId,
    },
    /// Base identity `node` was split into the result definitions `into`.
    SplitInto {
        /// Base identity that was split.
        node: NodeId,
        /// Result definition content ids, in declaration order.
        into: Vec<ObjectId>,
    },
    /// Result definition `result` was merged from the base identities `from`.
    MergedFrom {
        /// Result definition content id.
        result: ObjectId,
        /// Base identities that were merged, in declaration order.
        from: Vec<NodeId>,
    },
}

/// Carry [`NodeId`]s from `base` onto `result`, the file at `path`, in a
/// change whose base snapshot is `snapshot` (spec §3.4).
///
/// Steps 1–3 and unmatched births and deaths come from [`default_identify`],
/// including rename similarity (ADR 0007). Birth ids are then replaced with
/// ids derived per ADR 0019 from the definition's content, `path`'s file
/// root, its site, and `snapshot` (see [`crate::assign`]), so a definition
/// deleted and later re-added is born on a later base and gets a new id.
/// `declarations` (step 5) then override those heuristic results. Anything
/// still unmatched stays a birth or a death (step 6).
///
/// [`Declaration::DerivedFrom`] names a result definition by content id:
///
/// - If `from` is not already the identity of another result definition, the
///   result **keeps** `from`. A heuristic birth of that definition and a death
///   of `from` are removed. The mapping records [`IdentityDelta::DerivedFrom`]
///   with both ids equal to `from`: the declaration, not a heuristic, carried
///   the identity.
/// - If `from` is already carried by a different result definition, the
///   declaration is a copy. The result keeps a distinct [`NodeId`] and the
///   birth is replaced by [`IdentityDelta::DerivedFrom`] pointing at `from`.
///
/// [`Declaration::SplitInto`] replaces the source's death and each piece's
/// birth with one [`IdentityDelta::SplitInto`]. Pieces that heuristics already
/// matched keep those ids.
///
/// [`Declaration::MergedFrom`] continues the first listed source that is not
/// already carried elsewhere, or keeps the result's id when it is already one
/// of those sources. Consumed sources are not also recorded as deaths.
///
/// Declarations run in slice order. Two declarations must not name the same
/// result definition. A second declaration must not adopt a [`NodeId`] the
/// first one already continued.
///
/// `from` does not have to occur in this `base`. Callers use that for an
/// identity that lives in another file of the snapshot.
///
/// # Errors
///
/// [`Error::UnknownResult`] if a declaration names a content id that is not a
/// definition in `result`. [`Error::Conflict`] if two declarations name the
/// same result definition. [`Error::Claimed`] if two declarations adopt the
/// same free id, or a split source is still live on a definition that is not
/// one of the pieces. [`Error::EmptySplit`] and [`Error::EmptyMerge`] when the
/// corresponding list is empty. [`Error::DuplicateNode`] if the result would
/// give one [`NodeId`] to two definitions.
pub fn carry<A: LangAdapter + ?Sized>(
    adapter: &A,
    path: &RepoPath,
    snapshot: Option<SnapshotId>,
    base: &IdentifiedTree,
    result: &NodeTree,
    declarations: &[Declaration],
) -> Result<IdentityMapping> {
    let mut mapping = default_identify(adapter, base, result);
    // Births from `default_identify` are random ULIDs. Replace them before
    // declarations so a copy of a birth, and every other new id, stays a
    // function of the change's inputs.
    crate::assign::stabilize_births(
        &mut mapping,
        result,
        &crate::assign::BirthScope { path, snapshot },
    );
    if !declarations.is_empty() {
        check_unique_targets(declarations)?;
        apply_declarations(&mut mapping, result, declarations)?;
        ensure_unique(&mapping.nodes)?;
    }
    // Moves name the ids after declarations. With no declarations this
    // matches the moves [`default_identify`] already recorded.
    recompute_moves(&mut mapping, base, result);
    Ok(mapping)
}

fn check_unique_targets(declarations: &[Declaration]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for decl in declarations {
        for id in result_targets(decl) {
            if !seen.insert(*id) {
                return Err(Error::Conflict(*id));
            }
        }
    }
    Ok(())
}

fn result_targets(decl: &Declaration) -> &[ObjectId] {
    match decl {
        Declaration::DerivedFrom { result, .. } | Declaration::MergedFrom { result, .. } => {
            std::slice::from_ref(result)
        }
        Declaration::SplitInto { into, .. } => into,
    }
}

/// The site of the definition a declaration names by content id. Two
/// identical definitions share a content id; the first in preorder is named.
fn site_for(mapping: &IdentityMapping, tree: &NodeTree, oid: ObjectId) -> Result<Site> {
    mapping
        .nodes
        .keys()
        .find(|site| oid_at(tree, site) == Some(oid))
        .cloned()
        .ok_or(Error::UnknownResult(oid))
}

fn apply_declarations(
    mapping: &mut IdentityMapping,
    tree: &NodeTree,
    declarations: &[Declaration],
) -> Result<()> {
    let heuristic_held: BTreeSet<NodeId> = mapping.nodes.values().copied().collect();
    let mut claimed = BTreeSet::new();
    for decl in declarations {
        match decl {
            Declaration::DerivedFrom { result, from } => {
                let site = site_for(mapping, tree, *result)?;
                apply_derived_from(
                    mapping,
                    &heuristic_held,
                    &mut claimed,
                    (&site, *result),
                    *from,
                )?;
            }
            Declaration::SplitInto { node, into } => {
                let sites = into
                    .iter()
                    .map(|oid| site_for(mapping, tree, *oid))
                    .collect::<Result<Vec<_>>>()?;
                apply_split(mapping, *node, &sites)?;
            }
            Declaration::MergedFrom { result, from } => {
                let site = site_for(mapping, tree, *result)?;
                apply_merge(mapping, &mut claimed, (&site, *result), from)?;
            }
        }
    }
    Ok(())
}

fn apply_derived_from(
    mapping: &mut IdentityMapping,
    heuristic_held: &BTreeSet<NodeId>,
    claimed: &mut BTreeSet<NodeId>,
    (site, oid): (&Site, ObjectId),
    from: NodeId,
) -> Result<()> {
    let Some(&current) = mapping.nodes.get(site) else {
        return Err(Error::UnknownResult(oid));
    };
    if current == from {
        return Ok(());
    }
    if held_elsewhere(&mapping.nodes, site, from) {
        // Held by a declaration in this pass, not by the heuristic match.
        if !heuristic_held.contains(&from) {
            return Err(Error::Claimed(from));
        }
        apply_copy(mapping, (site, oid), current, from);
        return Ok(());
    }
    if claimed.contains(&from) {
        return Err(Error::Claimed(from));
    }
    detach(mapping, site, current);
    remove_death(&mut mapping.deltas, from);
    mapping.nodes.insert(site.clone(), from);
    claimed.insert(from);
    // `node == from`: the declaration continued the base identity, so the
    // result id is that id. A copy uses a distinct result id.
    mapping
        .deltas
        .push(IdentityDelta::DerivedFrom { node: from, from });
    Ok(())
}

/// A copy id derived from the result content id and the source id.
///
/// The value does not use [`NodeId::generate`], so it does not change when an
/// unrelated definition is inserted or when the process is run again.
fn unused_copy_id(mapping: &IdentityMapping, result: ObjectId, from: NodeId) -> NodeId {
    for salt in 0..64u32 {
        let id = stable_copy_id(result, from, salt);
        if id != NodeId::nil() && !id_in_use(mapping, id) {
            return id;
        }
    }
    stable_copy_id(result, from, 64)
}

fn stable_copy_id(oid: ObjectId, from: NodeId, salt: u32) -> NodeId {
    let bytes = oid.as_bytes();
    let hi = u128::from_be_bytes(bytes[..16].try_into().expect("16 bytes"));
    let lo = u128::from_be_bytes(bytes[16..].try_into().expect("16 bytes"));
    let mut mixed = hi
        ^ lo.rotate_left(29)
        ^ from.as_u128().rotate_left(5)
        ^ u128::from(salt).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mixed ^= 0xC000_0000_0000_0000;
    if mixed == 0 || mixed == from.as_u128() {
        mixed ^= 0x9E37_79B9_7F4A_7C15;
    }
    NodeId::from_u128(mixed)
}

fn id_in_use(mapping: &IdentityMapping, id: NodeId) -> bool {
    mapping.nodes.values().any(|node| *node == id)
        || mapping
            .deltas
            .iter()
            .flat_map(IdentityDelta::node_ids)
            .any(|node| node == id)
}

fn apply_copy(
    mapping: &mut IdentityMapping,
    (site, oid): (&Site, ObjectId),
    current: NodeId,
    from: NodeId,
) {
    let was_birth = remove_birth(&mut mapping.deltas, current);
    let copy_id = if was_birth {
        current
    } else {
        release_unmatched(mapping, site, current);
        unused_copy_id(mapping, oid, from)
    };
    mapping.nodes.insert(site.clone(), copy_id);
    mapping.deltas.push(IdentityDelta::DerivedFrom {
        node: copy_id,
        from,
    });
}

fn apply_split(mapping: &mut IdentityMapping, node: NodeId, into: &[Site]) -> Result<()> {
    if into.is_empty() {
        return Err(Error::EmptySplit(node));
    }
    let mut into_ids = Vec::with_capacity(into.len());
    for site in into {
        let Some(&current) = mapping.nodes.get(site) else {
            return Err(Error::EmptySplit(node));
        };
        into_ids.push(current);
    }
    if let Some(holder) = holder(&mapping.nodes, node)
        && !into.contains(&holder)
    {
        return Err(Error::Claimed(node));
    }
    for id in &into_ids {
        remove_birth(&mut mapping.deltas, *id);
    }
    remove_death(&mut mapping.deltas, node);
    mapping.deltas.push(IdentityDelta::SplitInto {
        node,
        into: into_ids,
    });
    Ok(())
}

fn apply_merge(
    mapping: &mut IdentityMapping,
    claimed: &mut BTreeSet<NodeId>,
    (site, oid): (&Site, ObjectId),
    from: &[NodeId],
) -> Result<()> {
    if from.is_empty() {
        return Err(Error::EmptyMerge(oid));
    }
    let Some(&current) = mapping.nodes.get(site) else {
        return Err(Error::UnknownResult(oid));
    };
    let chosen = choose_merge_id(mapping, claimed, site, current, from)?;
    if chosen == current {
        remove_birth(&mut mapping.deltas, current);
    } else {
        detach(mapping, site, current);
        mapping.nodes.insert(site.clone(), chosen);
        claimed.insert(chosen);
    }
    for src in from {
        remove_death(&mut mapping.deltas, *src);
    }
    mapping.deltas.push(IdentityDelta::MergedFrom {
        node: chosen,
        from: from.to_vec(),
    });
    Ok(())
}

fn choose_merge_id(
    mapping: &IdentityMapping,
    claimed: &BTreeSet<NodeId>,
    result: &Site,
    current: NodeId,
    from: &[NodeId],
) -> Result<NodeId> {
    if from.contains(&current) {
        return Ok(current);
    }
    for id in from {
        if held_elsewhere(&mapping.nodes, result, *id) {
            continue;
        }
        if claimed.contains(id) {
            return Err(Error::Claimed(*id));
        }
        return Ok(*id);
    }
    Ok(current)
}

fn detach(mapping: &mut IdentityMapping, site: &Site, id: NodeId) {
    let was_birth = remove_birth(&mut mapping.deltas, id);
    if !was_birth {
        release_unmatched(mapping, site, id);
    }
}

fn release_unmatched(mapping: &mut IdentityMapping, site: &Site, id: NodeId) {
    if held_elsewhere(&mapping.nodes, site, id) {
        return;
    }
    if mapping
        .deltas
        .iter()
        .any(|d| matches!(d, IdentityDelta::Death { node } if *node == id))
    {
        return;
    }
    mapping.deltas.push(IdentityDelta::Death { node: id });
}

fn held_elsewhere(nodes: &BTreeMap<Site, NodeId>, except: &Site, id: NodeId) -> bool {
    nodes.iter().any(|(site, nid)| site != except && *nid == id)
}

fn holder(nodes: &BTreeMap<Site, NodeId>, id: NodeId) -> Option<Site> {
    nodes
        .iter()
        .find_map(|(site, nid)| (*nid == id).then(|| site.clone()))
}

fn ensure_unique(nodes: &BTreeMap<Site, NodeId>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for id in nodes.values() {
        if !seen.insert(*id) {
            return Err(Error::DuplicateNode(*id));
        }
    }
    Ok(())
}

fn remove_birth(deltas: &mut Vec<IdentityDelta>, id: NodeId) -> bool {
    remove_delta(
        deltas,
        |d| matches!(d, IdentityDelta::Birth { node } if *node == id),
    )
}

fn remove_death(deltas: &mut Vec<IdentityDelta>, id: NodeId) {
    remove_delta(
        deltas,
        |d| matches!(d, IdentityDelta::Death { node } if *node == id),
    );
}

fn remove_delta(deltas: &mut Vec<IdentityDelta>, pred: impl Fn(&IdentityDelta) -> bool) -> bool {
    let Some(index) = deltas.iter().position(pred) else {
        return false;
    };
    deltas.remove(index);
    true
}

#[derive(Clone, Copy, Debug)]
struct Placement {
    parent: Option<NodeId>,
    index: u32,
}

/// Step 3 moves after declarations: both enclosing definitions have ids, and
/// the parent id changed. `index` is the position in the immediate CST parent.
fn recompute_moves(mapping: &mut IdentityMapping, base: &IdentifiedTree, result: &NodeTree) {
    let base_sites = site_index(&base.tree, &base.ids);
    let mut moves = Vec::new();
    let mut emitted = BTreeSet::new();
    for (node_id, site) in sites(result, &mapping.nodes) {
        if !emitted.insert(node_id) {
            continue;
        }
        let Some(base_site) = base_sites.get(&node_id) else {
            continue;
        };
        let (Some(from_parent), Some(to_parent)) = (base_site.parent, site.parent) else {
            continue;
        };
        if from_parent == to_parent {
            continue;
        }
        moves.push(Op::Move {
            node: node_id,
            from_parent,
            to_parent,
            index: site.index,
        });
    }
    mapping.moves = moves;
}

fn site_index(tree: &NodeTree, ids: &BTreeMap<Site, NodeId>) -> BTreeMap<NodeId, Placement> {
    let mut map = BTreeMap::new();
    for (id, placement) in sites(tree, ids) {
        map.entry(id).or_insert(placement);
    }
    map
}

fn sites<'a>(
    tree: &'a NodeTree,
    ids: &'a BTreeMap<Site, NodeId>,
) -> impl Iterator<Item = (NodeId, Placement)> + 'a {
    def_sites(tree, ids).map(|def| {
        let index = def.site.last().copied().unwrap_or(0);
        (
            def.node,
            Placement {
                parent: def.parent,
                index,
            },
        )
    })
}
