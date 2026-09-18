//! Tiny git fixture: import then export must reproduce every tree SHA.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;
use hord_core::{ChangeRecord, IntentRef};
use hord_git::{MemoryStore, Store, export_change, export_tree, import_git};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hord-git-{prefix}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    repo: gix::Repository,
    /// (commit sha hex, tree sha hex, summary)
    commits: Vec<(String, String, String)>,
}

impl Fixture {
    fn init() -> (TempDir, Self) {
        let dir = TempDir::new("src");
        let repo = gix::init_bare(dir.path()).unwrap();
        (
            dir,
            Self {
                repo,
                commits: Vec::new(),
            },
        )
    }

    fn signature(seconds: i64) -> gix::actor::Signature {
        gix::actor::Signature {
            name: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
            time: gix::date::Time::new(seconds, 0),
        }
    }

    fn write_blob(&self, bytes: &[u8]) -> gix::ObjectId {
        self.repo.write_blob(bytes).unwrap().detach()
    }

    fn write_tree(&self, mut entries: Vec<gix::objs::tree::Entry>) -> gix::ObjectId {
        entries.sort();
        self.repo
            .write_object(&gix::objs::Tree { entries })
            .unwrap()
            .detach()
    }

    fn entry(&self, name: &str, kind: EntryKind, oid: gix::ObjectId) -> gix::objs::tree::Entry {
        gix::objs::tree::Entry {
            mode: kind.into(),
            filename: BString::from(name),
            oid,
        }
    }

    fn commit(
        &mut self,
        tree: gix::ObjectId,
        parents: Vec<gix::ObjectId>,
        summary: &str,
        body: &str,
        seconds: i64,
    ) -> gix::ObjectId {
        let message = if body.is_empty() {
            format!("{summary}\n")
        } else {
            format!("{summary}\n\n{body}\n")
        };
        let author = Self::signature(seconds);
        let commit = gix::objs::Commit {
            tree,
            parents: parents.into_iter().collect(),
            author: author.clone(),
            committer: author,
            encoding: None,
            message: BString::from(message),
            extra_headers: Vec::new(),
        };
        let id = self.repo.write_object(&commit).unwrap().detach();
        self.commits.push((
            id.to_hex().to_string(),
            tree.to_hex().to_string(),
            summary.to_owned(),
        ));
        id
    }

    fn set_head(&self, id: gix::ObjectId) {
        self.repo
            .reference(
                "refs/heads/main",
                id,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .unwrap();
        self.repo
            .reference(
                "HEAD",
                id,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .ok();
    }
}

fn build_history() -> (TempDir, Fixture) {
    let (dir, mut fx) = Fixture::init();

    // Root: README + nested dir
    let readme = fx.write_blob(b"hello hord\n");
    let nested_blob = fx.write_blob(b"in a dir\n");
    let nested = fx.write_tree(vec![fx.entry("note.txt", EntryKind::Blob, nested_blob)]);
    let tree_a = fx.write_tree(vec![
        fx.entry("README", EntryKind::Blob, readme),
        fx.entry("doc", EntryKind::Tree, nested),
    ]);
    let a = fx.commit(tree_a, vec![], "initial", "create the repo", 1_700_000_000);

    // Second commit on main: add executable script
    let script = fx.write_blob(b"#!/bin/sh\necho hi\n");
    let tree_b = fx.write_tree(vec![
        fx.entry("README", EntryKind::Blob, readme),
        fx.entry("doc", EntryKind::Tree, nested),
        fx.entry("run.sh", EntryKind::BlobExecutable, script),
    ]);
    let b = fx.commit(tree_b, vec![a], "add runner", "", 1_700_000_100);

    // Side branch from root: add other.txt
    let other = fx.write_blob(b"side branch\n");
    let tree_c = fx.write_tree(vec![
        fx.entry("README", EntryKind::Blob, readme),
        fx.entry("doc", EntryKind::Tree, nested),
        fx.entry("other.txt", EntryKind::Blob, other),
    ]);
    let c = fx.commit(
        tree_c,
        vec![a],
        "side change",
        "from the root",
        1_700_000_200,
    );

    // Merge: both run.sh and other.txt
    let tree_m = fx.write_tree(vec![
        fx.entry("README", EntryKind::Blob, readme),
        fx.entry("doc", EntryKind::Tree, nested),
        fx.entry("other.txt", EntryKind::Blob, other),
        fx.entry("run.sh", EntryKind::BlobExecutable, script),
    ]);
    let m = fx.commit(
        tree_m,
        vec![b, c],
        "merge side",
        "combine both lines",
        1_700_000_300,
    );
    fx.set_head(m);

    (dir, fx)
}

#[test]
fn import_export_reproduces_every_tree_sha() {
    let (src, fx) = build_history();
    let mut store = MemoryStore::new();
    let head = import_git(&mut store, src.path()).unwrap();

    let log = store.log().unwrap();
    assert_eq!(log.len(), 4, "root + two children + merge");
    assert_eq!(store.head().unwrap(), Some(head));
    assert_eq!(*log.last().unwrap(), head);

    let dest = TempDir::new("dst");
    for change_id in &log {
        let change: ChangeRecord = store.get_object(*change_id).unwrap();
        let git_sha = change
            .intent
            .refs
            .iter()
            .find_map(|r| match r {
                IntentRef::GitCommit { sha } => Some(sha.as_str()),
                _ => None,
            })
            .expect("imported change records the git SHA");
        let expected = fx
            .commits
            .iter()
            .find(|(sha, _, _)| sha == git_sha)
            .expect("known fixture commit");
        let exported = export_tree(&store, change.result, dest.path()).unwrap();
        assert_eq!(
            exported.to_hex(),
            expected.1,
            "tree SHA mismatch for commit {} ({})",
            expected.2,
            git_sha
        );
    }
}

#[test]
fn import_records_intent_actor_and_merge_parents() {
    let (src, _fx) = build_history();
    let mut store = MemoryStore::new();
    import_git(&mut store, src.path()).unwrap();

    let log = store.log().unwrap();
    let mut by_summary = std::collections::HashMap::new();
    for id in &log {
        let change: ChangeRecord = store.get_object(*id).unwrap();
        by_summary.insert(change.intent.summary.clone(), change);
    }

    let initial = &by_summary["initial"];
    assert!(initial.parents.is_empty());
    assert_eq!(initial.intent.body, "create the repo");
    match &initial.provenance.actor {
        hord_core::Actor::Human { id } => {
            assert_eq!(id, "Ada Lovelace <ada@example.com>");
        }
        other => panic!("expected Human, got {other:?}"),
    }
    assert!(initial.evidence.is_empty());
    assert!(initial.read_set.is_empty());
    assert!(initial.write_set.is_empty());
    assert!(!initial.ops.is_empty());

    let merge = &by_summary["merge side"];
    assert_eq!(merge.parents.len(), 2, "merge has two parents");
    assert_eq!(merge.intent.body, "combine both lines");

    assert!(by_summary.contains_key("side change"));
    assert!(by_summary.contains_key("add runner"));
    let side_id = log
        .iter()
        .copied()
        .find(|id| {
            let c: ChangeRecord = store.get_object(*id).unwrap();
            c.intent.summary == "side change"
        })
        .unwrap();
    assert!(merge.parents.contains(&side_id));
}

#[test]
fn export_change_writes_hord_trailers() {
    let (src, _fx) = build_history();
    let mut store = MemoryStore::new();
    let head = import_git(&mut store, src.path()).unwrap();
    let dest = TempDir::new("export-commit");
    let commit_oid = export_change(&store, head, dest.path()).unwrap();

    let repo = gix::open_opts(dest.path(), gix::open::Options::isolated()).unwrap();
    let commit = repo.find_commit(commit_oid.as_gix()).unwrap();
    let raw = commit.message_raw().unwrap();
    let text = raw.to_str().unwrap();
    assert!(text.contains(&format!("Hord-Change: {head}")));
    assert!(text.contains("Hord-Intent: merge side"));
    assert!(text.contains("Hord-Actor: Ada Lovelace <ada@example.com>"));
}

#[test]
fn import_is_idempotent() {
    let (src, _fx) = build_history();
    let mut store = MemoryStore::new();
    let a = import_git(&mut store, src.path()).unwrap();
    let b = import_git(&mut store, src.path()).unwrap();
    assert_eq!(a, b);
    assert_eq!(store.log().unwrap().len(), 4);
}
