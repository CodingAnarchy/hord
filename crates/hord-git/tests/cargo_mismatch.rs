//! Reproduce a known cargo.git tree-SHA mismatch (gitlinks).
//!
//! Requires the bare clone at `~/.cache/hord/corpora/cargo.git`. Skipped if absent.

use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use hord_core::{ChangeRecord, IntentRef};
use hord_git::{MemoryStore, Store, export_tree, import_git_window};

const CARGO_GIT: &str = ".cache/hord/corpora/cargo.git";
const FAILING_GITLINK: &str = "ee1a81a801d61d5bedd71bf26038a5f2f500f9e9";
const FAILING_CHMOD: &str = "24ba7c80661a5bb6a5ac612733335ed4579e6276";

fn cargo_git() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("HOME")?).join(CARGO_GIT);
    path.exists().then_some(path)
}

fn list_tree(repo: &gix::Repository, id: gix::ObjectId) -> Vec<(String, String, String)> {
    let tree = repo.find_tree(id).unwrap();
    let mut out = Vec::new();
    for entry in tree.iter() {
        let entry = entry.unwrap();
        let mode = {
            let mut buf = [0u8; 6];
            entry.mode().as_bytes(&mut buf).to_str().unwrap().to_owned()
        };
        let name = entry.filename().to_str().unwrap().to_owned();
        let oid = entry.oid().to_hex().to_string();
        out.push((mode, name, oid));
    }
    out
}

fn dump_diff(
    orig: &gix::Repository,
    exported: &gix::Repository,
    orig_id: gix::ObjectId,
    exp_id: gix::ObjectId,
) {
    fn rec(
        orig: &gix::Repository,
        exported: &gix::Repository,
        orig_id: gix::ObjectId,
        exp_id: gix::ObjectId,
        path: &str,
    ) {
        let a = list_tree(orig, orig_id);
        let b = list_tree(exported, exp_id);
        eprintln!("--- tree {path} orig={orig_id} exp={exp_id} ---");
        let n = a.len().max(b.len());
        for i in 0..n {
            let left = a.get(i);
            let right = b.get(i);
            if left != right {
                eprintln!("  FIRST DIFF at index {i}:");
                eprintln!("    orig: {left:?}");
                eprintln!("    exp : {right:?}");
                if let (Some((am, an, ao)), Some((bm, bn, bo))) = (left, right)
                    && am == bm
                    && an == bn
                    && am == "40000"
                {
                    let child = if path.is_empty() {
                        an.clone()
                    } else {
                        format!("{path}/{an}")
                    };
                    rec(
                        orig,
                        exported,
                        gix::ObjectId::from_hex(ao.as_bytes()).unwrap(),
                        gix::ObjectId::from_hex(bo.as_bytes()).unwrap(),
                        &child,
                    );
                }
                return;
            }
        }
        eprintln!("  (no entry differences; SHA still differs — encoding?)");
    }
    rec(orig, exported, orig_id, exp_id, "");
}

#[test]
fn cargo_ee1a81a_gitlink_tree_sha() {
    let Some(git_dir) = cargo_git() else {
        eprintln!("skip: {CARGO_GIT} not present");
        return;
    };

    let orig = gix::open_opts(&git_dir, gix::open::Options::isolated()).unwrap();
    round_trip_one(&git_dir, &orig, FAILING_GITLINK);
    round_trip_one(&git_dir, &orig, FAILING_CHMOD);

    // Same store: later imports must not clobber earlier trees' modes.
    let mut store = MemoryStore::new();
    import_git_window(&mut store, &git_dir, FAILING_GITLINK, 1).unwrap();
    import_git_window(&mut store, &git_dir, FAILING_CHMOD, 1).unwrap();
    let dest = tempfile();
    for sha in [FAILING_GITLINK, FAILING_CHMOD] {
        let change_id = store
            .log()
            .unwrap()
            .into_iter()
            .find(|id| {
                let c: ChangeRecord = store.get_object(*id).unwrap();
                c.intent
                    .refs
                    .iter()
                    .any(|r| matches!(r, IntentRef::GitCommit { sha: s } if s == sha))
            })
            .unwrap();
        let change: ChangeRecord = store.get_object(change_id).unwrap();
        let orig_tree = orig
            .rev_parse_single(sha)
            .unwrap()
            .object()
            .unwrap()
            .try_into_commit()
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let exported = export_tree(&store, change.result, dest.path()).unwrap();
        assert_eq!(
            exported.to_hex(),
            orig_tree.to_hex().to_string(),
            "joint import clobbered {sha}"
        );
    }
}

fn round_trip_one(git_dir: &Path, orig: &gix::Repository, sha: &str) {
    let commit = orig
        .rev_parse_single(sha)
        .unwrap()
        .object()
        .unwrap()
        .try_into_commit()
        .unwrap();
    let orig_tree = commit.tree_id().unwrap().detach();

    let mut store = MemoryStore::new();
    let head = import_git_window(&mut store, git_dir, sha, 1).unwrap();
    let change: ChangeRecord = store.get_object(head).unwrap();
    assert!(
        change
            .intent
            .refs
            .iter()
            .any(|r| matches!(r, IntentRef::GitCommit { sha: s } if s == sha)),
        "imported the requested commit {sha}"
    );

    let dest = tempfile();
    let exported = export_tree(&store, change.result, dest.path()).unwrap();
    if exported.as_gix() != orig_tree {
        let exp_repo = gix::open_opts(dest.path(), gix::open::Options::isolated()).unwrap();
        dump_diff(orig, &exp_repo, orig_tree, exported.as_gix());
    }
    assert_eq!(
        exported.to_hex(),
        orig_tree.to_hex().to_string(),
        "export(import({sha})) must match git tree SHA"
    );
}

fn tempfile() -> TempDir {
    TempDir::new("cargo-mismatch")
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hord-git-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
