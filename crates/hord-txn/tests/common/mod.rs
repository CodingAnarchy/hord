//! Shared fixtures for `hord-txn` integration tests.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{Actor, Intent, NodeId, RepoPath};
use hord_txn::{BeginOptions, Repo, RepoOptions, Workspace};

pub struct TempRepo {
    pub path: PathBuf,
    pub repo: Repo,
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-txn-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

pub async fn repo_with(files: &[(&str, &str)], options: RepoOptions) -> TempRepo {
    let path = temp_dir("repo");
    let repo = Repo::create_with(&path, options).await.unwrap();
    let files = files
        .iter()
        .map(|(p, s)| (p.parse::<RepoPath>().unwrap(), s.as_bytes().to_vec()))
        .collect();
    repo.bootstrap(files, intent("bootstrap"), actor("seed"))
        .await
        .unwrap();
    TempRepo { path, repo }
}

pub async fn repo(files: &[(&str, &str)]) -> TempRepo {
    repo_with(files, RepoOptions::default()).await
}

/// [`repo`] with the M3 stub verifier: changes whose sets overlap a landed
/// change still land after a clean rebase (verification stubbed, spec §12
/// M3). The default verifier parks them.
pub async fn stub_repo(files: &[(&str, &str)]) -> TempRepo {
    let options = RepoOptions {
        verifier: Some(std::sync::Arc::new(hord_txn::StubVerifier)),
        ..RepoOptions::default()
    };
    repo_with(files, options).await
}

pub fn actor(id: &str) -> Actor {
    Actor::Agent {
        id: id.into(),
        model: "test".into(),
        model_hash: hord_core::Bytes::default(),
        harness: "hord-txn tests".into(),
    }
}

pub fn intent(summary: &str) -> Intent {
    Intent::from_summary(summary)
}

pub fn path(p: &str) -> RepoPath {
    p.parse().unwrap()
}

pub async fn begin(repo: &Repo, who: &str) -> Workspace {
    repo.begin(BeginOptions::at_head(actor(who))).await.unwrap()
}

/// NodeId of the definition whose name is `name` or ends in `::name`.
pub async fn def(ws: &mut Workspace, file: &str, name: &str) -> NodeId {
    let suffix = format!("::{name}");
    ws.definitions(&path(file))
        .await
        .unwrap()
        .into_iter()
        .find(|d| {
            d.name
                .as_ref()
                .is_some_and(|n| n.as_str() == name || n.as_str().ends_with(&suffix))
        })
        .unwrap_or_else(|| panic!("no definition {name} in {file}"))
        .node
}

/// Replace the whole text of definition `name` in `file`.
pub async fn rewrite(ws: &mut Workspace, file: &str, name: &str, text: &str) -> NodeId {
    let node = def(ws, file, name).await;
    ws.write_definition(&path(file), node, text).await.unwrap();
    node
}

pub const LIB: &str = "\
mod other;

pub fn alpha() -> u32 {
    1
}

pub fn beta() -> u32 {
    2
}

pub fn gamma() -> u32 {
    alpha() + 1
}

pub fn delta() -> u32 {
    4
}
";

pub const OTHER: &str = "\
pub fn one() -> u32 {
    1
}

pub fn two() -> u32 {
    2
}
";

pub const README: &str = "# Fixture\n\nline one\nline two\nline three\nline four\nline five\n";

pub fn fixture() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("README.md", README),
        ("src/lib.rs", LIB),
        ("src/other.rs", OTHER),
    ]
}

/// Write `file` as `content` with `from` replaced by `to` (no read logged).
pub async fn edit(ws: &mut Workspace, file: &str, content: &str, from: &str, to: &str) {
    assert!(content.contains(from), "{from:?} not in {file}");
    ws.write_file(&path(file), content.replacen(from, to, 1))
        .await
        .unwrap();
}

/// Propose, submit, and return the change id.
pub async fn submit(repo: &Repo, ws: &mut Workspace, summary: &str) -> hord_core::ChangeId {
    let proposal = ws.propose(intent(summary)).await.unwrap();
    repo.submit(proposal.change).await.unwrap();
    proposal.change
}

/// hord-store's dependency list header in the `hord-v4.lock` fixture.
pub const LOCK_STORE_DEPS: &str = "name = \"hord-store\"\nversion = \"0.0.0\"\ndependencies = [\n";

/// `(base, a, b)` for `Cargo.lock`: `a` adds `zz-alpha` (sorts last) and `b`
/// adds `aaa-beta` (sorts first); both make hord-store depend on theirs, so
/// both write hord-store's `dependencies` node.
pub fn lock_additions() -> (String, String, String) {
    // Normalized: git may check the fixture out with CRLF (Windows autocrlf).
    let lock =
        include_str!("../../../hord-lang-rust/testdata/lock/hord-v4.lock").replace("\r\n", "\n");
    const STORE_DEPS: &str = LOCK_STORE_DEPS;
    let package = |name: &str, sum: char| {
        format!(
            "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{}\"\n\n",
            sum.to_string().repeat(64)
        )
    };
    // a adds `zz-alpha` (sorts last); b adds `aaa-beta` (sorts first). Both
    // make hord-store depend on theirs.
    let a_lock = format!("{lock}\n{}", package("zz-alpha", 'a').trim_end()).replacen(
        STORE_DEPS,
        &format!("{STORE_DEPS} \"zz-alpha\",\n"),
        1,
    ) + "\n";
    let a_lock = a_lock.replacen(" \"zz-alpha\",\n \"hord-core\",", " \"hord-core\",", 1);
    let a_lock = a_lock.replacen(" \"zstd\",\n]", " \"zstd\",\n \"zz-alpha\",\n]", 1);
    let b_lock = lock
        .replacen(
            "[[package]]\n",
            &format!("{}[[package]]\n", package("aaa-beta", 'b')),
            1,
        )
        .replacen(STORE_DEPS, &format!("{STORE_DEPS} \"aaa-beta\",\n"), 1);
    (lock, a_lock, b_lock)
}

/// The [`hord_core::FileIdentity`] object `snapshot`'s identity tree records
/// for `file`, if any (ADR 0017).
pub fn file_identity(
    store: &hord_store::Store,
    snapshot: hord_core::SnapshotId,
    file: &str,
) -> Option<hord_core::ObjectId> {
    use hord_core::{IdentityEntry, IdentityTree, Snapshot};
    let snapshot: Snapshot = store.get_object(snapshot).unwrap();
    let mut tree: IdentityTree = store.get_object(snapshot.identity().unwrap()).unwrap();
    let path = self::path(file);
    let (last, dirs) = path.components().split_last().unwrap();
    for dir in dirs {
        match tree.entries.get(dir) {
            Some(IdentityEntry::Dir(id)) => tree = store.get_object(*id).unwrap(),
            _ => return None,
        }
    }
    match tree.entries.get(last) {
        Some(IdentityEntry::File(id)) => Some(*id),
        _ => None,
    }
}

/// Delete the loose object `id` from the store at `root` (to simulate a
/// lost object).
pub fn remove_loose_object(root: &std::path::Path, id: hord_core::ObjectId) {
    let hex = id.to_hex();
    let file = root
        .join(".hord")
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::remove_file(&file).unwrap_or_else(|err| panic!("remove {}: {err}", file.display()));
}
