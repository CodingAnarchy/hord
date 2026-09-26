//! Project Hord snapshots to git trees and landed changes to git commits.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

use gix::bstr::BString;
use gix::objs::tree::{EntryKind, EntryMode};
use gix::objs::{Kind as GitKind, WriteTo};
use hord_core::{
    Actor, Blob, ChangeId, ChangeRecord, NodeFile, ObjectId, Snapshot, SnapshotId, Tree, TreeEntry,
};

use crate::import::open_repo;
use crate::leaf::{GitLeaf, parse_mode};
use crate::store::Store;
use crate::{Error, GitOid};

/// Project the snapshot `snapshot_id` into `git_dir` and return the git tree
/// SHA of its content root.
///
/// `snapshot_id` is a [`Snapshot`] id (ADR 0017); the git tree is the
/// projection of [`Snapshot::tree`]. A root [`Tree`] id is accepted too. File
/// modes other than `100644` are stored on the leaf object so chmod-only
/// trees keep distinct ids.
pub fn export_tree<S: Store>(
    store: &S,
    snapshot_id: SnapshotId,
    git_dir: impl AsRef<Path>,
) -> Result<GitOid, Error> {
    let repo = open_or_init(git_dir.as_ref())?;
    let cache = ExportCache::default();
    let root = snapshot_root(store, snapshot_id)?;
    walk_tree(store, root, &Sink::Repo(&repo), &cache).map(GitOid::from_gix)
}

/// The content root of `snapshot`: [`Snapshot::tree`] when the object is a
/// [`Snapshot`], else `snapshot` itself (a root [`Tree`] id). The git bridge
/// maps only content, so identity plays no part (ADR 0017).
pub fn snapshot_root<S: Store>(store: &S, snapshot: SnapshotId) -> Result<ObjectId, Error> {
    let bytes = store.get(snapshot)?;
    Ok(hord_encoding::decode::<Snapshot>(&bytes).map_or(snapshot, |s| s.tree))
}

/// Memoizes git object ids already projected from Hord [`ObjectId`]s.
///
/// Shared blobs and subtrees are hashed once. Interior mutexes let the M0
/// eval walk commits in parallel against one cache.
#[derive(Debug, Default)]
pub struct ExportCache {
    trees: Mutex<HashMap<ObjectId, gix::ObjectId>>,
    leaves: Mutex<HashMap<ObjectId, (EntryMode, gix::ObjectId)>>,
}

fn lock_map<K, V>(mutex: &Mutex<HashMap<K, V>>) -> std::sync::MutexGuard<'_, HashMap<K, V>> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Git tree SHA of a Hord snapshot's content root (see [`export_tree`]),
/// hashed in memory (no destination repository).
///
/// Same bytes as [`export_tree`], so `export(import(repo))` can be checked
/// without writing a second git odb. `hash` must match the source repository
/// (`repo.object_hash()`, SHA-1 for cargo/tokio).
pub fn git_tree_sha<S: Store>(
    store: &S,
    snapshot_id: SnapshotId,
    hash: gix::hash::Kind,
    cache: &ExportCache,
) -> Result<GitOid, Error> {
    let root = snapshot_root(store, snapshot_id)?;
    walk_tree(store, root, &Sink::Hash(hash), cache).map(GitOid::from_gix)
}

/// Where projected git objects go: hashed only, or written to a repository.
/// Both yield the same object ids.
enum Sink<'a> {
    Hash(gix::hash::Kind),
    Repo(&'a gix::Repository),
}

impl Sink<'_> {
    fn blob(&self, bytes: &[u8]) -> Result<gix::ObjectId, Error> {
        match self {
            Self::Hash(hash) => {
                gix::objs::compute_hash(*hash, GitKind::Blob, bytes).map_err(Error::git)
            }
            Self::Repo(repo) => repo
                .write_blob(bytes)
                .map(|id| id.detach())
                .map_err(Error::git),
        }
    }

    fn tree(&self, tree: &gix::objs::Tree) -> Result<gix::ObjectId, Error> {
        match self {
            Self::Hash(hash) => {
                let mut buf = Vec::new();
                tree.write_to(&mut buf).map_err(Error::git)?;
                gix::objs::compute_hash(*hash, GitKind::Tree, &buf).map_err(Error::git)
            }
            Self::Repo(repo) => repo
                .write_object(tree)
                .map(|id| id.detach())
                .map_err(Error::git),
        }
    }
}

fn walk_tree<S: Store>(
    store: &S,
    tree_id: ObjectId,
    sink: &Sink<'_>,
    cache: &ExportCache,
) -> Result<gix::ObjectId, Error> {
    if let Some(oid) = lock_map(&cache.trees).get(&tree_id).copied() {
        return Ok(oid);
    }
    let tree: Tree = store.get_object(tree_id)?;
    let mut entries = Vec::with_capacity(tree.entries.len());
    for (name, entry) in &tree.entries {
        let (mode, oid) = match entry {
            TreeEntry::Tree(id) => (EntryKind::Tree.into(), walk_tree(store, *id, sink, cache)?),
            TreeEntry::Blob(id) | TreeEntry::NodeFile(id) => walk_leaf(store, *id, sink, cache)?,
        };
        entries.push(gix::objs::tree::Entry {
            mode,
            filename: BString::from(name.as_str()),
            oid,
        });
    }
    entries.sort();
    let oid = sink.tree(&gix::objs::Tree { entries })?;
    lock_map(&cache.trees).insert(tree_id, oid);
    Ok(oid)
}

/// A leaf is a plain [`Blob`] (`100644`), a [`GitLeaf`] carrying another
/// mode, or a [`NodeFile`] whose raw bytes are exported.
fn walk_leaf<S: Store>(
    store: &S,
    id: ObjectId,
    sink: &Sink<'_>,
    cache: &ExportCache,
) -> Result<(EntryMode, gix::ObjectId), Error> {
    if let Some(cached) = lock_map(&cache.leaves).get(&id).copied() {
        return Ok(cached);
    }
    // Read once and try each leaf shape against the same bytes.
    let bytes = store.get(id)?;
    let pair = if let Ok(blob) = hord_encoding::decode::<Blob>(&bytes) {
        (EntryKind::Blob.into(), sink.blob(&blob.bytes)?)
    } else if let Ok(leaf) = hord_encoding::decode::<GitLeaf>(&bytes) {
        let mode = parse_mode(&leaf.mode)
            .ok_or_else(|| Error::Git(format!("invalid stored git mode {:?}", leaf.mode)))?;
        let blob: Blob = store.get_object(leaf.blob)?;
        let oid = if mode.kind() == EntryKind::Commit {
            let hex = std::str::from_utf8(&blob.bytes)
                .map_err(|_| Error::Git("gitlink blob is not UTF-8 hex".into()))?;
            gix::ObjectId::from_hex(hex.as_bytes())
                .map_err(|e| Error::Git(format!("invalid gitlink oid {hex:?}: {e}")))?
        } else {
            sink.blob(&blob.bytes)?
        };
        (mode, oid)
    } else {
        let node_file: NodeFile =
            hord_encoding::decode(&bytes).map_err(|source| Error::UnexpectedObject {
                id,
                kind: std::any::type_name::<NodeFile>(),
                source,
            })?;
        let blob: Blob = store.get_object(node_file.raw_hash)?;
        (EntryKind::Blob.into(), sink.blob(&blob.bytes)?)
    };
    lock_map(&cache.leaves).insert(id, pair);
    Ok(pair)
}

/// Export a landed change as a git commit with Hord trailers.
///
/// The commit message ends with:
///
/// ```text
/// Hord-Change: <id>
/// Hord-Intent: <summary>
/// Hord-Actor: <actor>
/// ```
///
/// The commit tree is the projection of `change.result`'s content root. Each parent change is
/// exported first unless `refs/hord/changes/<id>` already names its commit.
/// Ancestors are exported iteratively, so long histories do not grow the stack.
pub fn export_change<S: Store>(
    store: &S,
    change_id: ChangeId,
    git_dir: impl AsRef<Path>,
) -> Result<GitOid, Error> {
    let repo = open_or_init(git_dir.as_ref())?;
    let cache = ExportCache::default();
    export_change_into(store, change_id, &repo, &cache).map(GitOid::from_gix)
}

/// Export `change_id` and any unexported ancestors, parents before children.
///
/// Depth-first with an explicit stack: a linear history is as deep as it is
/// long, which recursion would turn into a stack overflow.
fn export_change_into<S: Store>(
    store: &S,
    change_id: ChangeId,
    repo: &gix::Repository,
    cache: &ExportCache,
) -> Result<gix::ObjectId, Error> {
    let mut exported: HashMap<ChangeId, gix::ObjectId> = HashMap::new();
    // Changes waiting on their parents. Meeting one again as an ancestor means
    // the parent links form a cycle.
    let mut waiting: HashSet<ChangeId> = HashSet::new();
    let mut stack: Vec<(ChangeId, Option<ChangeRecord>)> = vec![(change_id, None)];
    while let Some((id, record)) = stack.pop() {
        if exported.contains_key(&id) {
            continue;
        }
        let change = match record {
            Some(change) => change,
            None => {
                if waiting.contains(&id) {
                    return Err(Error::Git(format!("change {id} is its own ancestor")));
                }
                if let Some(existing) = lookup_exported(repo, id) {
                    exported.insert(id, existing);
                    continue;
                }
                let change: ChangeRecord = store.get_object(id)?;
                let pending: Vec<ChangeId> = change
                    .parents
                    .iter()
                    .copied()
                    .filter(|parent| !exported.contains_key(parent))
                    .collect();
                if !pending.is_empty() {
                    // Revisit this change once its parents are exported.
                    waiting.insert(id);
                    stack.push((id, Some(change)));
                    stack.extend(pending.into_iter().rev().map(|parent| (parent, None)));
                    continue;
                }
                change
            }
        };
        let commit = write_commit(store, id, &change, &exported, repo, cache)?;
        waiting.remove(&id);
        exported.insert(id, commit);
    }
    exported
        .get(&change_id)
        .copied()
        .ok_or_else(|| Error::Git(format!("change {change_id} was not exported")))
}

/// Write one commit whose parents are already in `exported`.
fn write_commit<S: Store>(
    store: &S,
    change_id: ChangeId,
    change: &ChangeRecord,
    exported: &HashMap<ChangeId, gix::ObjectId>,
    repo: &gix::Repository,
    cache: &ExportCache,
) -> Result<gix::ObjectId, Error> {
    let root = snapshot_root(store, change.result)?;
    let tree = walk_tree(store, root, &Sink::Repo(repo), cache)?;
    let parents = change
        .parents
        .iter()
        .map(|parent| {
            exported.get(parent).copied().ok_or_else(|| {
                Error::Git(format!("parent {parent} of {change_id} was not exported"))
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let actor = actor_trailer(&change.provenance.actor);
    let message = format_commit_message(
        &change.intent.summary,
        &change.intent.body,
        change_id,
        &actor,
    );
    let author = git_signature(
        &change.provenance.actor,
        change.provenance.created_at.as_millis(),
    );

    let commit = gix::objs::Commit {
        tree,
        parents: parents.into_iter().collect(),
        author: author.clone(),
        committer: author,
        encoding: None,
        message: BString::from(message),
        extra_headers: Vec::new(),
    };
    let commit_id = repo
        .write_object(&commit)
        .map(|id| id.detach())
        .map_err(Error::git)?;

    let ref_name = format!("refs/hord/changes/{change_id}");
    repo.reference(
        ref_name.as_str(),
        commit_id,
        gix::refs::transaction::PreviousValue::Any,
        "hord export",
    )
    .map_err(Error::git)?;
    Ok(commit_id)
}

pub(crate) fn lookup_exported(
    repo: &gix::Repository,
    change_id: ChangeId,
) -> Option<gix::ObjectId> {
    let name = format!("refs/hord/changes/{change_id}");
    let reference = repo.find_reference(name.as_str()).ok()?;
    let id = reference.id();
    Some(id.detach())
}

/// Build the git commit message with Hord trailers (spec §9).
fn format_commit_message(summary: &str, body: &str, change_id: ChangeId, actor: &str) -> String {
    let mut msg = format!("{summary}\n");
    if !body.is_empty() {
        msg.push('\n');
        msg.push_str(body);
        if !body.ends_with('\n') {
            msg.push('\n');
        }
    }
    let _ = write!(
        msg,
        "\nHord-Change: {change_id}\nHord-Intent: {summary}\nHord-Actor: {actor}\n"
    );
    msg
}

fn actor_trailer(actor: &Actor) -> String {
    match actor {
        Actor::Human { id } => id.clone(),
        Actor::Agent {
            id, model, harness, ..
        } => format!("{id} ({model}, {harness})"),
    }
}

fn git_signature(actor: &Actor, created_at_ms: u64) -> gix::actor::Signature {
    let (name, email) = match actor {
        Actor::Human { id } => parse_git_author(id),
        Actor::Agent { id, .. } => (id.clone(), "agent@hord".to_owned()),
    };
    let seconds = i64::try_from(created_at_ms / 1000).unwrap_or(i64::MAX);
    gix::actor::Signature {
        name: BString::from(name),
        email: BString::from(email),
        time: gix::date::Time::new(seconds, 0),
    }
}

fn parse_git_author(id: &str) -> (String, String) {
    if let Some(start) = id.find('<')
        && let Some(end) = id.rfind('>')
        && end > start
    {
        let name = id[..start].trim().to_owned();
        let email = id[start + 1..end].to_owned();
        if !name.is_empty() && !email.is_empty() {
            return (name, email);
        }
    }
    (id.to_owned(), "unknown@hord".to_owned())
}

pub(crate) fn open_or_init(path: &Path) -> Result<gix::Repository, Error> {
    if let Ok(mut repo) = open_repo(path) {
        repo.object_cache_size_if_unset(4 * 1024 * 1024);
        return Ok(repo);
    }
    std::fs::create_dir_all(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    gix::init_bare(path).map_err(|e| Error::git(format!("init {}: {e}", path.display())))
}
