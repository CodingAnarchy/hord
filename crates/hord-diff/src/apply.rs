//! Apply a definition-granularity edit script to an identified tree.

use std::cmp::Reverse;
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
            Op::Replace { node, to, .. } => replaces.push(ReplaceEdit {
                node: *node,
                to: *to,
            }),
            Op::Insert {
                parent,
                index,
                node,
            } => inserts.push(InsertEdit {
                parent: *parent,
                index: *index,
                node: *node,
            }),
            Op::Move {
                node,
                to_parent,
                index,
                ..
            } => moves.push(MoveEdit {
                node: *node,
                to_parent: *to_parent,
                index: *index,
            }),
            Op::Delete { node } => deletes.push(*node),
            // M1: name lives in source tokens; a paired Replace carries the
            // new body. Rename is recorded for merge rule 4.
            Op::Rename { .. } | Op::Blob { .. } | Op::Tree { .. } => {}
        }
    }

    // Ancestors first, then content id. `NodeId` is a random ULID and must
    // not decide which edit lands.
    replaces.sort_by_key(|op| content_order(base, op.node));

    for op in replaces {
        graft(&mut working.tree, store, op.to)?;
        apply_replace(&mut working, op.node, op.to)?;
    }

    deletes.sort_by_key(|node| {
        let (depth, oid) = content_order(&working, *node);
        (Reverse(depth), oid)
    });

    for node in deletes {
        apply_delete(&mut working, node)?;
    }

    moves.sort_by_key(|op| {
        let (_, oid) = content_order(&working, op.node);
        let (_, parent) = content_order(&working, op.to_parent);
        (op.index, parent, oid)
    });

    for op in moves {
        apply_move(&mut working, op.node, op.to_parent, op.index)?;
    }

    // Inserts last so `index` is the result-side CST index after deletes.
    for op in inserts {
        graft(&mut working.tree, store, op.node)?;
        apply_insert(&mut working, op.parent, op.index, op.node)?;
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
        changed |= new_child != *child;
        rebuilt.push(new_child);
    }
    let mut kids = Vec::with_capacity(rebuilt.len());
    for (index, child) in rebuilt.iter().enumerate() {
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
    intern_children(working, id, kids)
}

struct ReplaceEdit {
    node: NodeId,
    to: ObjectId,
}

struct MoveEdit {
    node: NodeId,
    to_parent: NodeId,
    index: u32,
}

struct InsertEdit {
    parent: NodeId,
    index: u32,
    node: ObjectId,
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
    let idx = child_index(&working.tree, &site, index);
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
    let idx = child_index(&working.tree, &site, index);
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
    ids: &BTreeMap<ObjectId, NodeId>,
) -> Option<ObjectId> {
    fn walk(tree: &NodeTree, oid: ObjectId, ids: &BTreeMap<ObjectId, NodeId>) -> Option<ObjectId> {
        let node = tree.get(oid)?;
        if node.children.iter().any(|c| ids.contains_key(c)) {
            return Some(oid);
        }
        for child in &node.children {
            if let Some(found) = walk(tree, *child, ids) {
                return Some(found);
            }
        }
        None
    }
    walk(tree, start, ids)
}

fn child_index(tree: &NodeTree, site: &InsertSite, index: u32) -> usize {
    if site.fallback {
        fallback_insert_index(tree, site.container)
    } else {
        index as usize
    }
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
    let new_id = working
        .tree
        .intern_branch(node.kind, node.lang, kids, node.name)?;
    if let Some(nid) = working.ids.remove(&old) {
        working.ids.insert(new_id, nid);
    }
    Ok(new_id)
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
    let mut kids = working
        .tree
        .get(parent)
        .ok_or(Error::MissingNode(parent))?
        .children
        .clone();
    if index >= kids.len() || kids[index] != oid {
        let Some(real) = kids.iter().position(|c| *c == oid) else {
            return Ok(());
        };
        kids.remove(real);
    } else {
        kids.remove(index);
    }
    let parent_path = &path[..path.len() - 1];
    replace_children(working, parent, parent_path, kids)
}

fn insert_oid(
    working: &mut IdentifiedTree,
    container: ObjectId,
    path: &[(ObjectId, usize)],
    index: usize,
    node: ObjectId,
) -> Result<(), Error> {
    let mut kids = working
        .tree
        .get(container)
        .ok_or(Error::MissingNode(container))?
        .children
        .clone();
    let at = index.min(kids.len());
    kids.insert(at, node);
    replace_children(working, container, path, kids)
}

fn replace_children(
    working: &mut IdentifiedTree,
    parent: ObjectId,
    path_to_parent: &[(ObjectId, usize)],
    kids: Vec<ObjectId>,
) -> Result<(), Error> {
    let new_parent = intern_children(working, parent, kids)?;
    if path_to_parent.is_empty() {
        working.tree.set_root(new_parent)?;
        Ok(())
    } else {
        splice_up(working, path_to_parent, new_parent)
    }
}

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
        if index >= kids.len() {
            return Err(Error::apply(format!(
                "child index {index} out of range for {old_parent} ({} children)",
                kids.len()
            )));
        }
        kids[index] = current;
        current = intern_children(working, old_parent, kids)?;
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
