//! [`hord_core::IdentityMap`] view of one snapshot.

use std::collections::BTreeMap;

use hord_core::{IdentityDelta, IdentityMap, NodeId, NodePath, RepoPath};
use hord_lang::IdentifiedTree;

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
/// Each file with a root also gets [`NodeId::file_root`] at an empty
/// pointer (ADR 0015), unless a definition is itself the root.
///
/// # Errors
///
/// [`Error::DuplicateNode`] if one [`NodeId`] occurs at two paths.
/// [`Error::Unmapped`] if an id's site is not a node of its file's tree.
pub fn identity_map(files: &[SnapshotFile], deltas: Vec<IdentityDelta>) -> Result<IdentityMap> {
    let mut nodes = BTreeMap::new();
    for file in files {
        locate(&file.path, &file.identified, &mut nodes)?;
        let root_taken = nodes
            .values()
            .any(|at| at.file == file.path && at.pointer.is_empty());
        if file.identified.tree.root().is_some() && !root_taken {
            let root = NodeId::file_root(&file.path);
            if nodes.contains_key(&root) {
                return Err(Error::DuplicateNode(root));
            }
            nodes.insert(
                root,
                NodePath {
                    file: file.path.clone(),
                    pointer: Vec::new(),
                },
            );
        }
    }
    Ok(IdentityMap { nodes, deltas })
}

fn locate(
    path: &RepoPath,
    identified: &IdentifiedTree,
    nodes: &mut BTreeMap<NodeId, NodePath>,
) -> Result<()> {
    for (site, id) in &identified.ids {
        if identified.oid_at(site).is_none() {
            return Err(Error::Unmapped(*id));
        }
    }
    // Sites order lexicographically, which is preorder.
    for (site, &id) in &identified.ids {
        let at = NodePath {
            file: path.clone(),
            pointer: site.clone(),
        };
        if nodes.insert(id, at).is_some() {
            return Err(Error::DuplicateNode(id));
        }
    }
    Ok(())
}
