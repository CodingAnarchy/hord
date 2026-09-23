//! M3 commands end to end: `ws new` → edit → `status` → `propose` →
//! `submit` → `queue` → `land --local` → `conflicts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const LIB: &str = "\
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

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env_remove("HORD_AGENT_MODEL")
        .output()
        .unwrap_or_else(|err| panic!("run hord {args:?}: {err}"))
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut args = args.to_vec();
    args.push("--json");
    let text = ok(dir, &args);
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{args:?}: {err}\n{text}"))
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repo with `src/lib.rs`, imported into a fresh hord repo.
fn setup() -> (TempDir, TempDir) {
    let source = TempDir::new("hord-m3-git");
    fs::create_dir_all(source.0.join("src")).unwrap();
    fs::write(source.0.join("src/lib.rs"), LIB).unwrap();
    fs::write(source.0.join("README.md"), "# fixture\n").unwrap();
    git(&source.0, &["init", "-q", "-b", "main"]);
    git(&source.0, &["add", "."]);
    git(&source.0, &["commit", "-q", "-m", "fixture"]);
    let repo = TempDir::new("hord-m3-repo");
    ok(&repo.0, &["init", "--from-git", source.0.to_str().unwrap()]);
    (source, repo)
}

/// `hord ws new`, returning (id, checkout path).
fn ws_new(repo: &Path) -> (String, PathBuf) {
    let v = json(repo, &["ws", "new"]);
    (
        v["id"].as_str().unwrap().to_owned(),
        PathBuf::from(v["materialization"].as_str().unwrap()),
    )
}

fn edit(checkout: &Path, from: &str, to: &str) {
    let lib = checkout.join("src/lib.rs");
    let text = fs::read_to_string(&lib).unwrap();
    assert!(text.contains(from));
    fs::write(&lib, text.replacen(from, to, 1)).unwrap();
}

fn intent(repo: &Path, name: &str, summary: &str, extra: &str) -> String {
    let path = repo.join(format!("{name}.md"));
    fs::write(
        &path,
        format!("---\nsummary: {summary}\nrefs:\n  - issue: 7\nacceptance:\n  - test: it_works\n{extra}---\n\nWhy: {summary}.\n"),
    )
    .unwrap();
    path.to_str().unwrap().to_owned()
}

fn propose(repo: &Path, ws: &str, intent_file: &str) -> String {
    let v = json(repo, &["propose", "-w", ws, "--intent", intent_file]);
    v["change"].as_str().unwrap().to_owned()
}

#[test]
fn ws_new_checks_out_the_base() {
    let (_source, repo) = setup();
    let (_, checkout) = ws_new(&repo.0);
    assert_eq!(
        fs::read_to_string(checkout.join("src/lib.rs")).unwrap(),
        LIB
    );
}

#[test]
fn propose_submit_land_round_trip() {
    let (_source, repo) = setup();
    let dir = &repo.0;
    let (ws, checkout) = ws_new(dir);

    let status = json(dir, &["status", "-w", &ws]);
    assert_eq!(status["reads"], "unobserved");
    assert_eq!(status["ops"], serde_json::json!([]));

    edit(&checkout, "    2\n", "    20\n");
    let status = json(dir, &["status", "-w", &ws]);
    assert_eq!(status["reads"], "unobserved");
    assert_eq!(
        status["write_set"].as_array().unwrap().len(),
        1,
        "{status:#}"
    );
    let ops = status["ops"].as_array().unwrap();
    // ADR 0015: a parsed edit is structural ops, not a Blob.
    assert!(ops.iter().all(|op| op["op"] != "blob"), "{ops:?}");
    assert!(ops.iter().any(|op| op["op"] == "replace"), "{ops:?}");
    let text = ok(dir, &["status", "-w", &ws]);
    assert!(text.contains("reads: unobserved"), "{text}");
    assert!(text.contains("replace beta (src/lib.rs)"), "{text}");

    let file = intent(dir, "beta", "beta returns 20", "reads:\n  - delta\n");
    let text = ok(dir, &["propose", "-w", &ws, "--intent", &file]);
    let change = text.trim().to_owned();
    assert_eq!(change.len(), 64, "{text}");

    let submitted = json(dir, &["submit", &change]);
    assert_eq!(submitted["status"], "queued");
    let queue = json(dir, &["queue"]);
    assert_eq!(queue[0]["change"], change.as_str());
    assert_eq!(queue[0]["summary"], "beta returns 20");
    assert_eq!(json(dir, &["queue", "--mine"]).as_array().unwrap().len(), 1);

    let landed = json(dir, &["land", "--local"]);
    assert_eq!(landed["processed"][0]["status"], "landed");
    assert_eq!(landed["head"], change.as_str());

    let report = json(dir, &["conflicts", &change]);
    assert_eq!(report["clean"], true);
    assert_eq!(report["status"], "landed");

    // A new workspace sees the landed edit.
    let (_, after) = ws_new(dir);
    assert!(
        fs::read_to_string(after.join("src/lib.rs"))
            .unwrap()
            .contains("    20\n")
    );
    let log = json(dir, &["log"]);
    assert!(log.to_string().contains(&change), "{log}");
}

#[test]
fn land_local_with_a_change_submits_it() {
    let (_source, repo) = setup();
    let dir = &repo.0;
    let (a, ca) = ws_new(dir);
    let (b, cb) = ws_new(dir);
    edit(&ca, "    2\n", "    20\n");
    edit(&cb, "    4\n", "    40\n");
    let first = propose(dir, &a, &intent(dir, "a", "beta", ""));
    let second = propose(dir, &b, &intent(dir, "b", "delta", ""));
    json(dir, &["submit", &first]);
    let out = json(dir, &["land", "--local", &second]);
    assert_eq!(out["processed"].as_array().unwrap().len(), 2);
    assert_eq!(out["change"]["status"], "landed");
    // Rebased onto the first: lands under a new id with both edits.
    let landed = out["change"]["landed"].as_str().unwrap();
    assert_ne!(landed, second);
    assert_eq!(out["head"], landed);
    let (_, after) = ws_new(dir);
    let lib = fs::read_to_string(after.join("src/lib.rs")).unwrap();
    assert!(
        lib.contains("    20\n") && lib.contains("    40\n"),
        "{lib}"
    );
    assert!(
        !run(dir, &["land", &second]).status.success(),
        "--local is required"
    );
}

#[test]
fn a_conflicting_pair_is_parked_and_explained() {
    let (_source, repo) = setup();
    let dir = &repo.0;
    let (a, ca) = ws_new(dir);
    let (b, cb) = ws_new(dir);
    edit(&ca, "alpha() + 1", "alpha() + 2");
    edit(&cb, "alpha() + 1", "alpha() + 3");
    let first = propose(dir, &a, &intent(dir, "a", "gamma plus two", ""));
    let second = propose(dir, &b, &intent(dir, "b", "gamma plus three", ""));
    json(dir, &["submit", &first]);
    json(dir, &["submit", &second]);
    let out = json(dir, &["land", "--local"]);
    assert_eq!(out["processed"][0]["status"], "landed");
    assert_eq!(out["processed"][1]["status"], "conflicted");
    assert_eq!(out["processed"][1]["hard"], true);

    let report = json(dir, &["conflicts", &second]);
    assert_eq!(report["status"], "conflicted");
    assert_eq!(report["clean"], false);
    let ww = report["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "write-write")
        .expect("write-write");
    assert_eq!(ww["landed"], first.as_str());
    assert_eq!(ww["landed_summary"], "gamma plus two");
    assert!(
        ww["nodes"][0]["name"].as_str().unwrap().ends_with("gamma"),
        "{ww:#}"
    );
    assert_eq!(ww["nodes"][0]["path"], "src/lib.rs");
    assert_eq!(report["merge"][0]["severity"], "hard");

    let text = ok(dir, &["conflicts", &second]);
    assert!(text.contains("write-write with"), "{text}");
    assert!(text.contains("gamma plus two"), "{text}");
    assert!(text.contains("gamma (src/lib.rs)"), "{text}");
    assert!(text.contains("merge hard src/lib.rs"), "{text}");
    assert!(text.contains("needs replay"), "{text}");
}

#[test]
fn read_write_through_references_is_explained() {
    let (_source, repo) = setup();
    let dir = &repo.0;
    let (a, ca) = ws_new(dir);
    let (b, cb) = ws_new(dir);
    // a edits alpha; b edits gamma, which calls alpha.
    edit(&ca, "    1\n", "    10\n");
    edit(&cb, "alpha() + 1", "alpha() + 5");
    let first = propose(dir, &a, &intent(dir, "a", "alpha ten", ""));
    let second = propose(dir, &b, &intent(dir, "b", "gamma five", ""));
    // Before landing, `conflicts` checks against head: nothing landed yet.
    assert_eq!(json(dir, &["conflicts", &second])["clean"], true);
    json(dir, &["submit", &first]);
    json(dir, &["submit", &second]);
    json(dir, &["land", "--local"]);
    let report = json(dir, &["conflicts", &second]);
    assert_eq!(report["status"], "landed");
    let rw = &report["conflicts"][0];
    assert_eq!(rw["kind"], "read-write");
    assert!(
        rw["nodes"][0]["name"].as_str().unwrap().ends_with("alpha"),
        "{rw:#}"
    );
    let text = ok(dir, &["conflicts", &second]);
    assert!(
        text.contains("landed, flagged for re-verification"),
        "{text}"
    );
}

#[test]
fn bad_intent_files_and_ids_fail_with_json_errors() {
    let (_source, repo) = setup();
    let dir = &repo.0;
    let (ws, checkout) = ws_new(dir);
    edit(&checkout, "    2\n", "    20\n");
    let bad = dir.join("bad.md");
    fs::write(&bad, "no front matter\n").unwrap();
    let out = run(
        dir,
        &[
            "propose",
            "-w",
            &ws,
            "--intent",
            bad.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("front matter"));
    let unknown = intent(dir, "u", "x", "reads:\n  - no_such_function\n");
    assert!(
        !run(dir, &["propose", "-w", &ws, "--intent", &unknown])
            .status
            .success()
    );
    assert!(!run(dir, &["submit", "nothex"]).status.success());
}

#[test]
fn ws_materialize_modes_rm_gc_and_paranoid_status() {
    let (_source, repo) = setup();
    let dir = &repo.0;
    let cloned = json(dir, &["ws", "new"]);
    let copied = json(dir, &["ws", "new", "--materialize", "copy"]);
    assert_eq!(copied["materialize"], "copy");
    if cfg!(target_vendor = "apple") {
        assert_eq!(cloned["materialize"], "clone");
    }
    let id = copied["id"].as_str().unwrap();
    let checkout = PathBuf::from(copied["materialization"].as_str().unwrap());
    edit(&checkout, "    2\n", "    20\n");
    let status = json(dir, &["status", "-w", id, "--paranoid"]);
    assert_eq!(
        status["write_set"].as_array().unwrap().len(),
        1,
        "{status:#}"
    );

    // Land an edit so a new base exists, then retire both old workspaces.
    let change = propose(dir, id, &intent(dir, "p", "beta twenty", ""));
    json(dir, &["land", "--local", &change]);
    let (_, fresh) = ws_new(dir);
    assert!(
        fs::read_to_string(fresh.join("src/lib.rs"))
            .unwrap()
            .contains("    20\n")
    );
    json(dir, &["ws", "rm", id]);
    json(dir, &["ws", "rm", cloned["id"].as_str().unwrap()]);
    assert!(!checkout.exists());
    let gc = json(dir, &["ws", "gc"]);
    assert_eq!(
        gc["removed_pristine"].as_array().unwrap().len(),
        1,
        "{gc:#}"
    );
    assert!(
        !run(dir, &["ws", "rm", id]).status.success(),
        "already removed"
    );
}
