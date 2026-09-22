//! Apply a definition-granularity edit script to an identified tree.

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{NodeId, ObjectId, Op};
use hord_lang::{IdentifiedTree, NodeTree};

use crate::Error;
use crate::defs::{file_parent, oid_of, path_to};
use crate::graft::graft;

/// Apply `ops` to `base`, producing a new identified tree.
///
/// Returns an [`IdentifiedTree`]: interned CST plus the definition
/// [`NodeId`] map after the edit. [`Op::Insert`] / [`Op::Replace`] `to`
/// nodes must already be interned in `store` (the `result` tree passed to
/// [`crate::diff`], or a union of trees for a merge).
///
/// File-root parent is [`file_parent`] ([`NodeId::nil`]). A Replace of that
/// id swaps the CST root.
///
/// Ops are classified and applied in a fixed order (replaces, deletes,
/// moves, inserts). Insert indices are result-side CST positions and are
/// interpreted after deletes so they line up with the result child list.
pub fn apply(base: &IdentifiedTree, ops: &[Op], store: &NodeTree) -> Result<IdentifiedTree, Error> {
    let mut working = base.clone();
    if working.tree.root().is_none() {
        if let Some(Op::Replace { to, .. }) = ops
            .iter()
            .find(|o| matches!(o, Op::Replace { node, .. } if *node == file_parent()))
        {
            graft(&mut working.tree, store, *to)?;
            working.tree.set_root(*to)?;
            working.ids.retain(|oid, _| working.tree.contains(*oid));
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
            Op::Replace { .. } => replaces.push(op),
            Op::Insert { .. } => inserts.push(op),
            Op::Move { .. } => moves.push(op),
            Op::Delete { .. } => deletes.push(op),
            // M1: name lives in source tokens; a paired Replace carries the
            // new body. Rename is recorded for merge rule 4.
            Op::Rename { .. } | Op::Blob { .. } | Op::Tree { .. } => {}
        }
    }

    // Ancestors first, then content id. `NodeId` is a random ULID and must
    // not decide which edit lands.
    replaces.sort_by_key(|op| match op {
        Op::Replace { node, .. } => content_order(base, *node),
        _ => (usize::MAX, ObjectId::from_bytes([0xff; 32])),
    });

    for op in replaces {
        let Op::Replace { node, to, .. } = op else {
            continue;
        };
        graft(&mut working.tree, store, *to)?;
        apply_replace(&mut working, *node, *to)?;
    }

    deletes.sort_by_key(|op| match op {
        Op::Delete { node } => {
            let (depth, oid) = content_order(&working, *node);
            (std::cmp::Reverse(depth), oid)
        }
        _ => (std::cmp::Reverse(0), ObjectId::from_bytes([0; 32])),
    });

    for op in deletes {
        let Op::Delete { node } = op else {
            continue;
        };
        apply_delete(&mut working, *node)?;
    }

    moves.sort_by_key(|op| match op {
        Op::Move {
            node,
            to_parent,
            index,
            ..
        } => {
            let (_, oid) = content_order(&working, *node);
            let (_, parent) = content_order(&working, *to_parent);
            (*index, parent, oid)
        }
        _ => (
            0,
            ObjectId::from_bytes([0; 32]),
            ObjectId::from_bytes([0; 32]),
        ),
    });

    for op in moves {
        let Op::Move {
            node,
            to_parent,
            index,
            ..
        } = op
        else {
            continue;
        };
        apply_move(&mut working, *node, *to_parent, *index)?;
    }

    // Inserts last so `index` is the result-side CST index after deletes.
    for op in inserts {
        let Op::Insert {
            parent,
            index,
            node,
        } = op
        else {
            continue;
        };
        graft(&mut working.tree, store, *node)?;
        apply_insert(&mut working, *parent, *index, *node)?;
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
            let new_root = restore_trailing_commas(&mut working, root, &seps)?;
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
    let mut ids: Vec<ObjectId> = store.iter().map(|(id, _)| id).collect();
    ids.sort();
    let mut map = BTreeMap::new();
    for id in ids {
        let Some(parent) = store.get(id) else {
            continue;
        };
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
fn restore_trailing_commas(
    working: &mut IdentifiedTree,
    id: ObjectId,
    seps: &BTreeMap<ObjectId, ObjectId>,
) -> Result<ObjectId, Error> {
    let node = working.tree.get(id).ok_or(Error::MissingNode(id))?.clone();
    let mut rebuilt = Vec::with_capacity(node.children.len());
    let mut changed = false;
    for child in &node.children {
        let new_child = restore_trailing_commas(working, *child, seps)?;
        if new_child != *child {
            changed = true;
        }
        rebuilt.push(new_child);
    }
    let mut kids = Vec::with_capacity(rebuilt.len());
    for (index, child) in rebuilt.iter().enumerate() {
        kids.push(*child);
        let Some(sep) = seps.get(child) else {
            continue;
        };
        let next_is_sep = rebuilt.get(index + 1).is_some_and(|next| next == sep);
        if !next_is_sep {
            kids.push(*sep);
            changed = true;
        }
    }
    if !changed {
        return Ok(id);
    }
    let new_id = working
        .tree
        .intern_branch(node.kind, node.lang, kids, node.name)?;
    if let Some(nid) = working.ids.remove(&id) {
        working.ids.insert(new_id, nid);
    }
    Ok(new_id)
}

/// Tree depth, then the node's content id. Both come from the CST, not from
/// a generated [`NodeId`].
fn content_order(tree: &IdentifiedTree, node: NodeId) -> (usize, ObjectId) {
    let Some(oid) = oid_of(tree, node) else {
        return (usize::MAX, ObjectId::from_bytes([0; 32]));
    };
    let depth = path_to(&tree.tree, oid)
        .map(|path| path.len())
        .unwrap_or(usize::MAX);
    (depth, oid)
}

fn apply_replace(working: &mut IdentifiedTree, node_id: NodeId, to: ObjectId) -> Result<(), Error> {
    let old = oid_of(working, node_id).ok_or(Error::MissingId(node_id))?;
    if old == to {
        return Ok(());
    }
    splice_oid(working, old, to)?;
    working.ids.remove(&old);
    working.ids.insert(to, node_id);
    Ok(())
}

fn apply_delete(working: &mut IdentifiedTree, node_id: NodeId) -> Result<(), Error> {
    let Some(old) = oid_of(working, node_id) else {
        return Ok(());
    };
    remove_oid(working, old)?;
    drop_subtree_ids(working, old);
    Ok(())
}

fn apply_insert(
    working: &mut IdentifiedTree,
    parent: NodeId,
    index: u32,
    node: ObjectId,
) -> Result<(), Error> {
    let site = insert_container(working, parent)?;
    let idx = if site.fallback {
        fallback_insert_index(&working.tree, site.container)
    } else {
        index as usize
    };
    insert_oid(working, site.container, &site.path, idx, node)?;
    working.ids.entry(node).or_insert_with(NodeId::generate);
    Ok(())
}

fn apply_move(
    working: &mut IdentifiedTree,
    node_id: NodeId,
    to_parent: NodeId,
    index: u32,
) -> Result<(), Error> {
    let oid = oid_of(working, node_id).ok_or(Error::MissingId(node_id))?;
    remove_oid(working, oid)?;

    let dest_oid = oid_of(working, to_parent).ok_or(Error::MissingId(to_parent))?;
    if already_has_child(&working.tree, dest_oid, oid) {
        return Ok(());
    }

    let site = insert_container(working, to_parent)?;
    let idx = if site.fallback {
        fallback_insert_index(&working.tree, site.container)
    } else {
        index as usize
    };
    insert_oid(working, site.container, &site.path, idx, oid)?;
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

struct InsertSite {
    container: ObjectId,
    path: Vec<(ObjectId, usize)>,
    fallback: bool,
}

fn insert_container(working: &IdentifiedTree, parent: NodeId) -> Result<InsertSite, Error> {
    let parent_oid = oid_of(working, parent).ok_or(Error::MissingId(parent))?;
    let path_to_parent =
        path_to(&working.tree, parent_oid).ok_or(Error::MissingNode(parent_oid))?;
    if let Some(container) = find_def_container(&working.tree, parent_oid, &working.ids) {
        let path = path_to(&working.tree, container).ok_or(Error::MissingNode(container))?;
        Ok(InsertSite {
            container,
            path,
            fallback: false,
        })
    } else {
        Ok(InsertSite {
            container: parent_oid,
            path: path_to_parent,
            fallback: true,
        })
    }
}

fn find_def_container(
    tree: &NodeTree,
    start: ObjectId,
    ids: &std::collections::BTreeMap<ObjectId, NodeId>,
) -> Option<ObjectId> {
    fn walk(
        tree: &NodeTree,
        oid: ObjectId,
        start: ObjectId,
        ids: &std::collections::BTreeMap<ObjectId, NodeId>,
    ) -> Option<ObjectId> {
        let node = tree.get(oid)?;
        if node.children.iter().any(|c| ids.contains_key(c)) {
            return Some(oid);
        }
        for child in &node.children {
            if ids.contains_key(child) && *child != start {
                continue;
            }
            if let Some(found) = walk(tree, *child, start, ids) {
                return Some(found);
            }
        }
        None
    }
    walk(tree, start, start, ids)
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

fn splice_oid(working: &mut IdentifiedTree, old: ObjectId, new: ObjectId) -> Result<(), Error> {
    let path = path_to(&working.tree, old).ok_or(Error::MissingNode(old))?;
    if path.is_empty() {
        working.tree.set_root(new)?;
        return Ok(());
    }
    splice_up(working, &path, new)
}

fn remove_oid(working: &mut IdentifiedTree, oid: ObjectId) -> Result<(), Error> {
    let path = path_to(&working.tree, oid).ok_or(Error::MissingNode(oid))?;
    if path.is_empty() {
        return Err(Error::apply("cannot delete the CST root"));
    }
    let &(parent, index) = path.last().expect("path non-empty");
    let p = working
        .tree
        .get(parent)
        .ok_or(Error::MissingNode(parent))?
        .clone();
    let mut kids = p.children;
    if index >= kids.len() || kids[index] != oid {
        let Some(real) = kids.iter().position(|c| *c == oid) else {
            return Ok(());
        };
        kids.remove(real);
    } else {
        kids.remove(index);
    }
    let new_parent = working.tree.intern_branch(p.kind, p.lang, kids, p.name)?;
    if let Some(nid) = working.ids.remove(&parent) {
        working.ids.insert(new_parent, nid);
    }
    let parent_path = &path[..path.len() - 1];
    if parent_path.is_empty() {
        working.tree.set_root(new_parent)?;
        Ok(())
    } else {
        splice_up(working, parent_path, new_parent)
    }
}

fn insert_oid(
    working: &mut IdentifiedTree,
    container: ObjectId,
    path: &[(ObjectId, usize)],
    index: usize,
    node: ObjectId,
) -> Result<(), Error> {
    let c = working
        .tree
        .get(container)
        .ok_or(Error::MissingNode(container))?
        .clone();
    let mut kids = c.children;
    let at = index.min(kids.len());
    kids.insert(at, node);
    let new_container = working.tree.intern_branch(c.kind, c.lang, kids, c.name)?;
    if let Some(nid) = working.ids.remove(&container) {
        working.ids.insert(new_container, nid);
    }
    if path.is_empty() {
        working.tree.set_root(new_container)?;
        Ok(())
    } else {
        splice_up(working, path, new_container)
    }
}

fn splice_up(
    working: &mut IdentifiedTree,
    path: &[(ObjectId, usize)],
    mut current: ObjectId,
) -> Result<(), Error> {
    for &(old_parent, index) in path.iter().rev() {
        let p = working
            .tree
            .get(old_parent)
            .ok_or(Error::MissingNode(old_parent))?
            .clone();
        let mut kids = p.children;
        if index >= kids.len() {
            return Err(Error::apply(format!(
                "child index {index} out of range for {old_parent} ({} children)",
                kids.len()
            )));
        }
        kids[index] = current;
        current = working.tree.intern_branch(p.kind, p.lang, kids, p.name)?;
        if let Some(nid) = working.ids.remove(&old_parent) {
            working.ids.insert(current, nid);
        }
    }
    working.tree.set_root(current)?;
    Ok(())
}

fn drop_subtree_ids(working: &mut IdentifiedTree, oid: ObjectId) {
    let mut stack = vec![oid];
    let mut seen = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        working.ids.remove(&id);
        if let Some(node) = working.tree.get(id) {
            stack.extend(node.children.iter().copied());
        }
    }
}
