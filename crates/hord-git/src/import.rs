//! Walk git history and write Tier 0 snapshots and change records.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use gix::bstr::ByteSlice;
use gix::objs::tree::{EntryKind, EntryMode};
use hord_core::{
    Actor, Blob, ChangeId, ChangeRecord, Intent, IntentRef, ObjectId, Op, Provenance, RepoPath,
    Snapshot, SnapshotId, SnapshotMetadata, Timestamp, Tree, TreeEntry, TreeOpKind,
};

use crate::leaf::{GitLeaf, MODE_BLOB};
use crate::store::Store;
use crate::{Error, GitOid};

/// Deterministic toolchain id for git Tier 0 import (no compiler involved).
fn git_import_toolchain() -> ObjectId {
    ObjectId::of("hord-git-tier0").expect("string encodes")
}

fn git_commit_ref(sha: &str) -> String {
    format!("git/commit/{sha}")
}

/// Import `HEAD` of `git_dir` into `store`.
///
/// Returns the [`ChangeId`] of the imported tip (also stored as head).
pub fn import_git<S: Store>(store: &mut S, git_dir: impl AsRef<Path>) -> Result<ChangeId, Error> {
    import_git_ref(store, git_dir, "HEAD")
}

/// Import the named git ref of `git_dir` into `store`.
///
/// Walks ancestors oldest-first. Each commit becomes a [`Snapshot`] of blobs
/// and trees (no [`hord_core::NodeFile`]) and a [`ChangeRecord`]. Merge commits
/// get multiple `parents`. The git SHA is recorded as
/// [`IntentRef::GitCommit`].
pub fn import_git_ref<S: Store>(
    store: &mut S,
    git_dir: impl AsRef<Path>,
    git_ref: &str,
) -> Result<ChangeId, Error> {
    import_revwalk(store, git_dir, git_ref, None)
}

/// Import up to `max_commits` newest ancestors of `git_ref` (spec §12 tokio window).
///
/// Parents that fall outside the window are omitted from [`ChangeRecord::parents`];
/// the commit's tree is still imported so `export(import)` can check those SHAs.
pub fn import_git_window<S: Store>(
    store: &mut S,
    git_dir: impl AsRef<Path>,
    git_ref: &str,
    max_commits: usize,
) -> Result<ChangeId, Error> {
    import_revwalk(store, git_dir, git_ref, Some(max_commits))
}

fn import_revwalk<S: Store>(
    store: &mut S,
    git_dir: impl AsRef<Path>,
    git_ref: &str,
    max_commits: Option<usize>,
) -> Result<ChangeId, Error> {
    let git_dir = git_dir.as_ref();
    let mut repo = open_repo(git_dir)?;
    repo.object_cache_size_if_unset(64 * 1024 * 1024);

    let tip = repo
        .rev_parse_single(git_ref)
        .map_err(|_| Error::MissingRef(git_ref.to_owned()))?;
    let tip_id = tip.detach();

    let walk = repo.rev_walk([tip_id]).all().map_err(Error::git)?;
    let mut commit_ids = Vec::new();
    for info in walk {
        let info = info.map_err(Error::git)?;
        commit_ids.push(info.id);
        if let Some(max) = max_commits
            && commit_ids.len() >= max
        {
            break;
        }
    }
    if commit_ids.is_empty() {
        return Err(Error::EmptyHistory(git_ref.to_owned()));
    }
    // Date order is not topological (clock skew, merges). Parents must come first.
    commit_ids = topo_oldest_first(&repo, commit_ids)?;

    let empty_tree = store.put_object(&Tree::default())?;
    let mut cache = ImportCache::default();
    let mut last = None;
    let allow_missing_parents = max_commits.is_some();

    for git_id in commit_ids {
        let change = import_commit(
            store,
            &repo,
            git_id,
            empty_tree,
            &mut cache,
            allow_missing_parents,
        )?;
        last = Some(change);
    }

    let last = last.ok_or_else(|| Error::EmptyHistory(git_ref.to_owned()))?;
    store.set_head(last)?;
    Ok(last)
}

/// Order `ids` so every parent that is also in `ids` appears before its children.
fn topo_oldest_first(
    repo: &gix::Repository,
    ids: Vec<gix::ObjectId>,
) -> Result<Vec<gix::ObjectId>, Error> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let set: HashSet<gix::ObjectId> = ids.iter().copied().collect();
    let mut remaining: HashMap<gix::ObjectId, usize> = HashMap::with_capacity(ids.len());
    let mut children: HashMap<gix::ObjectId, Vec<gix::ObjectId>> = HashMap::new();

    for id in &ids {
        let commit = repo.find_commit(*id).map_err(Error::git)?;
        let mut n = 0usize;
        for parent in commit.parent_ids() {
            let parent = parent.detach();
            if set.contains(&parent) {
                n += 1;
                children.entry(parent).or_default().push(*id);
            }
        }
        remaining.insert(*id, n);
    }

    let mut queue: VecDeque<gix::ObjectId> = ids
        .iter()
        .copied()
        .filter(|id| remaining.get(id).copied() == Some(0))
        .collect();
    let mut out = Vec::with_capacity(ids.len());
    while let Some(id) = queue.pop_front() {
        out.push(id);
        if let Some(kids) = children.get(&id) {
            for child in kids {
                if let Some(n) = remaining.get_mut(child) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        queue.push_back(*child);
                    }
                }
            }
        }
    }
    if out.len() != ids.len() {
        return Err(Error::Git(format!(
            "commit graph cycle or missing parent during import ({} of {} ordered)",
            out.len(),
            ids.len()
        )));
    }
    Ok(out)
}

#[derive(Default)]
struct ImportCache {
    trees: HashMap<gix::ObjectId, ObjectId>,
    blobs: HashMap<gix::ObjectId, ObjectId>,
    commits: HashMap<gix::ObjectId, ChangeId>,
}

fn import_commit<S: Store>(
    store: &mut S,
    repo: &gix::Repository,
    git_id: gix::ObjectId,
    empty_tree: ObjectId,
    cache: &mut ImportCache,
    allow_missing_parents: bool,
) -> Result<ChangeId, Error> {
    if let Some(existing) = cache.commits.get(&git_id) {
        return Ok(*existing);
    }
    let sha = GitOid::from_gix(git_id).to_hex();
    if let Some(existing) = store.get_ref(&git_commit_ref(&sha))? {
        cache.commits.insert(git_id, existing);
        return Ok(existing);
    }

    let commit = repo.find_commit(git_id).map_err(Error::git)?;
    let parent_git_ids: Vec<gix::ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();

    let mut parents = Vec::with_capacity(parent_git_ids.len());
    for parent in &parent_git_ids {
        if let Some(id) = cache.commits.get(parent) {
            parents.push(*id);
            continue;
        }
        let parent_sha = GitOid::from_gix(*parent).to_hex();
        match store.get_ref(&git_commit_ref(&parent_sha))? {
            Some(id) => {
                cache.commits.insert(*parent, id);
                parents.push(id);
            }
            None if allow_missing_parents => {}
            None => {
                return Err(Error::Git(format!(
                    "parent {parent_sha} of {sha} was not imported"
                )));
            }
        }
    }

    let git_tree = commit.tree_id().map_err(Error::git)?.detach();
    let result = import_tree(store, repo, git_tree, cache)?;

    let base = if let Some(first_parent) = parent_git_ids.first() {
        let parent_commit = repo.find_commit(*first_parent).map_err(Error::git)?;
        let parent_tree = parent_commit.tree_id().map_err(Error::git)?.detach();
        import_tree(store, repo, parent_tree, cache)?
    } else {
        empty_tree
    };

    let snapshot = Snapshot {
        tree: result,
        metadata: SnapshotMetadata {
            toolchain: Some(git_import_toolchain()),
        },
        index: hord_core::IndexPointers::default(),
    };
    let _ = store.put_object(&snapshot)?;

    let message = commit.message().map_err(Error::git)?;
    let summary = message.summary().to_str_lossy().trim().to_owned();
    let body = message
        .body
        .map(|b| b.to_str_lossy().trim_end().to_owned())
        .unwrap_or_default();

    let author = commit.author().map_err(Error::git)?;
    let name = author.name.to_str_lossy();
    let email = author.email.to_str_lossy();
    let actor_id = format!("{name} <{email}>");
    let created_at = timestamp_from_git(author.seconds());

    let ops = diff_trees(store, &RepoPath::default(), Some(base), Some(result))?;

    let change = ChangeRecord {
        base,
        result,
        parents,
        ops,
        intent: Intent {
            summary,
            body,
            refs: vec![IntentRef::GitCommit { sha: sha.clone() }],
            acceptance: Vec::new(),
        },
        provenance: Provenance {
            actor: Actor::Human { id: actor_id },
            toolchain: git_import_toolchain(),
            created_at,
            session: None,
            parent_intent: None,
        },
        read_set: BTreeSet::new(),
        write_set: BTreeSet::new(),
        identity_deltas: Vec::new(),
        evidence: Vec::new(),
        signature: None,
    };
    let change_id = store.put_object(&change)?;
    store.append_log(change_id)?;
    store.set_ref(&git_commit_ref(&sha), change_id)?;
    cache.commits.insert(git_id, change_id);
    Ok(change_id)
}

fn import_tree<S: Store>(
    store: &mut S,
    repo: &gix::Repository,
    git_id: gix::ObjectId,
    cache: &mut ImportCache,
) -> Result<SnapshotId, Error> {
    if let Some(id) = cache.trees.get(&git_id) {
        return Ok(*id);
    }

    let tree = repo.find_tree(git_id).map_err(Error::git)?;
    let mut entries = BTreeMap::new();

    for entry in tree.iter() {
        let entry = entry.map_err(Error::git)?;
        let name = entry_name(entry.filename())?;
        let oid = entry.oid().to_owned();
        let kind = entry.mode().kind();
        let octal = mode_octal(entry.mode());
        match kind {
            EntryKind::Tree => {
                let child = import_tree(store, repo, oid, cache)?;
                entries.insert(name, TreeEntry::Tree(child));
            }
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                let blob = import_blob(store, repo, oid, cache)?;
                entries.insert(name, TreeEntry::Blob(put_leaf(store, octal, blob)?));
            }
            EntryKind::Commit => {
                // Submodule gitlink: store the target commit SHA as blob bytes.
                let blob = store.put_object(&Blob::new(oid.to_hex().to_string().into_bytes()))?;
                entries.insert(name, TreeEntry::Blob(put_leaf(store, octal, blob)?));
            }
        }
    }

    let hord_tree = Tree { entries };
    let id = store.put_object(&hord_tree)?;
    cache.trees.insert(git_id, id);
    Ok(id)
}

/// Regular `100644` files are stored as a plain [`Blob`]. Any other git mode
/// is a [`GitLeaf`] so chmod-only trees get a distinct [`ObjectId`].
fn put_leaf<S: Store>(store: &mut S, octal: String, blob: ObjectId) -> Result<ObjectId, Error> {
    if octal == MODE_BLOB {
        return Ok(blob);
    }
    store.put_object(&GitLeaf { mode: octal, blob })
}

fn import_blob<S: Store>(
    store: &mut S,
    repo: &gix::Repository,
    git_id: gix::ObjectId,
    cache: &mut ImportCache,
) -> Result<ObjectId, Error> {
    if let Some(id) = cache.blobs.get(&git_id) {
        return Ok(*id);
    }
    let blob = repo.find_blob(git_id).map_err(Error::git)?;
    let id = store.put_object(&Blob::new(blob.data.clone()))?;
    cache.blobs.insert(git_id, id);
    Ok(id)
}

fn mode_octal(mode: EntryMode) -> String {
    let mut buf = [0u8; 6];
    let encoded = mode.as_bytes(&mut buf);
    encoded.to_str().unwrap_or("100644").to_owned()
}

fn entry_name(name: &gix::bstr::BStr) -> Result<String, Error> {
    name.to_str()
        .map(str::to_owned)
        .map_err(|_| Error::PathEncoding(format!("{name:?}")))
}

fn timestamp_from_git(seconds: i64) -> Timestamp {
    let ms = seconds.max(0) as u64 * 1000;
    Timestamp::from_millis(ms)
}

fn diff_trees<S: Store>(
    store: &S,
    path: &RepoPath,
    from: Option<ObjectId>,
    to: Option<ObjectId>,
) -> Result<Vec<Op>, Error> {
    if from == to {
        return Ok(Vec::new());
    }
    let from_tree = match from {
        Some(id) => store.get_object::<Tree>(id)?,
        None => Tree::default(),
    };
    let to_tree = match to {
        Some(id) => store.get_object::<Tree>(id)?,
        None => Tree::default(),
    };

    let mut ops = Vec::new();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(from_tree.entries.keys().map(String::as_str));
    names.extend(to_tree.entries.keys().map(String::as_str));

    for name in names {
        let child_path = join_path(path, name);
        let old = from_tree.entries.get(name);
        let new = to_tree.entries.get(name);
        match (old, new) {
            (None, Some(TreeEntry::Blob(id))) => {
                ops.push(Op::Tree {
                    path: child_path.clone(),
                    kind: TreeOpKind::CreateFile,
                });
                ops.push(Op::Blob {
                    path: child_path,
                    from: None,
                    to: Some(*id),
                });
            }
            (None, Some(TreeEntry::Tree(id))) => {
                ops.push(Op::Tree {
                    path: child_path.clone(),
                    kind: TreeOpKind::CreateDir,
                });
                ops.extend(diff_trees(store, &child_path, None, Some(*id))?);
            }
            (None, Some(TreeEntry::NodeFile(id))) => {
                ops.push(Op::Tree {
                    path: child_path.clone(),
                    kind: TreeOpKind::CreateFile,
                });
                ops.push(Op::Blob {
                    path: child_path,
                    from: None,
                    to: Some(*id),
                });
            }
            (Some(TreeEntry::Blob(old_id)), Some(TreeEntry::Blob(new_id))) => {
                if old_id != new_id {
                    ops.push(Op::Blob {
                        path: child_path,
                        from: Some(*old_id),
                        to: Some(*new_id),
                    });
                }
            }
            (Some(TreeEntry::Tree(old_id)), Some(TreeEntry::Tree(new_id))) => {
                ops.extend(diff_trees(
                    store,
                    &child_path,
                    Some(*old_id),
                    Some(*new_id),
                )?);
            }
            (Some(_), None) => {
                ops.extend(delete_entry(store, &child_path, old.expect("matched"))?);
            }
            (Some(old_e), Some(new_e)) => {
                ops.extend(delete_entry(store, &child_path, old_e)?);
                match new_e {
                    TreeEntry::Blob(id) | TreeEntry::NodeFile(id) => {
                        ops.push(Op::Tree {
                            path: child_path.clone(),
                            kind: TreeOpKind::CreateFile,
                        });
                        ops.push(Op::Blob {
                            path: child_path,
                            from: None,
                            to: Some(*id),
                        });
                    }
                    TreeEntry::Tree(id) => {
                        ops.push(Op::Tree {
                            path: child_path.clone(),
                            kind: TreeOpKind::CreateDir,
                        });
                        ops.extend(diff_trees(store, &child_path, None, Some(*id))?);
                    }
                }
            }
            (None, None) => {}
        }
    }
    Ok(ops)
}

fn delete_entry<S: Store>(store: &S, path: &RepoPath, entry: &TreeEntry) -> Result<Vec<Op>, Error> {
    match entry {
        TreeEntry::Blob(id) | TreeEntry::NodeFile(id) => Ok(vec![
            Op::Blob {
                path: path.clone(),
                from: Some(*id),
                to: None,
            },
            Op::Tree {
                path: path.clone(),
                kind: TreeOpKind::Delete,
            },
        ]),
        TreeEntry::Tree(id) => {
            let mut ops = diff_trees(store, path, Some(*id), None)?;
            ops.push(Op::Tree {
                path: path.clone(),
                kind: TreeOpKind::Delete,
            });
            Ok(ops)
        }
    }
}

fn join_path(parent: &RepoPath, name: &str) -> RepoPath {
    let mut parts = parent.components().to_vec();
    parts.push(name.to_owned());
    RepoPath::new(parts)
}

pub(crate) fn open_repo(path: &Path) -> Result<gix::Repository, Error> {
    match gix::open_opts(path, gix::open::Options::isolated()) {
        Ok(repo) => Ok(repo),
        Err(open_err) => match gix::discover(path) {
            Ok(repo) => Ok(repo),
            Err(_) => Err(Error::git(format!(
                "open {} failed: {open_err}",
                path.display()
            ))),
        },
    }
}
