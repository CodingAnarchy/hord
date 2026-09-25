//! The CLI through its per-repo daemon (ADR 0021, ADR 0024 amendment), and
//! against a true remote (`hord serve`, `hord remote`).

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LIB: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n";
const LANDED: &str = "QUEUE_STATUS_LANDED";
/// Every change needs `cargo check` (ADR 0026).
const POLICY: &str = "[land]\nrequire = [\"check\"]\n";

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
        // A daemon exits once `.hord/` is gone (or idle).
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hord(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hord"));
    cmd.args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_DAEMON_IDLE_SECS", "10")
        .env_remove("HORD_NO_DAEMON")
        .env_remove("HORD_AGENT_MODEL");
    cmd
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = hord(dir, args).output().unwrap();
    assert!(out.status.success(), "hord {args:?}: {}", describe(&out));
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
    assert!(out.status.success(), "git {args:?}: {}", describe(&out));
}

/// A git repository with `src/lib.rs`, imported into `repo` (which is also
/// that git repository's working tree, for `git export`).
fn setup() -> TempDir {
    let repo = TempDir::new("hord-daemon");
    fs::create_dir_all(repo.0.join("src")).unwrap();
    fs::write(repo.0.join("src/lib.rs"), LIB).unwrap();
    fs::write(
        repo.0.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(repo.0.join(".gitignore"), "target/\n*.md\n").unwrap();
    fs::write(repo.0.join(".hord-policy.toml"), POLICY).unwrap();
    git(&repo.0, &["init", "-q", "-b", "main"]);
    git(&repo.0, &["add", "."]);
    git(&repo.0, &["commit", "-q", "-m", "fixture"]);
    ok(&repo.0, &["init", "--from-git", repo.0.to_str().unwrap()]);
    repo
}

fn intent(dir: &Path, summary: &str) -> String {
    let path = dir.join(format!("{}.md", summary.replace(' ', "-")));
    fs::write(&path, format!("---\nsummary: {summary}\n---\nWhy.\n")).unwrap();
    path.to_str().unwrap().to_owned()
}

fn edit(checkout: &Path, from: &str, to: &str) {
    let lib = checkout.join("src/lib.rs");
    let text = fs::read_to_string(&lib).unwrap();
    fs::write(&lib, text.replacen(from, to, 1)).unwrap();
}

/// Whether a process other than the caller holds the store.
fn store_held(dir: &Path) -> bool {
    hord_store::Store::open_with_lock_timeout(dir, Duration::ZERO).is_err()
}

#[test]
fn the_daemon_serves_workspaces_and_lands_on_submit() {
    let repo = setup();
    let dir = &repo.0;
    let ws = json(dir, &["ws", "new"]);
    assert!(
        store_held(dir),
        "ws new started a daemon that owns the store; daemon.log:\n{}",
        fs::read_to_string(dir.join(".hord").join("daemon.log")).unwrap_or_default()
    );
    let id = ws["id"].as_str().unwrap().to_owned();
    let checkout = PathBuf::from(ws["materialization"].as_str().unwrap());
    edit(&checkout, "    2\n", "    20\n");
    let status = json(dir, &["status", "-w", &id]);
    assert_eq!(status["changes"], true, "{status:#}");
    assert!(ok(dir, &["status", "-w", &id]).contains("replace beta (src/lib.rs)"));
    let change = ok(
        dir,
        &[
            "propose",
            "-w",
            &id,
            "--intent",
            &intent(dir, "beta twenty"),
        ],
    )
    .trim()
    .to_owned();
    assert_eq!(change.len(), 64);
    json(dir, &["submit", &change]);
    // The daemon's lander lands it without `land --local`; watch exits once
    // it settles.
    let watched = ok(dir, &["watch", "--from", "0", "--change", &change]);
    assert!(watched.contains("landed"), "{watched}");
    let landed = json(dir, &["land", "--local"]);
    assert_eq!(landed["head"], change.as_str());
    let queue = json(dir, &["queue"]);
    assert_eq!(queue["entries"][0]["status"], LANDED);
    let list = json(dir, &["ws", "list"]);
    assert_eq!(list["current"], id.as_str());
    let blame = json(dir, &["blame", "beta"]);
    assert_eq!(blame["history"][0]["change"], change.as_str());
    // A command that needs the store stops the daemon, and the next one
    // starts it again.
    json(dir, &["git", "export", "head"]);
    assert!(!store_held(dir), "git export stopped the daemon");
    json(dir, &["queue"]);
    assert!(store_held(dir));
    // `--no-daemon` stops it too, and works on the store directly.
    let direct = json(dir, &["--no-daemon", "log"]);
    assert_eq!(direct["head"], change.as_str());
}

/// ADR 0021's repro, through the daemon: eight concurrent `ws new` calls
/// all succeed, with no lock wait.
#[test]
fn eight_concurrent_ws_new_through_the_daemon() {
    let repo = setup();
    let children: Vec<_> = (0..8)
        .map(|_| {
            hord(&repo.0, &["ws", "new", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ids = HashSet::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "ws new: {}", describe(&out));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(ids.insert(v["id"].as_str().unwrap().to_owned()));
    }
    assert_eq!(ids.len(), 8);
    let list = json(&repo.0, &["ws", "list"]);
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 8);
}

#[test]
fn an_idle_daemon_exits() {
    let repo = setup();
    let out = hord(&repo.0, &["queue"])
        .env("HORD_DAEMON_IDLE_SECS", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    let deadline = Instant::now() + Duration::from_secs(20);
    while store_held(&repo.0) {
        assert!(Instant::now() < deadline, "the daemon never exited");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `hord serve` on a loopback port; stopped (killed) on drop.
struct Serve {
    child: Child,
    url: String,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &Path) -> Serve {
    let mut child = hord(dir, &["serve", "--bind", "127.0.0.1:0"])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("hord serve: ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("unexpected serve output {line:?}"))
        .to_owned();
    Serve { child, url }
}

#[test]
fn serve_refuses_a_non_loopback_address_without_the_flag() {
    let repo = setup();
    let out = hord(&repo.0, &["serve", "--bind", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--insecure-bind"),
        "{}",
        describe(&out)
    );
}

/// A clone works against a remote: `ws new` from the remote's head,
/// `propose` pushes, `submit` queues on the server, whose lander lands it.
#[test]
fn a_clone_works_against_its_default_upstream() {
    let origin = setup();
    // The server owns the origin store; stop the origin's own daemon first.
    json(&origin.0, &["git", "export", "head"]);
    let server = serve(&origin.0);

    let clone = TempDir::new("hord-clone");
    let dir = &clone.0;
    ok(dir, &["init"]);
    json(dir, &["remote", "add", "origin", &server.url]);
    let remotes = json(dir, &["remote", "set-default", "origin"]);
    assert_eq!(remotes["remotes"][0]["default"], true, "{remotes:#}");
    let ws = json(dir, &["ws", "new"]);
    let id = ws["id"].as_str().unwrap().to_owned();
    let checkout = PathBuf::from(ws["materialization"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(checkout.join("src/lib.rs")).unwrap(),
        LIB
    );
    edit(&checkout, "    1\n", "    10\n");
    let proposed = json(
        dir,
        &["propose", "-w", &id, "--intent", &intent(dir, "alpha ten")],
    );
    assert!(proposed["pushed"].as_u64().unwrap() > 0, "{proposed:#}");
    let change = proposed["change"].as_str().unwrap().to_owned();
    json(dir, &["submit", &change]);
    let watched = ok(dir, &["watch", "--from", "0", "--change", &change]);
    assert!(watched.contains("landed"), "{watched}");
    let log = json(dir, &["log"]);
    let ids: Vec<&str> = log["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["change"].as_str().unwrap())
        .collect();
    assert_eq!(ids.last(), Some(&change.as_str()), "{log:#}");
    // `--remote` names it explicitly, and `<remote>/<ref>` picks a base
    // from it.
    let explicit = json(dir, &["--remote", "origin", "queue"]);
    assert_eq!(explicit["entries"][0]["status"], LANDED);
    let based = json(dir, &["ws", "new", "--base", "origin/head"]);
    let based = PathBuf::from(based["materialization"].as_str().unwrap());
    assert!(
        fs::read_to_string(based.join("src/lib.rs"))
            .unwrap()
            .contains("    10\n")
    );
    drop(server);
}

/// `hord verify` and `hord policy check` run in the daemon: the plan, then
/// cargo in the workspace, evidence the policy then counts, and the lander
/// reuses (ADR 0025).
#[test]
fn verify_and_policy_check_through_the_daemon() {
    if Command::new("cargo").arg("-V").output().is_err() {
        return;
    }
    let repo = setup();
    let dir = &repo.0;
    let ws = json(dir, &["ws", "new"]);
    let id = ws["id"].as_str().unwrap().to_owned();
    let checkout = PathBuf::from(ws["materialization"].as_str().unwrap());
    edit(&checkout, "    2\n", "    20\n");
    let out = hord(dir, &["policy", "check", "-w", &id, "--json"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "deny exits 1: {}",
        describe(&out)
    );
    let before: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(before["decision"], "deny", "{before:#}");
    let plan = json(dir, &["verify", "-w", &id, "--plan-only"]);
    assert_eq!(
        plan["requirements"],
        serde_json::json!(["check"]),
        "{plan:#}"
    );
    assert!(
        plan["checks"][0]["command"]
            .as_str()
            .unwrap()
            .starts_with("cargo check"),
        "{plan:#}"
    );
    assert!(plan["passed"].is_null());
    let ran = json(dir, &["verify", "-w", &id]);
    assert_eq!(ran["passed"], true, "{ran:#}");
    assert_eq!(ran["evidence"].as_array().unwrap().len(), 1);
    let again = json(dir, &["verify", "-w", &id, "--plan-only"]);
    assert_eq!(again["reused"].as_array().unwrap().len(), 1, "{again:#}");
    let after = json(dir, &["policy", "check", "-w", &id]);
    assert_eq!(after["decision"], "allow", "{after:#}");
    let change = ok(
        dir,
        &[
            "propose",
            "-w",
            &id,
            "--intent",
            &intent(dir, "beta twenty"),
        ],
    )
    .trim()
    .to_owned();
    json(dir, &["submit", &change]);
    let watched = ok(dir, &["watch", "--from", "0", "--change", &change]);
    assert!(watched.contains("landed"), "{watched}");
}
