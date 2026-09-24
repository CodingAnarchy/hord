//! Apply a definition-granularity edit script to an identified tree.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use hord_core::{NodeId, ObjectId, Op, RepoPath};
use hord_lang::{IdentifiedTree, NodeTree, Site};

use crate::Error;
use crate::defs::{chain, oid_of, site_of};
use crate::graft::graft;

/// Apply `ops` to `base` (the file at `path`), producing a new identified
/// tree.
///
/// Returns an [`IdentifiedTree`]: interned CST plus the definition
/// [`NodeId`] map after the edit. [`Op::Insert`] / [`Op::Replace`] `to`
/// nodes must already be interned in `store` (the `result` tree passed to
/// [`crate::diff`], or a union of trees for a merge).
///
/// The file root is [`NodeId::file_root`]`(path)` (ADR 0015). A Replace of
/// that id swaps the file's glue or, on an empty base, creates the file. An
/// op that names any other id not in `base` (such as [`NodeId::nil`]) fails
/// with [`Error::MissingId`].
///
/// Ops are classified and applied in a fixed order (replaces, deletes,
/// moves, inserts). Insert indices are result-side CST positions and are
/// interpreted after deletes so they line up with the result child list.
pub fn apply(
    path: &RepoPath,
    base: &IdentifiedTree,
    ops: &[Op],
    store: &NodeTree,
) -> Result<IdentifiedTree, Error> {
    let root = NodeId::file_root(path);
    check_replace_sources(base, ops, root)?;
    apply_internal(base, ops, store, root)
}

/// [`apply`] with a `store` whose definitions have ids: typically the result
/// of the change the ops were diffed from. Content the ops bring in takes
/// its ids from `store` instead of fresh ones:
///
/// - a replaced definition's subtree takes the ids under that definition in
///   `store`;
/// - the sites of each inserted content id are paired, in preorder, with the
///   sites of that content in `store` whose ids `base` does not have.
///
/// Every id in the result is from `base` or `store`. A definition whose id
/// cannot be placed that way (a replaced definition missing from `store`,
/// or inserted content that occurs a different number of times on the two
/// sides) is left without one, so a caller can tell and carry identity
/// instead.
pub fn apply_identified(
    path: &RepoPath,
    base: &IdentifiedTree,
    ops: &[Op],
    store: &IdentifiedTree,
) -> Result<IdentifiedTree, Error> {
    let root = NodeId::file_root(path);
    let mut applied = apply(path, base, ops, &store.tree)?;
    let mut ids = applied.ids.clone();
    let mut inserted = BTreeSet::new();
    for op in ops {
        match op {
            Op::Replace { node, .. } => {
                let Some(at) = site_of(&applied, *node, root) else {
                    continue;
                };
                match site_of(store, *node, root) {
                    Some(from) => copy_subtree(&mut ids, store, &from, &at),
                    None => ids.retain(|site, _| !site.starts_with(&at)),
                }
            }
            Op::Insert { node, .. } => {
                inserted.insert(*node);
            }
            _ => {}
        }
    }
    let base_ids: HashSet<NodeId> = base.ids.values().copied().collect();
    let known: HashSet<NodeId> = base_ids.iter().chain(store.ids.values()).copied().collect();
    for oid in inserted {
        // Sites `apply` gave a fresh id, and new sites of the same content in
        // `store`.
        let targets: Vec<&Site> = applied
            .ids
            .iter()
            .filter(|(site, id)| !known.contains(id) && applied.oid_at(site) == Some(oid))
            .map(|(site, _)| site)
            .collect();
        let sources: Vec<&Site> = store
            .ids
            .iter()
            .filter(|(site, id)| !base_ids.contains(id) && store.oid_at(site) == Some(oid))
            .map(|(site, _)| site)
            .collect();
        if targets.len() == sources.len() {
            for (at, from) in targets.into_iter().zip(sources) {
                copy_subtree(&mut ids, store, from, at);
            }
        }
    }
    ids.retain(|site, id| known.contains(id) && applied.oid_at(site).is_some());
    applied.ids = ids;
    Ok(applied)
}

/// Replace the ids at and below `at` with `store`'s ids at and below
/// `from`, at the same relative sites.
fn copy_subtree(
    ids: &mut BTreeMap<Site, NodeId>,
    store: &IdentifiedTree,
    from: &[u32],
    at: &[u32],
) {
    ids.retain(|site, _| !site.starts_with(at));
    for (site, id) in &store.ids {
        if site.starts_with(from) {
            let mut key = at.to_vec();
            key.extend_from_slice(&site[from.len()..]);
            ids.insert(key, *id);
        }
    }
}

/// Every [`Op::Replace`] must find its `from` at its node in `base`, so a
/// script never overwrites content it was not diffed from (for example an
/// edit landed since, in a rebase). An empty base has nothing to check.
fn check_replace_sources(base: &IdentifiedTree, ops: &[Op], root: NodeId) -> Result<(), Error> {
    if base.tree.root().is_none() {
        return Ok(());
    }
    for op in ops {
        if let Op::Replace { node, from, .. } = op
            && let Some(found) = oid_of(base, *node, root)
            && found != *from
        {
            return Err(Error::StaleReplace {
                node: *node,
                expected: *from,
                found,
            });
        }
    }
    Ok(())
}

/// [`apply`] without the stale-Replace check, for a file whose root id is
/// `root`.
pub(crate) fn apply_internal(
    base: &IdentifiedTree,
    ops: &[Op],
    store: &NodeTree,
    root: NodeId,
) -> Result<IdentifiedTree, Error> {
    let mut working = base.clone();
    if working.tree.root().is_none() {
        if let Some(Op::Replace { to, .. }) = ops
            .iter()
            .find(|o| matches!(o, Op::Replace { node, .. } if *node == root))
        {
            graft(&mut working.tree, store, *to)?;
            working.tree.set_root(*to)?;
            working.ids.clear();
            return Ok(working);
        }
        if ops.is_empty() {
            return Ok(working);
        }
        return Err(Error::apply(
            "apply on an empty base requires a root Replace",
        ));
    }

    let mut replaces = Vec::new();
    let mut inserts = Vec::new();
    let mut moves = Vec::new();
    let mut deletes = Vec::new();

    for op in ops {
        match op {
            Op::Replace { node, to, .. } => replaces.push((*node, *to)),
            Op::Insert {
                parent,
                index,
                node,
            } => inserts.push((*parent, *index, *node)),
            Op::Move {
                node,
                to_parent,
                index,
                ..
            } => moves.push((*node, *to_parent, *index)),
            Op::Delete { node } => deletes.push(*node),
            // A name lives in source tokens, so a paired Replace carries the
            // new body. Rename is recorded for merge rule 4.
            Op::Rename { .. } | Op::Blob { .. } | Op::Tree { .. } => {}
        }
    }

    // Ancestors first, then content id. `NodeId` is a random ULID and must
    // not decide which edit lands.
    replaces.sort_by_key(|&(node, _)| content_order(base, node, root));

    for (node, to) in replaces {
        graft(&mut working.tree, store, to)?;
        apply_replace(&mut working, node, to, root)?;
    }

    deletes.sort_by_key(|node| {
        let (depth, oid) = content_order(&working, *node, root);
        (Reverse(depth), oid)
    });

    for node in deletes {
        apply_delete(&mut working, node, root)?;
    }

    moves.sort_by_key(|&(node, to_parent, index)| {
        let (_, oid) = content_order(&working, node, root);
        let (_, parent) = content_order(&working, to_parent, root);
        (index, parent, oid)
    });

    for (node, to_parent, index) in moves {
        apply_move(&mut working, node, to_parent, index, root)?;
    }

    // Inserts last so `index` is the result-side CST index after deletes.
    for (parent, index, node) in inserts {
        graft(&mut working.tree, store, node)?;
        apply_insert(&mut working, parent, index, node, root)?;
    }

    let seps = trailing_commas(store);
    if !seps.is_empty() {
        let mut sep_ids: Vec<ObjectId> = seps.values().copied().collect();
        sep_ids.sort();
        sep_ids.dedup();
        for id in &sep_ids {
            graft(&mut working.tree, store, *id)?;
        }
        if let Some(root) = working.tree.root() {
            let new_root = restore_trailing_commas(&mut working, root, &mut Vec::new(), &seps)?;
            working.tree.set_root(new_root)?;
        }
    }

    Ok(working)
}

/// Child definition → the `,` sibling that followed it in a source tree.
///
/// Only fields and enum variants. A shared token such as `i32` must not pick
/// up a comma just because one occurrence was followed by one.
fn trailing_commas(store: &NodeTree) -> BTreeMap<ObjectId, ObjectId> {
    let mut map = BTreeMap::new();
    for (_, parent) in store.iter() {
        for (index, child) in parent.children.iter().enumerate() {
            let Some(node) = store.get(*child) else {
                continue;
            };
            if !matches!(node.kind.as_str(), "field_declaration" | "enum_variant") {
                continue;
            }
            let Some(next) = parent.children.get(index + 1) else {
                continue;
            };
            let Some(sep) = store.get(*next) else {
                continue;
            };
            if sep.children.is_empty() && sep.raw.as_slice() == b"," {
                map.insert(*child, *next);
            }
        }
    }
    map
}

/// Put back the `,` that followed an inserted field or variant in its source.
/// Sites of later siblings shift with each comma added.
fn restore_trailing_commas(
    working: &mut IdentifiedTree,
    id: ObjectId,
    site: &mut Site,
    seps: &BTreeMap<ObjectId, ObjectId>,
) -> Result<ObjectId, Error> {
    let node = working.tree.get(id).ok_or(Error::MissingNode(id))?.clone();
    let mut rebuilt = Vec::with_capacity(node.children.len());
    let mut changed = false;
    for (i, child) in node.children.iter().enumerate() {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        let new_child = restore_trailing_commas(working, *child, site, seps)?;
        site.pop();
        changed |= new_child != *child;
        rebuilt.push(new_child);
    }
    let mut kids = Vec::with_capacity(rebuilt.len());
    let mut moved_to = Vec::with_capacity(rebuilt.len());
    for (index, child) in rebuilt.iter().enumerate() {
        moved_to.push(u32::try_from(kids.len()).unwrap_or(u32::MAX));
        kids.push(*child);
        let Some(sep) = seps.get(child) else {
            continue;
        };
        if rebuilt.get(index + 1).is_some_and(|next| next == sep) {
            continue;
        }
        kids.push(*sep);
        changed = true;
    }
    if !changed {
        return Ok(id);
    }
    remap_children(&mut working.ids, site, |k| {
        moved_to.get(k as usize).copied()
    });
    intern_children(working, id, kids)
}

/// Tree depth, then the node's content id. Both come from the CST, not from
/// a generated [`NodeId`].
fn content_order(tree: &IdentifiedTree, node: NodeId, root: NodeId) -> (usize, ObjectId) {
    match (site_of(tree, node, root), oid_of(tree, node, root)) {
        (Some(site), Some(oid)) => (site.len(), oid),
        _ => (usize::MAX, ObjectId::from_bytes([0; 32])),
    }
}

fn apply_replace(
    working: &mut IdentifiedTree,
    node_id: NodeId,
    to: ObjectId,
    root: NodeId,
) -> Result<(), Error> {
    let site = site_of(working, node_id, root).ok_or(Error::MissingId(node_id))?;
    let old = working.oid_at(&site).ok_or(Error::MissingId(node_id))?;
    if old == to {
        return Ok(());
    }
    set_at(working, &site, to)?;
    // Definitions inside the old content are gone; `to` brings none of ours.
    working
        .ids
        .retain(|key, _| !(key.len() > site.len() && key.starts_with(&site)));
    if node_id != root {
        working.ids.insert(site, node_id);
    }
    Ok(())
}

fn apply_delete(working: &mut IdentifiedTree, node_id: NodeId, root: NodeId) -> Result<(), Error> {
    let Some(site) = site_of(working, node_id, root) else {
        return Ok(());
    };
    remove_at(working, &site)?;
    Ok(())
}

fn apply_insert(
    working: &mut IdentifiedTree,
    parent: NodeId,
    index: u32,
    node: ObjectId,
    root: NodeId,
) -> Result<(), Error> {
    let (container, fallback) = insert_container(working, parent, root)?;
    let idx = child_index(working, &container, fallback, index)?;
    let at = insert_at(working, &container, idx, node)?;
    working.ids.entry(at).or_insert_with(NodeId::generate);
    Ok(())
}

fn apply_move(
    working: &mut IdentifiedTree,
    node_id: NodeId,
    to_parent: NodeId,
    index: u32,
    root: NodeId,
) -> Result<(), Error> {
    let site = site_of(working, node_id, root).ok_or(Error::MissingId(node_id))?;
    let oid = working.oid_at(&site).ok_or(Error::MissingId(node_id))?;
    // Ids at and below the moved node, relative to it.
    let carried: Vec<(Site, NodeId)> = working
        .ids
        .iter()
        .filter(|(key, _)| key.starts_with(&site))
        .map(|(key, id)| (key[site.len()..].to_vec(), *id))
        .collect();
    remove_at(working, &site)?;

    let dest_site = site_of(working, to_parent, root).ok_or(Error::MissingId(to_parent))?;
    let dest_oid = working
        .oid_at(&dest_site)
        .ok_or(Error::MissingId(to_parent))?;
    if already_has_child(&working.tree, dest_oid, oid) {
        return Ok(());
    }

    let (container, fallback) = insert_container(working, to_parent, root)?;
    let idx = child_index(working, &container, fallback, index)?;
    let at = insert_at(working, &container, idx, oid)?;
    for (suffix, id) in carried {
        let mut key = at.clone();
        key.extend(suffix);
        working.ids.insert(key, id);
    }
    Ok(())
}

fn already_has_child(tree: &NodeTree, ancestor: ObjectId, child: ObjectId) -> bool {
    let Some(node) = tree.get(ancestor) else {
        return false;
    };
    if node.children.contains(&child) {
        return true;
    }
    node.children
        .iter()
        .any(|c| already_has_child(tree, *c, child))
}

/// Where a child of `parent` goes: the first node at or below `parent`
/// whose children include an identified definition, else `parent` itself
/// (`true`: append before its closing token).
fn insert_container(
    working: &IdentifiedTree,
    parent: NodeId,
    root: NodeId,
) -> Result<(Site, bool), Error> {
    let parent_site = site_of(working, parent, root).ok_or(Error::MissingId(parent))?;
    let parent_oid = working
        .oid_at(&parent_site)
        .ok_or(Error::MissingId(parent))?;
    match find_def_container(working, parent_oid, &mut parent_site.clone()) {
        Some(site) => Ok((site, false)),
        None => Ok((parent_site, true)),
    }
}

fn find_def_container(working: &IdentifiedTree, oid: ObjectId, site: &mut Site) -> Option<Site> {
    let node = working.tree.get(oid)?;
    let has_def_child = (0..node.children.len()).any(|i| {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        let hit = working.ids.contains_key(site.as_slice());
        site.pop();
        hit
    });
    if has_def_child {
        return Some(site.clone());
    }
    for (i, child) in node.children.iter().enumerate() {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        let found = find_def_container(working, *child, site);
        site.pop();
        if found.is_some() {
            return found;
        }
    }
    None
}

fn child_index(
    working: &IdentifiedTree,
    container: &[u32],
    fallback: bool,
    index: u32,
) -> Result<usize, Error> {
    if !fallback {
        return Ok(index as usize);
    }
    let oid = working
        .oid_at(container)
        .ok_or_else(|| Error::apply("insert container is not in the tree"))?;
    Ok(fallback_insert_index(&working.tree, oid))
}

fn fallback_insert_index(tree: &NodeTree, container: ObjectId) -> usize {
    let Some(node) = tree.get(container) else {
        return 0;
    };
    if node.children.is_empty() {
        return 0;
    }
    let last = node.children[node.children.len() - 1];
    if let Some(child) = tree.get(last)
        && child.children.is_empty()
    {
        let raw = child.raw.as_slice();
        if matches!(raw, b"}" | b")" | b"]" | b";") {
            return node.children.len() - 1;
        }
    }
    node.children.len()
}

/// Re-intern `old` with `kids`. Sites do not change, so ids stay put.
fn intern_children(
    working: &mut IdentifiedTree,
    old: ObjectId,
    kids: Vec<ObjectId>,
) -> Result<ObjectId, Error> {
    let node = working
        .tree
        .get(old)
        .ok_or(Error::MissingNode(old))?
        .clone();
    Ok(working
        .tree
        .intern_branch(node.kind, node.lang, kids, node.name)?)
}

/// Put `new` at `site`, re-interning its ancestors.
fn set_at(working: &mut IdentifiedTree, site: &[u32], new: ObjectId) -> Result<(), Error> {
    if site.is_empty() {
        working.tree.set_root(new)?;
        return Ok(());
    }
    let path = chain(&working.tree, site)
        .ok_or_else(|| Error::apply(format!("site {site:?} is not in the tree")))?;
    splice_up(working, &path, new)
}

/// Remove the node at `site`: its ids go, later siblings' sites shift down.
fn remove_at(working: &mut IdentifiedTree, site: &[u32]) -> Result<(), Error> {
    let Some((&index, parent_site)) = site.split_last() else {
        return Err(Error::apply("cannot delete the CST root"));
    };
    let (parent, mut kids) = children_at(working, parent_site, || {
        Error::apply(format!("site {site:?} is not in the tree"))
    })?;
    if index as usize >= kids.len() {
        return Ok(());
    }
    kids.remove(index as usize);
    set_children(working, parent_site, parent, kids)?;
    remap_children(&mut working.ids, parent_site, |k| match k.cmp(&index) {
        std::cmp::Ordering::Less => Some(k),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(k - 1),
    });
    Ok(())
}

/// Insert `node` as child `index` of the node at `container`. Later
/// siblings' sites shift up. Returns the new node's site.
fn insert_at(
    working: &mut IdentifiedTree,
    container: &[u32],
    index: usize,
    node: ObjectId,
) -> Result<Site, Error> {
    let (parent, mut kids) = children_at(working, container, || {
        Error::apply("insert container is not in the tree")
    })?;
    let at = index.min(kids.len());
    kids.insert(at, node);
    set_children(working, container, parent, kids)?;
    let at = u32::try_from(at).unwrap_or(u32::MAX);
    remap_children(&mut working.ids, container, |k| {
        Some(if k >= at { k + 1 } else { k })
    });
    let mut site = container.to_vec();
    site.push(at);
    Ok(site)
}

/// Content id and children of the node at `site`; `missing` when there is
/// no node there.
fn children_at(
    working: &IdentifiedTree,
    site: &[u32],
    missing: impl FnOnce() -> Error,
) -> Result<(ObjectId, Vec<ObjectId>), Error> {
    let parent = working.oid_at(site).ok_or_else(missing)?;
    let kids = working
        .tree
        .get(parent)
        .ok_or(Error::MissingNode(parent))?
        .children
        .clone();
    Ok((parent, kids))
}

/// Re-intern `parent`, the node at `site`, with `kids`, and put it back.
fn set_children(
    working: &mut IdentifiedTree,
    site: &[u32],
    parent: ObjectId,
    kids: Vec<ObjectId>,
) -> Result<(), Error> {
    let new_parent = intern_children(working, parent, kids)?;
    set_at(working, site, new_parent)
}

/// Re-key every id below `parent` by mapping its child index there through
/// `f` (`None` drops it and everything below it).
fn remap_children(
    ids: &mut BTreeMap<Site, NodeId>,
    parent: &[u32],
    f: impl Fn(u32) -> Option<u32>,
) {
    let depth = parent.len();
    let affected: Vec<Site> = ids
        .keys()
        .filter(|key| key.len() > depth && key.starts_with(parent))
        .cloned()
        .collect();
    let mut moved = Vec::with_capacity(affected.len());
    for key in affected {
        if let Some(id) = ids.remove(&key)
            && let Some(k) = f(key[depth])
        {
            let mut new_key = key;
            new_key[depth] = k;
            moved.push((new_key, id));
        }
    }
    ids.extend(moved);
}

/// Swap `current` in along `path` (from [`chain`] on the same tree, so every
/// index is in range), re-interning each ancestor up to the root.
fn splice_up(
    working: &mut IdentifiedTree,
    path: &[(ObjectId, usize)],
    mut current: ObjectId,
) -> Result<(), Error> {
    for &(old_parent, index) in path.iter().rev() {
        let mut kids = working
            .tree
            .get(old_parent)
            .ok_or(Error::MissingNode(old_parent))?
            .children
            .clone();
        kids[index] = current;
        current = intern_children(working, old_parent, kids)?;
    }
    working.tree.set_root(current)?;
    Ok(())
}
