//! Project Hord snapshots to git trees and landed changes to git commits.

use std::collections::HashMap;
use std::path::Path;

use gix::bstr::BString;
use gix::objs::tree::{EntryKind, EntryMode};
use gix::objs::{Kind as GitKind, WriteTo};
use hord_core::{
    Actor, Blob, ChangeId, ChangeRecord, NodeFile, ObjectId, SnapshotId, Tree, TreeEntry,
};

use crate::import::open_repo;
use crate::leaf::{GitLeaf, parse_mode};
use crate::store::Store;
use crate::{Error, GitOid};

/// Project the snapshot root tree `snapshot_id` into `git_dir` and return the
/// git tree SHA.
///
/// `snapshot_id` is the [`ObjectId`] of the root [`Tree`] (spec §3.1), not of
/// the [`hord_core::Snapshot`] wrapper. Exported trees are byte-identical to
/// this projection. File modes other than `100644` are stored on the leaf
/// object so chmod-only trees keep distinct ids.
pub fn export_tree<S: Store>(
    store: &S,
    snapshot_id: SnapshotId,
    git_dir: impl AsRef<Path>,
) -> Result<GitOid, Error> {
    let git_dir = git_dir.as_ref();
    let repo = open_or_init(git_dir)?;
    let oid = export_tree_into(store, snapshot_id, &repo)?;
    Ok(GitOid::from_gix(oid))
}

/// Memoizes git object ids already projected from Hord [`ObjectId`]s.
///
/// Shared blobs and subtrees are hashed once. The M0 eval walks every commit
/// in a history; without this cache each commit re-hashes its whole tree.
#[derive(Clone, Debug, Default)]
pub struct ExportCache {
    trees: HashMap<ObjectId, gix::ObjectId>,
    leaves: HashMap<ObjectId, (EntryMode, gix::ObjectId)>,
}

/// Git tree SHA of a Hord tree, hashed in memory (no destination repository).
///
/// Same bytes as [`export_tree`], so `export(import(repo))` can be checked
/// without writing a second git odb. `hash` must match the source repository
/// (`repo.object_hash()`, SHA-1 for cargo/tokio).
pub fn git_tree_sha<S: Store>(
    store: &S,
    snapshot_id: SnapshotId,
    hash: gix::hash::Kind,
    cache: &mut ExportCache,
) -> Result<GitOid, Error> {
    hash_tree(store, snapshot_id, hash, cache).map(GitOid::from_gix)
}

fn hash_tree<S: Store>(
    store: &S,
    tree_id: ObjectId,
    hash: gix::hash::Kind,
    cache: &mut ExportCache,
) -> Result<gix::ObjectId, Error> {
    if let Some(oid) = cache.trees.get(&tree_id) {
        return Ok(*oid);
    }
    let tree: Tree = store.get_object(tree_id)?;
    let mut entries = Vec::with_capacity(tree.entries.len());
    for (name, entry) in &tree.entries {
        let (mode, oid) = match entry {
            TreeEntry::Tree(id) => (EntryKind::Tree.into(), hash_tree(store, *id, hash, cache)?),
            TreeEntry::Blob(id) => hash_leaf(store, *id, hash, cache)?,
            TreeEntry::NodeFile(id) => {
                let oid = hash_node_file(store, *id, hash, cache)?;
                (EntryKind::Blob.into(), oid)
            }
        };
        entries.push(gix::objs::tree::Entry {
            mode,
            filename: BString::from(name.as_str()),
            oid,
        });
    }
    entries.sort();
    let encoded = encode_tree(entries)?;
    let oid = gix::objs::compute_hash(hash, GitKind::Tree, &encoded).map_err(Error::git)?;
    cache.trees.insert(tree_id, oid);
    Ok(oid)
}

fn hash_leaf<S: Store>(
    store: &S,
    id: ObjectId,
    hash: gix::hash::Kind,
    cache: &mut ExportCache,
) -> Result<(EntryMode, gix::ObjectId), Error> {
    if let Some(cached) = cache.leaves.get(&id) {
        return Ok(*cached);
    }
    let pair = if let Ok(blob) = store.get_object::<Blob>(id) {
        let oid = hash_blob(hash, blob.bytes.as_slice())?;
        (EntryKind::Blob.into(), oid)
    } else {
        let leaf: GitLeaf = store.get_object(id)?;
        let mode = parse_mode(&leaf.mode)
            .ok_or_else(|| Error::Git(format!("invalid stored git mode {:?}", leaf.mode)))?;
        let blob: Blob = store.get_object(leaf.blob)?;
        let oid = if mode.kind() == EntryKind::Commit {
            let hex = std::str::from_utf8(blob.bytes.as_slice())
                .map_err(|_| Error::Git("gitlink blob is not UTF-8 hex".into()))?;
            gix::ObjectId::from_hex(hex.as_bytes())
                .map_err(|e| Error::Git(format!("invalid gitlink oid {hex:?}: {e}")))?
        } else {
            hash_blob(hash, blob.bytes.as_slice())?
        };
        (mode, oid)
    };
    cache.leaves.insert(id, pair);
    Ok(pair)
}

fn hash_node_file<S: Store>(
    store: &S,
    id: ObjectId,
    hash: gix::hash::Kind,
    cache: &mut ExportCache,
) -> Result<gix::ObjectId, Error> {
    if let Some((_, oid)) = cache.leaves.get(&id) {
        return Ok(*oid);
    }
    let bytes = if let Ok(blob) = store.get_object::<Blob>(id) {
        blob.bytes
    } else {
        let node_file: NodeFile = store.get_object(id)?;
        let blob: Blob = store.get_object(node_file.raw_hash)?;
        blob.bytes
    };
    let oid = hash_blob(hash, bytes.as_slice())?;
    cache.leaves.insert(id, (EntryKind::Blob.into(), oid));
    Ok(oid)
}

fn hash_blob(hash: gix::hash::Kind, bytes: &[u8]) -> Result<gix::ObjectId, Error> {
    gix::objs::compute_hash(hash, GitKind::Blob, bytes).map_err(Error::git)
}

fn encode_tree(entries: Vec<gix::objs::tree::Entry>) -> Result<Vec<u8>, Error> {
    let tree = gix::objs::Tree { entries };
    let mut buf = Vec::new();
    tree.write_to(&mut buf)
        .map_err(|e| Error::git(format!("encode git tree: {e}")))?;
    Ok(buf)
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
/// The commit tree is the projection of `change.result`. Parent git commits
/// are resolved from previously exported changes (`refs/hord/changes/<id>`)
/// or from [`hord_core::IntentRef::GitCommit`] on the parent records.
pub fn export_change<S: Store>(
    store: &S,
    change_id: ChangeId,
    git_dir: impl AsRef<Path>,
) -> Result<GitOid, Error> {
    let git_dir = git_dir.as_ref();
    let repo = open_or_init(git_dir)?;
    let oid = export_change_into(store, change_id, &repo)?;
    Ok(GitOid::from_gix(oid))
}

/// Export every landed change in log order. Returns the tip commit SHA.
pub fn export_log<S: Store>(store: &S, git_dir: impl AsRef<Path>) -> Result<GitOid, Error> {
    let git_dir = git_dir.as_ref();
    let repo = open_or_init(git_dir)?;
    let log = store.log()?;
    let mut last = None;
    for change_id in log {
        last = Some(export_change_into(store, change_id, &repo)?);
    }
    last.map(GitOid::from_gix)
        .ok_or_else(|| Error::Git("hord log is empty".into()))
}

fn export_tree_into<S: Store>(
    store: &S,
    tree_id: ObjectId,
    repo: &gix::Repository,
) -> Result<gix::ObjectId, Error> {
    let tree: Tree = store.get_object(tree_id)?;
    let mut entries = Vec::with_capacity(tree.entries.len());

    for (name, entry) in &tree.entries {
        let (mode, oid) = match entry {
            TreeEntry::Tree(id) => (EntryKind::Tree.into(), export_tree_into(store, *id, repo)?),
            TreeEntry::Blob(id) => export_leaf(store, *id, repo)?,
            TreeEntry::NodeFile(id) => {
                let oid = export_node_file(store, *id, repo)?;
                (EntryKind::Blob.into(), oid)
            }
        };
        entries.push(gix::objs::tree::Entry {
            mode,
            filename: BString::from(name.as_str()),
            oid,
        });
    }
    entries.sort();
    repo.write_object(&gix::objs::Tree { entries })
        .map(|id| id.detach())
        .map_err(Error::git)
}

fn export_leaf<S: Store>(
    store: &S,
    id: ObjectId,
    repo: &gix::Repository,
) -> Result<(EntryMode, gix::ObjectId), Error> {
    if let Ok(blob) = store.get_object::<Blob>(id) {
        let oid = repo
            .write_blob(blob.bytes.as_slice())
            .map(|id| id.detach())
            .map_err(Error::git)?;
        return Ok((EntryKind::Blob.into(), oid));
    }
    let leaf: GitLeaf = store.get_object(id)?;
    let mode = parse_mode(&leaf.mode)
        .ok_or_else(|| Error::Git(format!("invalid stored git mode {:?}", leaf.mode)))?;
    let blob: Blob = store.get_object(leaf.blob)?;
    if mode.kind() == EntryKind::Commit {
        let hex = std::str::from_utf8(blob.bytes.as_slice())
            .map_err(|_| Error::Git("gitlink blob is not UTF-8 hex".into()))?;
        let oid = gix::ObjectId::from_hex(hex.as_bytes())
            .map_err(|e| Error::Git(format!("invalid gitlink oid {hex:?}: {e}")))?;
        return Ok((mode, oid));
    }
    let oid = repo
        .write_blob(blob.bytes.as_slice())
        .map(|id| id.detach())
        .map_err(Error::git)?;
    Ok((mode, oid))
}

fn export_node_file<S: Store>(
    store: &S,
    id: ObjectId,
    repo: &gix::Repository,
) -> Result<gix::ObjectId, Error> {
    if let Ok(blob) = store.get_object::<Blob>(id) {
        return repo
            .write_blob(blob.bytes.as_slice())
            .map(|id| id.detach())
            .map_err(Error::git);
    }
    let node_file: NodeFile = store.get_object(id)?;
    let blob: Blob = store.get_object(node_file.raw_hash)?;
    repo.write_blob(blob.bytes.as_slice())
        .map(|id| id.detach())
        .map_err(Error::git)
}

fn export_change_into<S: Store>(
    store: &S,
    change_id: ChangeId,
    repo: &gix::Repository,
) -> Result<gix::ObjectId, Error> {
    if let Some(existing) = lookup_exported(repo, change_id) {
        return Ok(existing);
    }

    let change: ChangeRecord = store.get_object(change_id)?;
    let tree = export_tree_into(store, change.result, repo)?;

    let mut parents = Vec::new();
    for parent in &change.parents {
        parents.push(export_change_into(store, *parent, repo)?);
    }

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

fn lookup_exported(repo: &gix::Repository, change_id: ChangeId) -> Option<gix::ObjectId> {
    let name = format!("refs/hord/changes/{change_id}");
    let reference = repo.find_reference(name.as_str()).ok()?;
    let id = reference.id();
    Some(id.detach())
}

/// Build the git commit message with Hord trailers (spec §9).
pub fn format_commit_message(
    summary: &str,
    body: &str,
    change_id: ChangeId,
    actor: &str,
) -> String {
    let mut msg = String::new();
    msg.push_str(summary);
    msg.push('\n');
    if !body.is_empty() {
        msg.push('\n');
        msg.push_str(body);
        if !body.ends_with('\n') {
            msg.push('\n');
        }
    }
    msg.push('\n');
    msg.push_str(&format!("Hord-Change: {change_id}\n"));
    msg.push_str(&format!("Hord-Intent: {summary}\n"));
    msg.push_str(&format!("Hord-Actor: {actor}\n"));
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
    let seconds = (created_at_ms / 1000) as i64;
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

fn open_or_init(path: &Path) -> Result<gix::Repository, Error> {
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
