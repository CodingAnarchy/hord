//! [`hord_core::IdentityMap`] view of one snapshot.

use std::collections::BTreeMap;

use hord_core::{IdentityDelta, IdentityMap, NodeId, NodePath, ObjectId, RepoPath};
use hord_lang::{IdentifiedTree, NodeTree};

use crate::Result;
use crate::error::Error;

/// One file in a snapshot, with definition [`NodeId`]s already assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotFile {
    /// Repository path of this file.
    pub path: RepoPath,
    /// Interned CST plus the definition ids carried or assigned in it.
    pub identified: IdentifiedTree,
}

/// Build the [`IdentityMap`] for one snapshot.
///
/// `nodes` locates every definition id reachable from a file root.
/// [`NodePath::pointer`] is the child-index walk from that root; a definition
/// that is itself the root has an empty pointer. `deltas` are stored unchanged
/// (births, deaths, and derivations relative to the parent snapshot).
///
/// # Errors
///
/// [`Error::DuplicateNode`] if one [`NodeId`] occurs at two paths.
/// [`Error::MissingNode`] if an id names a content object that is not interned.
/// [`Error::Unmapped`] if an id is interned but not reachable from the file root.
pub fn identity_map(files: &[SnapshotFile], deltas: Vec<IdentityDelta>) -> Result<IdentityMap> {
    let mut nodes = BTreeMap::new();
    for file in files {
        locate(&file.path, &file.identified, &mut nodes)?;
    }
    Ok(IdentityMap { nodes, deltas })
}

fn locate(
    path: &RepoPath,
    identified: &IdentifiedTree,
    nodes: &mut BTreeMap<NodeId, NodePath>,
) -> Result<()> {
    for oid in identified.ids.keys() {
        if !identified.tree.contains(*oid) {
            return Err(Error::MissingNode(*oid));
        }
    }
    if let Some(root) = identified.tree.root() {
        let mut pointer = Vec::new();
        walk(
            path,
            &identified.tree,
            &identified.ids,
            root,
            &mut pointer,
            nodes,
        )?;
    }
    for id in identified.ids.values() {
        if !nodes.contains_key(id) {
            return Err(Error::Unmapped(*id));
        }
    }
    Ok(())
}

fn walk(
    path: &RepoPath,
    tree: &NodeTree,
    ids: &BTreeMap<ObjectId, NodeId>,
    oid: ObjectId,
    pointer: &mut Vec<u32>,
    nodes: &mut BTreeMap<NodeId, NodePath>,
) -> Result<()> {
    if let Some(&node_id) = ids.get(&oid) {
        let inserted = nodes.insert(
            node_id,
            NodePath {
                file: path.clone(),
                pointer: pointer.clone(),
            },
        );
        if inserted.is_some() {
            return Err(Error::DuplicateNode(node_id));
        }
    }
    let node = tree.get(oid).ok_or(Error::MissingNode(oid))?;
    for (i, child) in node.children.iter().enumerate() {
        let index = u32::try_from(i).unwrap_or(u32::MAX);
        pointer.push(index);
        walk(path, tree, ids, *child, pointer, nodes)?;
        pointer.pop();
    }
    Ok(())
}
