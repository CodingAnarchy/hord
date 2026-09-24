//! Node identity maps and deltas (spec §3.2, §3.4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{NodeId, RepoPath};

/// Per-snapshot mapping `NodeId → path-in-tree`, plus births/deaths/derivations.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IdentityMap {
    /// Location of every identified definition in the snapshot tree.
    pub nodes: BTreeMap<NodeId, NodePath>,
    /// Births, deaths, and derivations relative to the parent snapshot.
    pub deltas: Vec<IdentityDelta>,
}

/// Location of a node inside a snapshot: file path plus a child-index walk
/// from the [`crate::NodeFile`] root.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct NodePath {
    /// Repository path of the file that contains the node.
    pub file: RepoPath,
    /// Child indices from the file root [`crate::Node`] down to this node.
    pub pointer: Vec<u32>,
}

/// Birth, death, or declared derivation of a [`NodeId`] (spec §3.4).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum IdentityDelta {
    /// A definition that did not exist in the base tree.
    Birth {
        /// Newly assigned identity.
        node: NodeId,
    },
    /// A definition that no longer exists in the result tree.
    Death {
        /// Retired identity.
        node: NodeId,
    },
    /// `node` was derived from `from` (rename, copy, or explicit declaration).
    DerivedFrom {
        /// Result identity.
        node: NodeId,
        /// Base identity it was derived from.
        from: NodeId,
    },
    /// `node` was split into `into`.
    SplitInto {
        /// Base identity that was split.
        node: NodeId,
        /// Result identities.
        into: Vec<NodeId>,
    },
    /// `node` was merged from `from`.
    MergedFrom {
        /// Result identity.
        node: NodeId,
        /// Base identities that were merged.
        from: Vec<NodeId>,
    },
}

impl IdentityDelta {
    /// Every [`NodeId`] the delta names: `node`, then `from` or `into`.
    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        let (node, rest): (&NodeId, &[NodeId]) = match self {
            Self::Birth { node } | Self::Death { node } => (node, &[]),
            Self::DerivedFrom { node, from } => (node, std::slice::from_ref(from)),
            Self::SplitInto { node, into } => (node, into),
            Self::MergedFrom { node, from } => (node, from),
        };
        std::iter::once(*node).chain(rest.iter().copied())
    }
}

/// One directory of a snapshot's identity (ADR 0017): name → subdirectory or
/// [`FileIdentity`].
///
/// Merkle-shaped like [`crate::Tree`], so unchanged subtrees share objects
/// between snapshots. A parsed file whose ids equal the fresh deterministic
/// assignment (ADR 0019) has no entry; readers fall back to that
/// assignment. Directories with no entries are omitted, except the root.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IdentityTree {
    /// Child entries, keyed by basename. Encoded as a CBOR map.
    pub entries: BTreeMap<String, IdentityEntry>,
}

/// One child of an [`IdentityTree`].
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum IdentityEntry {
    /// Nested [`IdentityTree`] object.
    Dir(crate::ObjectId),
    /// [`FileIdentity`] object of the file with this name.
    File(crate::ObjectId),
}

/// Where [`edit_identity_tree`] reads and writes [`IdentityTree`] objects.
pub trait IdentityTrees {
    /// Error of a read or a write.
    type Error;

    /// The identity tree object `id`.
    fn load(&mut self, id: crate::ObjectId) -> Result<IdentityTree, Self::Error>;

    /// Store `tree` and return its id.
    fn store(&mut self, tree: IdentityTree) -> Result<crate::ObjectId, Self::Error>;
}

/// A path's path components and its new [`FileIdentity`] object, or `None`
/// to remove its entry ([`edit_identity_tree`]).
pub type IdentityEdit<'a> = (&'a [String], Option<crate::ObjectId>);

/// The identity tree `tree` (`None`: an empty one) with `edits` applied,
/// stored: each is a path relative to `tree` and its new
/// [`FileIdentity`] object, or `None` to remove the file's entry.
/// Directories on edited paths are rewritten; emptied ones are dropped, and
/// a missing one is not created only to remove from it. `None` when the
/// result is empty.
pub fn edit_identity_tree<T: IdentityTrees + ?Sized>(
    trees: &mut T,
    tree: Option<crate::ObjectId>,
    edits: &[IdentityEdit<'_>],
) -> Result<Option<crate::ObjectId>, T::Error> {
    let mut out = match tree {
        Some(id) => trees.load(id)?,
        None => IdentityTree::default(),
    };
    let mut nested: BTreeMap<&str, Vec<IdentityEdit<'_>>> = BTreeMap::new();
    for (components, identity) in edits {
        match components {
            [name] => match identity {
                Some(id) => {
                    out.entries.insert(name.clone(), IdentityEntry::File(*id));
                }
                None => {
                    if matches!(out.entries.get(name), Some(IdentityEntry::File(_))) {
                        out.entries.remove(name);
                    }
                }
            },
            [dir, rest @ ..] => nested
                .entry(dir.as_str())
                .or_default()
                .push((rest, *identity)),
            [] => {}
        }
    }
    for (dir, sub) in nested {
        let existing = match out.entries.get(dir) {
            Some(IdentityEntry::Dir(id)) => Some(*id),
            _ => None,
        };
        if existing.is_none() && sub.iter().all(|(_, id)| id.is_none()) {
            continue;
        }
        match edit_identity_tree(trees, existing, &sub)? {
            Some(id) => {
                out.entries.insert(dir.to_owned(), IdentityEntry::Dir(id));
            }
            None => {
                out.entries.remove(dir);
            }
        }
    }
    if out.entries.is_empty() {
        return Ok(None);
    }
    trees.store(out).map(Some)
}

/// The [`NodeId`]s of one parsed file (ADR 0017).
///
/// Bound to the blob they were computed for: a reader whose tree has a
/// different blob at the path must not use them.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    /// [`crate::Blob`] the ids describe.
    pub blob: crate::ObjectId,
    /// Definition site (the child-index walk from the file's root node) →
    /// identity, sorted by site.
    pub nodes: Vec<(Vec<u32>, NodeId)>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ObjectId;

    #[derive(Default)]
    struct Memory(HashMap<ObjectId, IdentityTree>);

    impl IdentityTrees for Memory {
        type Error = std::convert::Infallible;

        fn load(&mut self, id: ObjectId) -> Result<IdentityTree, Self::Error> {
            Ok(self.0[&id].clone())
        }

        fn store(&mut self, tree: IdentityTree) -> Result<ObjectId, Self::Error> {
            let id = ObjectId::of(&tree).expect("an identity tree encodes");
            self.0.insert(id, tree);
            Ok(id)
        }
    }

    fn path(p: &str) -> Vec<String> {
        p.split('/').map(str::to_owned).collect()
    }

    /// Edits record and remove file entries along their paths, drop
    /// emptied directories, and create none only to remove from it.
    #[test]
    fn edits_rewrite_the_paths_and_drop_emptied_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut trees = Memory::default();
        let (a, b) = (
            ObjectId::from_canonical(b"a"),
            ObjectId::from_canonical(b"b"),
        );
        let (x, y, z) = (path("src/x.rs"), path("src/deep/y.rs"), path("z.rs"));
        let root = edit_identity_tree(
            &mut trees,
            None,
            &[(&x, Some(a)), (&y, Some(b)), (&z, Some(a))],
        )?
        .ok_or("recording files leaves a root")?;
        let top = trees.0[&root].clone();
        assert_eq!(top.entries.len(), 2);
        assert_eq!(top.entries["z.rs"], IdentityEntry::File(a));
        let Some(IdentityEntry::Dir(src)) = top.entries.get("src").cloned() else {
            panic!("src is a directory");
        };
        assert_eq!(trees.0[&src].entries["x.rs"], IdentityEntry::File(a));

        let (gone, missing) = (path("src/deep"), path("nowhere/w.rs"));
        let pruned = edit_identity_tree(
            &mut trees,
            Some(root),
            &[(&y, None), (&x, None), (&gone, None), (&missing, None)],
        )?
        .ok_or("z.rs still keeps the root")?;
        let top = &trees.0[&pruned];
        assert_eq!(top.entries.len(), 1, "src emptied and dropped: {top:?}");
        assert_eq!(top.entries["z.rs"], IdentityEntry::File(a));
        assert_eq!(
            edit_identity_tree(&mut trees, Some(pruned), &[(&z, None)])?,
            None
        );
        Ok(())
    }
}
