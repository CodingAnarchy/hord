//! Reading and writing snapshots (spec §3.2, §3.7, ADR 0017).
//!
//! A [`SnapshotId`] names a [`Snapshot`] object: a content root [`Tree`] and
//! an [`IdentityTree`]. Content trees keep the Tier 0 shape that `hord-git`
//! imports: every file is a [`TreeEntry::Blob`]. Parsed structure is
//! recomputed from the bytes (parsing is deterministic), so a structural
//! op's node objects are reproducible from the result blob without storing
//! each CST node. The identity tree holds a [`hord_core::FileIdentity`] for
//! each parsed file whose ids differ from the fresh assignment.

use std::collections::BTreeMap;
use std::sync::Arc;

use hord_core::{
    Blob, Bytes, IdentityEntry, IdentityTree, ObjectId, RepoPath, Snapshot, SnapshotId, Tree,
    TreeEntry,
};

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
/// Cap on cached snapshot and identity tree objects.
const MAX_CACHED_SNAPSHOTS: usize = 16_384;

/// Identity changes of one change: path → its new [`hord_core::FileIdentity`]
/// object, or `None` when the file is gone or takes the fresh assignment.
pub(crate) type IdentityEdits = BTreeMap<RepoPath, Option<ObjectId>>;

/// [`ObjectId`] of `bytes` stored as a [`Blob`], without storing it.
pub(crate) fn blob_object_id(bytes: &[u8]) -> Result<ObjectId> {
    Ok(ObjectId::of(&Blob::new(bytes.to_vec()))?)
}

impl Inner {
    /// The [`Snapshot`] object `id`.
    pub(crate) fn snapshot(&self, id: SnapshotId) -> Result<Arc<Snapshot>> {
        if let Some(snapshot) = lock(&self.snapshots).get(&id) {
            return Ok(Arc::clone(snapshot));
        }
        let snapshot: Snapshot = match self.get_object(id) {
            Ok(snapshot) => snapshot,
            Err(Error::Store(hord_store::Error::Encoding(err))) => {
                return Err(Error::Corrupt {
                    id,
                    reason: format!("not a snapshot: {err}"),
                });
            }
            Err(err) => return Err(err),
        };
        let snapshot = Arc::new(snapshot);
        let mut cache = lock(&self.snapshots);
        if cache.len() >= MAX_CACHED_SNAPSHOTS {
            cache.clear();
        }
        cache.insert(id, Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// The content root [`Tree`] of `snapshot`.
    pub(crate) fn root(&self, snapshot: SnapshotId) -> Result<ObjectId> {
        Ok(self.snapshot(snapshot)?.tree)
    }

    /// The identity tree of `snapshot`. A snapshot without one, or whose
    /// identity tree is not stored, is [`Error::MissingIdentity`]: its
    /// NodeIds are never guessed (ADR 0017).
    pub(crate) fn identity_root(&self, snapshot: SnapshotId) -> Result<ObjectId> {
        self.snapshot(snapshot)?
            .identity()
            .ok_or(Error::MissingIdentity(snapshot))
    }

    fn identity_tree(&self, snapshot: SnapshotId, id: ObjectId) -> Result<Arc<IdentityTree>> {
        if let Some(tree) = lock(&self.identity_trees).get(&id) {
            return Ok(Arc::clone(tree));
        }
        let tree: IdentityTree = match self.get_object(id) {
            Ok(tree) => tree,
            Err(Error::Store(hord_store::Error::MissingObject(_))) => {
                return Err(Error::MissingIdentity(snapshot));
            }
            Err(err) => return Err(err),
        };
        let tree = Arc::new(tree);
        let mut cache = lock(&self.identity_trees);
        if cache.len() >= MAX_CACHED_SNAPSHOTS {
            cache.clear();
        }
        cache.insert(id, Arc::clone(&tree));
        Ok(tree)
    }

    /// The [`hord_core::FileIdentity`] `snapshot` records for `path`.
    /// `None`: the file takes the fresh assignment (or is not parsed).
    pub(crate) fn file_identity(
        &self,
        snapshot: SnapshotId,
        path: &RepoPath,
    ) -> Result<Option<ObjectId>> {
        let Some((last, dirs)) = path.components().split_last() else {
            return Ok(None);
        };
        let mut tree = self.identity_tree(snapshot, self.identity_root(snapshot)?)?;
        for dir in dirs {
            match tree.entries.get(dir) {
                Some(IdentityEntry::Dir(id)) => {
                    let id = *id;
                    tree = self.identity_tree(snapshot, id)?;
                }
                _ => return Ok(None),
            }
        }
        Ok(match tree.entries.get(last) {
            Some(IdentityEntry::File(id)) => Some(*id),
            Some(IdentityEntry::Dir(_)) | None => None,
        })
    }

    /// Store the snapshot of content root `tree` and identity tree `identity`.
    pub(crate) fn put_snapshot(&self, tree: ObjectId, identity: ObjectId) -> Result<SnapshotId> {
        let snapshot = Snapshot::new(tree, identity);
        let id = self.store.put_object(&snapshot)?;
        let mut cache = lock(&self.snapshots);
        if cache.len() >= MAX_CACHED_SNAPSHOTS {
            cache.clear();
        }
        cache.insert(id, Arc::new(snapshot));
        Ok(id)
    }

    /// `base` with file changes (`Some(blob)` to write, `None` to delete)
    /// and identity edits applied, stored. `base` itself when neither
    /// changes anything.
    pub(crate) fn commit_snapshot(
        &self,
        base: SnapshotId,
        files: &BTreeMap<RepoPath, Option<ObjectId>>,
        identity: &IdentityEdits,
    ) -> Result<SnapshotId> {
        let snapshot = self.snapshot(base)?;
        let identity_root = self.identity_root(base)?;
        let tree = self.update_tree(base, files)?;
        let identity = self.update_identity(base, identity_root, identity)?;
        if tree == snapshot.tree && identity == identity_root {
            return Ok(base);
        }
        self.put_snapshot(tree, identity)
    }

    /// Apply identity edits to the identity tree `root` of `snapshot` and
    /// store the new trees. Emptied directories are removed.
    fn update_identity(
        &self,
        snapshot: SnapshotId,
        root: ObjectId,
        edits: &IdentityEdits,
    ) -> Result<ObjectId> {
        if edits.is_empty() {
            return Ok(root);
        }
        let list: PathChanges<'_> = edits
            .iter()
            .filter(|(path, _)| !path.is_root())
            .map(|(path, id)| (path.components(), *id))
            .collect();
        match self.update_identity_subtree(snapshot, Some(root), &list)? {
            Some(id) => Ok(id),
            None => Ok(self.store.put_object(&IdentityTree::default())?),
        }
    }

    fn update_identity_subtree(
        &self,
        snapshot: SnapshotId,
        tree: Option<ObjectId>,
        changes: PathChangesRef<'_, '_>,
    ) -> Result<Option<ObjectId>> {
        let mut out = match tree {
            Some(id) => (*self.identity_tree(snapshot, id)?).clone(),
            None => IdentityTree::default(),
        };
        let mut nested: BTreeMap<&str, PathChanges<'_>> = BTreeMap::new();
        for (components, identity) in changes {
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
            match self.update_identity_subtree(snapshot, existing, &sub)? {
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
        let id = self.store.put_object(&out)?;
        let mut cache = lock(&self.identity_trees);
        if cache.len() >= MAX_CACHED_SNAPSHOTS {
            cache.clear();
        }
        cache.insert(id, Arc::new(out));
        Ok(Some(id))
    }

    pub(crate) fn tree(&self, id: ObjectId) -> Result<Arc<Tree>> {
        if let Some(tree) = lock(&self.trees).get(&id) {
            return Ok(Arc::clone(tree));
        }
        let tree: Arc<Tree> = Arc::new(self.get_object(id)?);
        let mut cache = lock(&self.trees);
        if cache.len() >= MAX_CACHED_TREES {
            cache.clear();
        }
        cache.insert(id, Arc::clone(&tree));
        Ok(tree)
    }

    /// The blob at `path` in `snapshot`. `None` when nothing (or a
    /// directory) is there.
    pub(crate) fn blob_id(
        &self,
        snapshot: SnapshotId,
        path: &RepoPath,
    ) -> Result<Option<ObjectId>> {
        let components = path.components();
        let Some((last, dirs)) = components.split_last() else {
            return Ok(None);
        };
        let mut tree = self.tree(self.root(snapshot)?)?;
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
        let blob: Blob = self.get_object(id)?;
        Ok(blob.bytes)
    }

    pub(crate) fn file_bytes(
        &self,
        snapshot: SnapshotId,
        path: &RepoPath,
    ) -> Result<Option<Bytes>> {
        match self.blob_id(snapshot, path)? {
            Some(id) => Ok(Some(self.blob_bytes(id)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn put_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        Ok(self.store.put_object(&Blob::new(bytes.to_vec()))?)
    }

    /// Every file in `snapshot`, in path order.
    pub(crate) fn list_files(&self, snapshot: SnapshotId) -> Result<Vec<(RepoPath, ObjectId)>> {
        let mut out = Vec::new();
        let mut prefix = Vec::new();
        self.walk_files(self.root(snapshot)?, &mut prefix, &mut out)?;
        Ok(out)
    }

    /// Every file at or below `prefix` in `snapshot`, in path order.
    pub(crate) fn files_under(
        &self,
        snapshot: SnapshotId,
        prefix: &RepoPath,
    ) -> Result<Vec<(RepoPath, ObjectId)>> {
        let mut tree = self.root(snapshot)?;
        let components = prefix.components();
        for (i, name) in components.iter().enumerate() {
            match self.tree(tree)?.entries.get(name) {
                Some(TreeEntry::Tree(id)) => tree = *id,
                Some(TreeEntry::Blob(id)) if i + 1 == components.len() => {
                    return Ok(vec![(prefix.clone(), *id)]);
                }
                _ => return Ok(Vec::new()),
            }
        }
        let mut out = Vec::new();
        self.walk_files(tree, &mut components.to_vec(), &mut out)?;
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
        if from == to {
            return Ok(out);
        }
        let (from, to) = (self.root(from)?, self.root(to)?);
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
    /// the content root of `snapshot` and store the new trees. Returns the
    /// new root [`Tree`]. Only the directories on changed paths are
    /// rewritten. Directories left empty are removed, as git would.
    pub(crate) fn update_tree(
        &self,
        snapshot: SnapshotId,
        changes: &BTreeMap<RepoPath, Option<ObjectId>>,
    ) -> Result<ObjectId> {
        let root = self.root(snapshot)?;
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
