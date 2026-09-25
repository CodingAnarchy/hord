//! M3 commands end to end: `ws new` → edit → `status` → `propose` →
//! `submit` → `queue` → `land --local` → `conflicts`.
//!
//! Single-user mode: `--no-daemon`, so a submitted change stays queued until
//! `land --local` (a daemon's lander lands it at once; see `daemon.rs`).
//! `--json` is the protobuf JSON mapping (ADR 0024): lowerCamelCase keys,
//! enums by value name.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

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

const QUEUED: &str = "QUEUE_STATUS_QUEUED";
const LANDED: &str = "QUEUE_STATUS_LANDED";
const CONFLICTED: &str = "QUEUE_STATUS_CONFLICTED";

/// Ids of `hord log --json`'s changes, oldest first.
fn log_ids(value: &serde_json::Value) -> TestResult<Vec<String>> {
    let ids = value["changes"]
        .as_array()
        .ok_or_else(|| format!("missing changes in {value}"))?
        .iter()
        .map(|c| {
            c["change"]
                .as_str()
                .map(str::to_owned)
                .ok_or("change id in log entry")
        })
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> TestResult<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        common::drop_tree(&self.0);
    }
}

fn run(dir: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_NO_DAEMON", "1")
        .env_remove("HORD_AGENT_MODEL")
        .output()
        .map_err(|err| format!("run hord {args:?}: {err}"))?)
}

fn ok(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = run(dir, args)?;
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

fn json(dir: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let text = ok(dir, &args)?;
    Ok(serde_json::from_str(&text).map_err(|err| format!("{args:?}: {err}\n{text}"))?)
}

fn git(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()?;
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// A git repo with `src/lib.rs`, imported into a fresh hord repo.
fn setup() -> TestResult<(TempDir, TempDir)> {
    let source = TempDir::new("hord-m3-git")?;
    fs::create_dir_all(source.0.join("src"))?;
    fs::write(source.0.join("src/lib.rs"), LIB)?;
    fs::write(source.0.join("README.md"), "# fixture\n")?;
    git(&source.0, &["init", "-q", "-b", "main"])?;
    git(&source.0, &["add", "."])?;
    git(&source.0, &["commit", "-q", "-m", "fixture"])?;
    let repo = TempDir::new("hord-m3-repo")?;
    let from = source.0.to_str().ok_or("UTF-8 source path")?;
    ok(&repo.0, &["init", "--from-git", from])?;
    Ok((source, repo))
}

/// `hord ws new`, returning (id, checkout path).
fn ws_new(repo: &Path) -> TestResult<(String, PathBuf)> {
    let v = json(repo, &["ws", "new"])?;
    Ok((
        v["id"].as_str().ok_or("workspace id")?.to_owned(),
        PathBuf::from(
            v["materialization"]
                .as_str()
                .ok_or("workspace materialization")?,
        ),
    ))
}

fn edit(checkout: &Path, from: &str, to: &str) -> TestResult {
    let lib = checkout.join("src/lib.rs");
    let text = fs::read_to_string(&lib)?;
    assert!(text.contains(from));
    fs::write(&lib, text.replacen(from, to, 1))?;
    Ok(())
}

fn intent(repo: &Path, name: &str, summary: &str, extra: &str) -> TestResult<String> {
    let path = repo.join(format!("{name}.md"));
    fs::write(
        &path,
        format!(
            "---\nsummary: {summary}\nrefs:\n  - issue: 7\nacceptance:\n  - test: it_works\n{extra}---\n\nWhy: {summary}.\n"
        ),
    )?;
    Ok(path.to_str().ok_or("UTF-8 intent path")?.to_owned())
}

fn propose(repo: &Path, ws: &str, intent_file: &str) -> TestResult<String> {
    let v = json(repo, &["propose", "-w", ws, "--intent", intent_file])?;
    Ok(v["change"].as_str().ok_or("proposed change id")?.to_owned())
}

#[test]
fn ws_new_checks_out_the_base() -> TestResult {
    let (_source, repo) = setup()?;
    let (_, checkout) = ws_new(&repo.0)?;
    assert_eq!(fs::read_to_string(checkout.join("src/lib.rs"))?, LIB);
    Ok(())
}

#[test]
fn propose_submit_land_round_trip() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (ws, checkout) = ws_new(dir)?;

    let status = json(dir, &["status", "-w", &ws])?;
    assert_eq!(status["reads"], "unobserved");
    assert_eq!(status["ops"], serde_json::json!([]));

    edit(&checkout, "    2\n", "    20\n")?;
    let status = json(dir, &["status", "-w", &ws])?;
    assert_eq!(status["reads"], "unobserved");
    assert_eq!(
        status["writeSet"].as_array().ok_or("writeSet array")?.len(),
        1,
        "{status:#}"
    );
    let ops = status["ops"].as_array().ok_or("ops array")?;
    // ADR 0015: a parsed edit is structural ops, not a Blob.
    assert!(ops.iter().all(|op| op["op"] != "blob"), "{ops:?}");
    assert!(ops.iter().any(|op| op["op"] == "replace"), "{ops:?}");
    let text = ok(dir, &["status", "-w", &ws])?;
    assert!(text.contains("reads: unobserved"), "{text}");
    assert!(text.contains("replace beta (src/lib.rs)"), "{text}");

    let file = intent(dir, "beta", "beta returns 20", "reads:\n  - delta\n")?;
    let text = ok(dir, &["propose", "-w", &ws, "--intent", &file])?;
    let change = text.trim().to_owned();
    assert_eq!(change.len(), 64, "{text}");

    let submitted = json(dir, &["submit", &change])?;
    assert_eq!(submitted["entry"]["status"], QUEUED);
    let queue = json(dir, &["queue"])?;
    assert_eq!(queue["entries"][0]["change"], change.as_str());
    assert_eq!(queue["entries"][0]["summary"], "beta returns 20");
    assert_eq!(
        json(dir, &["queue", "--mine"])?["entries"]
            .as_array()
            .ok_or("queue entries array")?
            .len(),
        1
    );

    let landed = json(dir, &["land", "--local"])?;
    assert_eq!(landed["processed"][0]["status"], LANDED);
    assert_eq!(landed["head"], change.as_str());

    let report = json(dir, &["conflicts", &change])?;
    assert_eq!(report["report"]["clean"], true);
    assert_eq!(report["entry"]["status"], LANDED);

    // A new workspace sees the landed edit.
    let (_, after) = ws_new(dir)?;
    assert!(fs::read_to_string(after.join("src/lib.rs"))?.contains("    20\n"));
    let log = json(dir, &["log"])?;
    assert!(log.to_string().contains(&change), "{log}");
    Ok(())
}

#[test]
fn land_local_with_a_change_submits_it() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (a, ca) = ws_new(dir)?;
    let (b, cb) = ws_new(dir)?;
    edit(&ca, "    2\n", "    20\n")?;
    edit(&cb, "    4\n", "    40\n")?;
    let first = propose(dir, &a, &intent(dir, "a", "beta", "")?)?;
    let second = propose(dir, &b, &intent(dir, "b", "delta", "")?)?;
    json(dir, &["submit", &first])?;
    let out = json(dir, &["land", "--local", &second])?;
    assert_eq!(
        out["processed"].as_array().ok_or("processed array")?.len(),
        2
    );
    assert_eq!(out["change"]["status"], LANDED);
    // Rebased onto the first: lands under a new id with both edits.
    let landed = out["change"]["landed"].as_str().ok_or("landed change id")?;
    assert_ne!(landed, second);
    assert_eq!(out["head"], landed);
    let (_, after) = ws_new(dir)?;
    let lib = fs::read_to_string(after.join("src/lib.rs"))?;
    assert!(
        lib.contains("    20\n") && lib.contains("    40\n"),
        "{lib}"
    );
    assert!(
        !run(dir, &["land", &second])?.status.success(),
        "--local is required"
    );
    Ok(())
}

#[test]
fn a_conflicting_pair_is_parked_and_explained() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (a, ca) = ws_new(dir)?;
    let (b, cb) = ws_new(dir)?;
    edit(&ca, "alpha() + 1", "alpha() + 2")?;
    edit(&cb, "alpha() + 1", "alpha() + 3")?;
    let first = propose(dir, &a, &intent(dir, "a", "gamma plus two", "")?)?;
    let second = propose(dir, &b, &intent(dir, "b", "gamma plus three", "")?)?;
    json(dir, &["submit", &first])?;
    json(dir, &["submit", &second])?;
    let out = json(dir, &["land", "--local"])?;
    assert_eq!(out["processed"][0]["status"], LANDED);
    assert_eq!(out["processed"][1]["status"], CONFLICTED);
    assert_eq!(out["processed"][1]["hard"], true);

    let result = json(dir, &["conflicts", &second])?;
    assert_eq!(result["entry"]["status"], CONFLICTED);
    let report = &result["report"];
    assert_eq!(report["clean"], false);
    let ww = report["conflicts"]
        .as_array()
        .ok_or("conflicts array")?
        .iter()
        .find(|c| c["kind"] == "CONFLICT_KIND_WRITE_WRITE")
        .expect("write-write");
    assert_eq!(ww["landed"], first.as_str());
    assert_eq!(ww["landedSummary"], "gamma plus two");
    assert!(
        ww["nodes"][0]["name"]
            .as_str()
            .ok_or("write-write node name")?
            .ends_with("gamma"),
        "{ww:#}"
    );
    assert_eq!(ww["nodes"][0]["path"], "src/lib.rs");
    assert_eq!(report["merge"][0]["severity"], "MERGE_SEVERITY_HARD");

    let text = ok(dir, &["conflicts", &second])?;
    assert!(text.contains("write-write with"), "{text}");
    assert!(text.contains("gamma plus two"), "{text}");
    assert!(text.contains("gamma (src/lib.rs)"), "{text}");
    assert!(text.contains("merge hard src/lib.rs"), "{text}");
    assert!(text.contains("no replay harness ran"), "{text}");
    // Spec §6.4 rung 3: the conflict summary names both intents.
    assert!(text.contains("Parked (theirs)"), "{text}");
    assert!(text.contains("Landed (ours)"), "{text}");
    let summary = &result["entry"]["escalation"]["summary"];
    assert_eq!(
        summary["sides"].as_array().map(Vec::len),
        Some(2),
        "{summary:#}"
    );
    Ok(())
}

/// `hord arbitrate --pick theirs`: the resolution lands with both colliding
/// changes as parents, and the parked change is arbitrated.
#[test]
fn arbitrate_picks_theirs_and_lands_with_both_parents() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (a, ca) = ws_new(dir)?;
    let (b, cb) = ws_new(dir)?;
    edit(&ca, "alpha() + 1", "alpha() + 2")?;
    edit(&cb, "alpha() + 1", "alpha() + 3")?;
    let first = propose(dir, &a, &intent(dir, "a", "gamma plus two", "")?)?;
    let second = propose(dir, &b, &intent(dir, "b", "gamma plus three", "")?)?;
    json(dir, &["submit", &first])?;
    json(dir, &["submit", &second])?;
    json(dir, &["land", "--local"])?;

    let reply = json(dir, &["arbitrate", &second, "--pick", "theirs"])?;
    let resolution = reply["change"].as_str().ok_or("resolution id")?.to_owned();
    let out = json(dir, &["land", "--local"])?;
    assert_eq!(out["head"], resolution.as_str(), "{out:#}");
    let result = json(dir, &["conflicts", &second])?;
    assert_eq!(
        result["entry"]["status"], "QUEUE_STATUS_ARBITRATED",
        "{result:#}"
    );
    assert_eq!(result["entry"]["landed"], resolution.as_str());
    let log = json(dir, &["log"])?;
    let landed = log["changes"]
        .as_array()
        .ok_or("changes")?
        .iter()
        .find(|c| c["change"] == resolution.as_str())
        .ok_or("the resolution is in the log")?;
    assert_eq!(
        landed["parents"],
        serde_json::json!([first, second]),
        "{landed:#}"
    );
    let (_, checkout) = ws_new(dir)?;
    assert!(fs::read_to_string(checkout.join("src/lib.rs"))?.contains("alpha() + 3"));

    // Arbitrated is final; a bad pick is refused.
    assert!(
        !run(dir, &["arbitrate", &second, "--pick", "ours"])?
            .status
            .success()
    );
    assert!(!run(dir, &["arbitrate", &second])?.status.success());
    Ok(())
}

#[test]
fn read_write_through_references_is_explained() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (a, ca) = ws_new(dir)?;
    let (b, cb) = ws_new(dir)?;
    // a edits alpha; b edits gamma, which calls alpha.
    edit(&ca, "    1\n", "    10\n")?;
    edit(&cb, "alpha() + 1", "alpha() + 5")?;
    let first = propose(dir, &a, &intent(dir, "a", "alpha ten", "")?)?;
    let second = propose(dir, &b, &intent(dir, "b", "gamma five", "")?)?;
    // Before landing, `conflicts` checks against head: nothing landed yet.
    assert_eq!(json(dir, &["conflicts", &second])?["report"]["clean"], true);
    json(dir, &["submit", &first])?;
    json(dir, &["submit", &second])?;
    let landed = json(dir, &["land", "--local"])?;
    // No verifier until M4, so the default fails closed (spec §15): the
    // overlap parks instead of landing flagged.
    assert_eq!(landed["head"], first.as_str());
    let result = json(dir, &["conflicts", &second])?;
    assert_eq!(result["entry"]["status"], CONFLICTED);
    let report = &result["report"];
    assert_eq!(report["clean"], false);
    assert!(
        report["verification"]
            .as_str()
            .ok_or("verification message")?
            .contains("read-write"),
        "{report:#}"
    );
    let rw = &report["conflicts"][0];
    assert_eq!(rw["kind"], "CONFLICT_KIND_READ_WRITE");
    assert!(
        rw["nodes"][0]["name"]
            .as_str()
            .ok_or("read-write node name")?
            .ends_with("alpha"),
        "{rw:#}"
    );
    let text = ok(dir, &["conflicts", &second])?;
    assert!(text.contains("verification failed: "), "{text}");
    assert!(text.contains("parked: no replay harness ran"), "{text}");
    Ok(())
}

#[test]
fn bad_intent_files_and_ids_fail_with_json_errors() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (ws, checkout) = ws_new(dir)?;
    edit(&checkout, "    2\n", "    20\n")?;
    let bad = dir.join("bad.md");
    fs::write(&bad, "no front matter\n")?;
    let out = run(
        dir,
        &[
            "propose",
            "-w",
            &ws,
            "--intent",
            bad.to_str().ok_or("UTF-8 intent path")?,
            "--json",
        ],
    )?;
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr)?;
    assert!(
        err["error"]
            .as_str()
            .ok_or("error message")?
            .contains("front matter")
    );
    let unknown = intent(dir, "u", "x", "reads:\n  - no_such_function\n")?;
    assert!(
        !run(dir, &["propose", "-w", &ws, "--intent", &unknown])?
            .status
            .success()
    );
    assert!(!run(dir, &["submit", "nothex"])?.status.success());
    Ok(())
}

#[test]
fn ws_materialize_modes_rm_gc_and_paranoid_status() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let cloned = json(dir, &["ws", "new"])?;
    let copied = json(dir, &["ws", "new", "--materialize", "copy"])?;
    assert_eq!(copied["materialize"], "copy");
    if cfg!(target_vendor = "apple") {
        assert_eq!(cloned["materialize"], "clone");
    }
    let id = copied["id"].as_str().ok_or("workspace id")?;
    let checkout = PathBuf::from(
        copied["materialization"]
            .as_str()
            .ok_or("workspace materialization")?,
    );
    edit(&checkout, "    2\n", "    20\n")?;
    let status = json(dir, &["status", "-w", id, "--paranoid"])?;
    assert_eq!(
        status["writeSet"].as_array().ok_or("writeSet array")?.len(),
        1,
        "{status:#}"
    );

    // Land an edit so a new base exists, then retire both old workspaces.
    let change = propose(dir, id, &intent(dir, "p", "beta twenty", "")?)?;
    json(dir, &["land", "--local", &change])?;
    let (_, fresh) = ws_new(dir)?;
    assert!(fs::read_to_string(fresh.join("src/lib.rs"))?.contains("    20\n"));
    json(dir, &["ws", "rm", id])?;
    json(
        dir,
        &[
            "ws",
            "rm",
            cloned["id"].as_str().ok_or("cloned workspace id")?,
        ],
    )?;
    assert!(!checkout.exists());
    let gc = json(dir, &["ws", "gc"])?;
    assert_eq!(
        gc["removedPristine"]
            .as_array()
            .ok_or("removedPristine array")?
            .len(),
        1,
        "{gc:#}"
    );
    assert!(
        !run(dir, &["ws", "rm", id])?.status.success(),
        "already removed"
    );
    Ok(())
}

/// Two concurrent `Cargo.lock` dependency additions both land under the
/// default verifier: their overlap (hord-store's `dependencies`) is resolved
/// by the lockfile merge, which the fail-closed default exempts (ADR 0013).
#[test]
fn concurrent_lockfile_additions_land_under_the_default_verifier() -> TestResult {
    let lock =
        include_str!("../../hord-lang-rust/testdata/lock/hord-v4.lock").replace("\r\n", "\n");
    const STORE_DEPS: &str = "name = \"hord-store\"\nversion = \"0.0.0\"\ndependencies = [\n";
    let package = |name: &str, sum: char| {
        format!(
            "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{}\"\n\n",
            sum.to_string().repeat(64)
        )
    };
    // a adds `zz-alpha` (sorts last), b adds `aaa-beta` (sorts first); each
    // makes hord-store depend on its package, in Cargo's order.
    let a_lock = format!("{lock}\n{}", package("zz-alpha", 'a').trim_end()).replacen(
        " \"zstd\",\n]",
        " \"zstd\",\n \"zz-alpha\",\n]",
        1,
    ) + "\n";
    let b_lock = lock
        .replacen(
            "[[package]]\n",
            &format!("{}[[package]]\n", package("aaa-beta", 'b')),
            1,
        )
        .replacen(STORE_DEPS, &format!("{STORE_DEPS} \"aaa-beta\",\n"), 1);
    assert!(a_lock.contains(" \"zz-alpha\",\n]") && b_lock.contains(" \"aaa-beta\",\n"));

    let source = TempDir::new("hord-m3-lock-git")?;
    fs::write(source.0.join("Cargo.lock"), &lock)?;
    git(&source.0, &["init", "-q", "-b", "main"])?;
    git(&source.0, &["add", "."])?;
    git(&source.0, &["commit", "-q", "-m", "fixture"])?;
    let repo = TempDir::new("hord-m3-lock-repo")?;
    let dir = &repo.0;
    ok(
        dir,
        &[
            "init",
            "--from-git",
            source.0.to_str().ok_or("UTF-8 source path")?,
        ],
    )?;

    let (a, ca) = ws_new(dir)?;
    let (b, cb) = ws_new(dir)?;
    fs::write(ca.join("Cargo.lock"), &a_lock)?;
    fs::write(cb.join("Cargo.lock"), &b_lock)?;
    let first = propose(dir, &a, &intent(dir, "a", "add zz-alpha", "")?)?;
    let second = propose(dir, &b, &intent(dir, "b", "add aaa-beta", "")?)?;
    json(dir, &["submit", &first])?;
    json(dir, &["submit", &second])?;
    let landed = json(dir, &["land", "--local"])?;
    let statuses: Vec<_> = landed["processed"]
        .as_array()
        .ok_or("processed array")?
        .iter()
        .map(|e| {
            e["status"]
                .as_str()
                .map(str::to_owned)
                .ok_or("entry status")
        })
        .collect::<Result<_, _>>()?;
    assert_eq!(statuses, [LANDED, LANDED], "{landed:#}");

    let result = json(dir, &["conflicts", &second])?;
    assert_eq!(result["entry"]["status"], LANDED);
    let report = &result["report"];
    assert_eq!(
        report["conflicts"][0]["kind"], "CONFLICT_KIND_WRITE_WRITE",
        "{report:#}"
    );
    assert_eq!(report["adapterMerged"][0]["path"], "Cargo.lock");
    assert!(report["verification"].is_null(), "{report:#}");
    let text = ok(dir, &["conflicts", &second])?;
    assert!(text.contains("merged by adapter: Cargo.lock"), "{text}");
    Ok(())
}

/// ADR 0017 (system review item 2): a change landed with `land --local` is
/// visible to the M2 commands. `hord blame` by name and by `path:line`, and
/// `hord log --node`, resolve through the snapshot's own identity.
#[test]
fn blame_and_log_resolve_after_land_local() -> TestResult {
    let (_source, repo) = setup()?;
    let dir = &repo.0;
    let (ws, checkout) = ws_new(dir)?;
    edit(&checkout, "    2\n", "    20\n")?;
    let file = intent(dir, "beta", "beta returns 20", "")?;
    let change = propose(dir, &ws, &file)?;
    json(dir, &["submit", &change])?;
    let landed = json(dir, &["land", "--local"])?;
    assert_eq!(landed["processed"][0]["status"], LANDED);

    let by_name = json(dir, &["blame", "beta"])?;
    let node = by_name["node"].as_str().ok_or("blamed node id")?.to_owned();
    let history = by_name["history"].as_array().ok_or("history array")?;
    assert_eq!(history.len(), 1, "{by_name:#}");
    assert_eq!(history[0]["change"], change.as_str());
    assert_eq!(history[0]["intent"], "beta returns 20");
    assert_eq!(history[0]["actor"], "tester");

    // Line 6 is `    20` inside `beta`.
    let by_line = json(dir, &["blame", "src/lib.rs:6"])?;
    assert_eq!(by_line["node"], node.as_str());
    assert_eq!(by_line["history"], by_name["history"]);

    let log = log_ids(&json(dir, &["log", "--node", "beta"])?)?;
    assert_eq!(log, std::slice::from_ref(&change));
    let by_id = log_ids(&json(dir, &["log", "--node", &node])?)?;
    assert_eq!(by_id, log);
    let by_path = log_ids(&json(dir, &["log", "--path", "src"])?)?;
    assert_eq!(by_path, log);

    // An untouched definition resolves too; it has no landed history.
    let alpha = json(dir, &["blame", "alpha"])?;
    assert_ne!(alpha["node"], node.as_str());
    assert_eq!(alpha["history"], serde_json::json!([]));
    Ok(())
}
