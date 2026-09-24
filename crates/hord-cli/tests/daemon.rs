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

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

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

fn ok(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = hord(dir, args).output()?;
    assert!(out.status.success(), "hord {args:?}: {}", describe(&out));
    Ok(String::from_utf8(out.stdout)?)
}

fn json(dir: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let text = ok(dir, &args)?;
    serde_json::from_str(&text).map_err(|err| format!("{args:?}: {err}\n{text}").into())
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
    assert!(out.status.success(), "git {args:?}: {}", describe(&out));
    Ok(())
}

/// A git repository with `src/lib.rs`, imported into `repo` (which is also
/// that git repository's working tree, for `git export`).
fn setup() -> TestResult<TempDir> {
    let repo = TempDir::new("hord-daemon")?;
    fs::create_dir_all(repo.0.join("src"))?;
    fs::write(repo.0.join("src/lib.rs"), LIB)?;
    fs::write(
        repo.0.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(repo.0.join(".gitignore"), "target/\n*.md\n")?;
    fs::write(repo.0.join(".hord-policy.toml"), POLICY)?;
    git(&repo.0, &["init", "-q", "-b", "main"])?;
    git(&repo.0, &["add", "."])?;
    git(&repo.0, &["commit", "-q", "-m", "fixture"])?;
    let git_path = repo.0.to_str().ok_or("temp path is UTF-8")?;
    ok(&repo.0, &["init", "--from-git", git_path])?;
    Ok(repo)
}

fn intent(dir: &Path, summary: &str) -> TestResult<String> {
    let path = dir.join(format!("{}.md", summary.replace(' ', "-")));
    fs::write(&path, format!("---\nsummary: {summary}\n---\nWhy.\n"))?;
    Ok(path.to_str().ok_or("intent path is UTF-8")?.to_owned())
}

fn edit(checkout: &Path, from: &str, to: &str) -> TestResult {
    let lib = checkout.join("src/lib.rs");
    let text = fs::read_to_string(&lib)?;
    fs::write(&lib, text.replacen(from, to, 1))?;
    Ok(())
}

/// A string field of a `--json` report.
fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> TestResult<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("{key} is a string: {v:#}").into())
}

/// Whether a process other than the caller holds the store.
fn store_held(dir: &Path) -> bool {
    hord_store::Store::open_with_lock_timeout(dir, Duration::ZERO).is_err()
}

#[test]
fn the_daemon_serves_workspaces_and_lands_on_submit() -> TestResult {
    let repo = setup()?;
    let dir = &repo.0;
    let ws = json(dir, &["ws", "new"])?;
    assert!(
        store_held(dir),
        "ws new started a daemon that owns the store"
    );
    let id = str_field(&ws, "id")?.to_owned();
    let checkout = PathBuf::from(str_field(&ws, "materialization")?);
    edit(&checkout, "    2\n", "    20\n")?;
    let status = json(dir, &["status", "-w", &id])?;
    assert_eq!(status["changes"], true, "{status:#}");
    assert!(ok(dir, &["status", "-w", &id])?.contains("replace beta (src/lib.rs)"));
    let change = ok(
        dir,
        &[
            "propose",
            "-w",
            &id,
            "--intent",
            &intent(dir, "beta twenty")?,
        ],
    )?
    .trim()
    .to_owned();
    assert_eq!(change.len(), 64);
    json(dir, &["submit", &change])?;
    // The daemon's lander lands it without `land --local`; watch exits once
    // it settles.
    let watched = ok(dir, &["watch", "--from", "0", "--change", &change])?;
    assert!(watched.contains("landed"), "{watched}");
    let landed = json(dir, &["land", "--local"])?;
    assert_eq!(landed["head"], change.as_str());
    let queue = json(dir, &["queue"])?;
    assert_eq!(queue["entries"][0]["status"], LANDED);
    let list = json(dir, &["ws", "list"])?;
    assert_eq!(list["current"], id.as_str());
    let blame = json(dir, &["blame", "beta"])?;
    assert_eq!(blame["history"][0]["change"], change.as_str());
    // A command that needs the store stops the daemon, and the next one
    // starts it again.
    json(dir, &["git", "export", "head"])?;
    assert!(!store_held(dir), "git export stopped the daemon");
    json(dir, &["queue"])?;
    assert!(store_held(dir));
    // `--no-daemon` stops it too, and works on the store directly.
    let direct = json(dir, &["--no-daemon", "log"])?;
    assert_eq!(direct["head"], change.as_str());
    Ok(())
}

/// ADR 0021's repro, through the daemon: eight concurrent `ws new` calls
/// all succeed, with no lock wait.
#[test]
fn eight_concurrent_ws_new_through_the_daemon() -> TestResult {
    let repo = setup()?;
    let children = (0..8)
        .map(|_| {
            hord(&repo.0, &["ws", "new", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut ids = HashSet::new();
    for child in children {
        let out = child.wait_with_output()?;
        assert!(out.status.success(), "ws new: {}", describe(&out));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        assert!(ids.insert(str_field(&v, "id")?.to_owned()));
    }
    assert_eq!(ids.len(), 8);
    let list = json(&repo.0, &["ws", "list"])?;
    let workspaces = list["workspaces"]
        .as_array()
        .ok_or("workspaces is an array")?;
    assert_eq!(workspaces.len(), 8);
    Ok(())
}

#[test]
fn an_idle_daemon_exits() -> TestResult {
    let repo = setup()?;
    let out = hord(&repo.0, &["queue"])
        .env("HORD_DAEMON_IDLE_SECS", "1")
        .output()?;
    assert!(out.status.success(), "{}", describe(&out));
    let deadline = Instant::now() + Duration::from_secs(20);
    while store_held(&repo.0) {
        assert!(Instant::now() < deadline, "the daemon never exited");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
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

fn serve(dir: &Path) -> TestResult<Serve> {
    let mut child = hord(dir, &["serve", "--bind", "127.0.0.1:0"])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let stderr = child.stderr.take().ok_or("serve stderr is piped")?;
    // Owned by `Serve` from here, so an early return still kills it.
    let mut serve = Serve {
        child,
        url: String::new(),
    };
    let mut line = String::new();
    BufReader::new(stderr).read_line(&mut line)?;
    serve.url = line
        .trim()
        .strip_prefix("hord serve: ")
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or_else(|| format!("unexpected serve output {line:?}"))?
        .to_owned();
    Ok(serve)
}

#[test]
fn serve_refuses_a_non_loopback_address_without_the_flag() -> TestResult {
    let repo = setup()?;
    let out = hord(&repo.0, &["serve", "--bind", "0.0.0.0:0"]).output()?;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--insecure-bind"),
        "{}",
        describe(&out)
    );
    Ok(())
}

/// A clone works against a remote: `ws new` from the remote's head,
/// `propose` pushes, `submit` queues on the server, whose lander lands it.
#[test]
fn a_clone_works_against_its_default_upstream() -> TestResult {
    let origin = setup()?;
    // The server owns the origin store; stop the origin's own daemon first.
    json(&origin.0, &["git", "export", "head"])?;
    let server = serve(&origin.0)?;

    let clone = TempDir::new("hord-clone")?;
    let dir = &clone.0;
    ok(dir, &["init"])?;
    json(dir, &["remote", "add", "origin", &server.url])?;
    let remotes = json(dir, &["remote", "set-default", "origin"])?;
    assert_eq!(remotes["remotes"][0]["default"], true, "{remotes:#}");
    let ws = json(dir, &["ws", "new"])?;
    let id = str_field(&ws, "id")?.to_owned();
    let checkout = PathBuf::from(str_field(&ws, "materialization")?);
    assert_eq!(fs::read_to_string(checkout.join("src/lib.rs"))?, LIB);
    edit(&checkout, "    1\n", "    10\n")?;
    let proposed = json(
        dir,
        &["propose", "-w", &id, "--intent", &intent(dir, "alpha ten")?],
    )?;
    let pushed = proposed["pushed"]
        .as_u64()
        .ok_or_else(|| format!("pushed is a number: {proposed:#}"))?;
    assert!(pushed > 0, "{proposed:#}");
    let change = str_field(&proposed, "change")?.to_owned();
    json(dir, &["submit", &change])?;
    let watched = ok(dir, &["watch", "--from", "0", "--change", &change])?;
    assert!(watched.contains("landed"), "{watched}");
    let log = json(dir, &["log"])?;
    let ids = log["changes"]
        .as_array()
        .ok_or("changes is an array")?
        .iter()
        .map(|c| str_field(c, "change"))
        .collect::<TestResult<Vec<&str>>>()?;
    assert_eq!(ids.last(), Some(&change.as_str()), "{log:#}");
    // `--remote` names it explicitly, and `<remote>/<ref>` picks a base
    // from it.
    let explicit = json(dir, &["--remote", "origin", "queue"])?;
    assert_eq!(explicit["entries"][0]["status"], LANDED);
    let based = json(dir, &["ws", "new", "--base", "origin/head"])?;
    let based = PathBuf::from(str_field(&based, "materialization")?);
    assert!(fs::read_to_string(based.join("src/lib.rs"))?.contains("    10\n"));
    drop(server);
    Ok(())
}

/// `hord verify` and `hord policy check` run in the daemon: the plan, then
/// cargo in the workspace, evidence the policy then counts, and the lander
/// reuses (ADR 0025).
#[test]
fn verify_and_policy_check_through_the_daemon() -> TestResult {
    if Command::new("cargo").arg("-V").output().is_err() {
        return Ok(());
    }
    let repo = setup()?;
    let dir = &repo.0;
    let ws = json(dir, &["ws", "new"])?;
    let id = str_field(&ws, "id")?.to_owned();
    let checkout = PathBuf::from(str_field(&ws, "materialization")?);
    edit(&checkout, "    2\n", "    20\n")?;
    let out = hord(dir, &["policy", "check", "-w", &id, "--json"]).output()?;
    assert_eq!(
        out.status.code(),
        Some(1),
        "deny exits 1: {}",
        describe(&out)
    );
    let before: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(before["decision"], "deny", "{before:#}");
    let plan = json(dir, &["verify", "-w", &id, "--plan-only"])?;
    assert_eq!(
        plan["requirements"],
        serde_json::json!(["check"]),
        "{plan:#}"
    );
    assert!(
        str_field(&plan["checks"][0], "command")?.starts_with("cargo check"),
        "{plan:#}"
    );
    assert!(plan["passed"].is_null());
    let ran = json(dir, &["verify", "-w", &id])?;
    assert_eq!(ran["passed"], true, "{ran:#}");
    let evidence = ran["evidence"].as_array().ok_or("evidence is an array")?;
    assert_eq!(evidence.len(), 1);
    let again = json(dir, &["verify", "-w", &id, "--plan-only"])?;
    let reused = again["reused"]
        .as_array()
        .ok_or_else(|| format!("reused is an array: {again:#}"))?;
    assert_eq!(reused.len(), 1, "{again:#}");
    let after = json(dir, &["policy", "check", "-w", &id])?;
    assert_eq!(after["decision"], "allow", "{after:#}");
    let change = ok(
        dir,
        &[
            "propose",
            "-w",
            &id,
            "--intent",
            &intent(dir, "beta twenty")?,
        ],
    )?
    .trim()
    .to_owned();
    json(dir, &["submit", &change])?;
    let watched = ok(dir, &["watch", "--from", "0", "--change", &change])?;
    assert!(watched.contains("landed"), "{watched}");
    Ok(())
}
