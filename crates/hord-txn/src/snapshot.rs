//! Reading and writing snapshot trees (spec §3.2, §3.7).
//!
//! Snapshots written here keep the Tier 0 shape that `hord-git` imports:
//! every file is a [`TreeEntry::Blob`]. Parsed structure is recomputed from
//! the bytes (parsing is deterministic), so a structural op's node objects
//! are reproducible from the result blob without storing each CST node.

use std::collections::BTreeMap;
use std::sync::Arc;

use hord_core::{Blob, Bytes, ObjectId, RepoPath, SnapshotId, Tree, TreeEntry};

use crate::repo::{Inner, lock};
use crate::{Error, Result};

/// Path components (relative to a subtree) and the blob to put there.
type PathChanges<'a> = Vec<(&'a [String], Option<ObjectId>)>;
type PathChangesRef<'s, 'a> = &'s [(&'a [String], Option<ObjectId>)];

/// One file that differs between two snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileDelta {
    pub path: RepoPath,
    pub from: Option<ObjectId>,
    pub to: Option<ObjectId>,
}

/// Cap on cached tree objects.
const MAX_CACHED_TREES: usize = 65_536;

/// [`ObjectId`] of `bytes` stored as a [`Blob`], without storing it.
pub(crate) fn blob_object_id(bytes: &[u8]) -> Result<ObjectId> {
    Ok(ObjectId::of(&Blob::new(bytes.to_vec()))?)
}

impl Inner {
    pub(crate) fn tree(&self, id: ObjectId) -> Result<Arc<Tree>> {
        if let Some(tree) = lock(&self.trees).get(&id) {
            return Ok(Arc::clone(tree));
        }
        let tree: Arc<Tree> = Arc::new(self.store.get_object(id)?);
        let mut cache = lock(&self.trees);
        if cache.len() >= MAX_CACHED_TREES {
            cache.clear();
        }
        cache.insert(id, Arc::clone(&tree));
        Ok(tree)
    }

    /// The blob at `path` in `root`. `None` when nothing (or a directory) is
    /// there.
    pub(crate) fn blob_id(&self, root: SnapshotId, path: &RepoPath) -> Result<Option<ObjectId>> {
        let components = path.components();
        let Some((last, dirs)) = components.split_last() else {
            return Ok(None);
        };
        let mut tree = self.tree(root)?;
        for dir in dirs {
            match tree.entries.get(dir) {
                Some(TreeEntry::Tree(id)) => {
                    let id = *id;
                    tree = self.tree(id)?;
                }
                _ => return Ok(None),
            }
        }
        match tree.entries.get(last) {
            Some(TreeEntry::Blob(id)) => Ok(Some(*id)),
            Some(TreeEntry::Tree(_)) | None => Ok(None),
            Some(TreeEntry::NodeFile(_)) => Err(Error::UnsupportedEntry {
                path: path.clone(),
                kind: "NodeFile",
            }),
        }
    }

    pub(crate) fn blob_bytes(&self, id: ObjectId) -> Result<Bytes> {
        let blob: Blob = self.store.get_object(id)?;
        Ok(blob.bytes)
    }

    pub(crate) fn file_bytes(&self, root: SnapshotId, path: &RepoPath) -> Result<Option<Bytes>> {
        match self.blob_id(root, path)? {
            Some(id) => Ok(Some(self.blob_bytes(id)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn put_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        Ok(self.store.put_object(&Blob::new(bytes.to_vec()))?)
    }

    /// Every file in `root`, in path order.
    pub(crate) fn list_files(&self, root: SnapshotId) -> Result<Vec<(RepoPath, ObjectId)>> {
        let mut out = Vec::new();
        let mut prefix = Vec::new();
        self.walk_files(root, &mut prefix, &mut out)?;
        Ok(out)
    }

    fn walk_files(
        &self,
        tree: ObjectId,
        prefix: &mut Vec<String>,
        out: &mut Vec<(RepoPath, ObjectId)>,
    ) -> Result<()> {
        let tree = self.tree(tree)?;
        for (name, entry) in &tree.entries {
            prefix.push(name.clone());
            match entry {
                TreeEntry::Blob(id) => out.push((RepoPath::new(prefix.clone()), *id)),
                TreeEntry::Tree(id) => self.walk_files(*id, prefix, out)?,
                TreeEntry::NodeFile(_) => {
                    return Err(Error::UnsupportedEntry {
                        path: RepoPath::new(prefix.clone()),
                        kind: "NodeFile",
                    });
                }
            }
            prefix.pop();
        }
        Ok(())
    }

    /// Files that differ between `from` and `to`, in path order, with their
    /// blobs on each side. Shared subtrees are skipped by id.
    pub(crate) fn changed_paths(&self, from: SnapshotId, to: SnapshotId) -> Result<Vec<FileDelta>> {
        let mut out = Vec::new();
        self.diff_trees(Some(from), Some(to), &mut Vec::new(), &mut out)?;
        Ok(out)
    }

    fn diff_trees(
        &self,
        from: Option<ObjectId>,
        to: Option<ObjectId>,
        prefix: &mut Vec<String>,
        out: &mut Vec<FileDelta>,
    ) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let empty = Arc::new(Tree::default());
        let a = match from {
            Some(id) => self.tree(id)?,
            None => Arc::clone(&empty),
        };
        let b = match to {
            Some(id) => self.tree(id)?,
            None => empty,
        };
        let names: std::collections::BTreeSet<&String> =
            a.entries.keys().chain(b.entries.keys()).collect();
        for name in names {
            let (left, right) = (a.entries.get(name), b.entries.get(name));
            if left == right {
                continue;
            }
            prefix.push(name.clone());
            let blob = |e: Option<&TreeEntry>| match e {
                Some(TreeEntry::Blob(id)) => Some(*id),
                _ => None,
            };
            let tree = |e: Option<&TreeEntry>| match e {
                Some(TreeEntry::Tree(id)) => Some(*id),
                _ => None,
            };
            if matches!(left, Some(TreeEntry::NodeFile(_)))
                || matches!(right, Some(TreeEntry::NodeFile(_)))
            {
                return Err(Error::UnsupportedEntry {
                    path: RepoPath::new(prefix.clone()),
                    kind: "NodeFile",
                });
            }
            let (lb, rb) = (blob(left), blob(right));
            if lb != rb {
                out.push(FileDelta {
                    path: RepoPath::new(prefix.clone()),
                    from: lb,
                    to: rb,
                });
            }
            let (lt, rt) = (tree(left), tree(right));
            if lt.is_some() || rt.is_some() {
                self.diff_trees(lt, rt, prefix, out)?;
            }
            prefix.pop();
        }
        Ok(())
    }

    /// Apply file-level changes (`Some(blob)` to write, `None` to delete) to
    /// `root` and store the new trees. Only the directories on changed paths
    /// are rewritten. Directories left empty are removed, as git would.
    pub(crate) fn update_tree(
        &self,
        root: SnapshotId,
        changes: &BTreeMap<RepoPath, Option<ObjectId>>,
    ) -> Result<SnapshotId> {
        if changes.is_empty() {
            return Ok(root);
        }
        let list: PathChanges<'_> = changes
            .iter()
            .filter(|(path, _)| !path.is_root())
            .map(|(path, blob)| (path.components(), *blob))
            .collect();
        Ok(self
            .update_subtree(Some(root), &list, true)?
            .unwrap_or(self.empty_tree))
    }

    fn update_subtree(
        &self,
        tree: Option<ObjectId>,
        changes: PathChangesRef<'_, '_>,
        is_root: bool,
    ) -> Result<Option<ObjectId>> {
        let mut out = match tree {
            Some(id) => (*self.tree(id)?).clone(),
            None => Tree::default(),
        };
        let mut nested: BTreeMap<&str, PathChanges<'_>> = BTreeMap::new();
        for (components, blob) in changes {
            match components {
                [name] => match blob {
                    Some(id) => {
                        out.entries.insert(name.clone(), TreeEntry::Blob(*id));
                    }
                    None => {
                        if matches!(out.entries.get(name), Some(TreeEntry::Blob(_))) {
                            out.entries.remove(name);
                        }
                    }
                },
                [dir, rest @ ..] => nested.entry(dir.as_str()).or_default().push((rest, *blob)),
                [] => {}
            }
        }
        for (dir, sub) in nested {
            let existing = match out.entries.get(dir) {
                Some(TreeEntry::Tree(id)) => Some(*id),
                _ => None,
            };
            match self.update_subtree(existing, &sub, false)? {
                Some(id) => {
                    out.entries.insert(dir.to_owned(), TreeEntry::Tree(id));
                }
                None => {
                    if existing.is_some() {
                        out.entries.remove(dir);
                    }
                }
            }
        }
        if out.entries.is_empty() && !is_root {
            return Ok(None);
        }
        let id = self.store.put_object(&out)?;
        {
            let mut cache = lock(&self.trees);
            if cache.len() >= MAX_CACHED_TREES {
                cache.clear();
            }
            cache.insert(id, Arc::new(out));
        }
        Ok(Some(id))
    }
}
