//! Spec §12 M5 review round-trip through the CLI against `hord serve
//! --auth` (spec §10.5.4): a policy rule requiring `review:human` parks an
//! agent's change; `hord review --as human --approve` with a human token
//! unblocks it and it lands; the review verifies with the reviewer's key
//! and not with another. Along the way: a request without a token is
//! refused, and a token without `review:human` cannot sign that review.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LIB: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n";
/// Editing a function needs a human's review, and nothing else.
const POLICY: &str = "[[rule]]\nname = \"functions need a human\"\n\
    when = { touches_kind = \"function_item\", paths = [\"src/**\"] }\n\
    require = [\"review:human\"]\n";
const LANDED: &str = "QUEUE_STATUS_LANDED";
const PARKED: &str = "QUEUE_STATUS_PARKED";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> TestResult<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        common::clear_stale(&path)?;
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        common::drop_tree(&self.0);
    }
}

/// `hord` in `dir` as the identity whose `~/.hord` is `home`.
fn hord(dir: &Path, home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hord"));
    cmd.args(args)
        .current_dir(dir)
        .env("HORD_HOME", home)
        .env("HORD_ACTOR", "tester")
        .env("HORD_NO_DAEMON", "1")
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

/// Run with `stdin` fed in.
fn run(dir: &Path, home: &Path, args: &[&str], stdin: &str) -> TestResult<Output> {
    let mut child = hord(dir, home, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin is piped")?
        .write_all(stdin.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn json_in(dir: &Path, home: &Path, args: &[&str], stdin: &str) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let out = run(dir, home, &args, stdin)?;
    assert!(out.status.success(), "hord {args:?}: {}", describe(&out));
    let text = String::from_utf8(out.stdout)?;
    serde_json::from_str(&text).map_err(|err| format!("{args:?}: {err}\n{text}").into())
}

fn json(dir: &Path, home: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    json_in(dir, home, args, "")
}

/// Run expecting failure; its stderr.
fn fails(dir: &Path, home: &Path, args: &[&str]) -> TestResult<String> {
    let out = run(dir, home, args, "")?;
    assert!(
        !out.status.success(),
        "hord {args:?} succeeded: {}",
        describe(&out)
    );
    Ok(String::from_utf8(out.stderr)?)
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> TestResult<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("{key} is a string: {v:#}").into())
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

/// `hord serve --auth`; killed on drop.
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

fn serve(dir: &Path, home: &Path, auth: &Path) -> TestResult<Serve> {
    let auth = auth.to_str().ok_or("auth path is UTF-8")?;
    let mut child = hord(
        dir,
        home,
        &["serve", "--bind", "127.0.0.1:0", "--auth", auth],
    )
    .stderr(Stdio::piped())
    .stdout(Stdio::null())
    .spawn()?;
    let stderr = child.stderr.take().ok_or("serve stderr is piped")?;
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

/// The latest queue entry for `change`, polled until `status` or a timeout.
fn wait_for(dir: &Path, home: &Path, change: &str, status: &str) -> TestResult<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let queue = json(dir, home, &["queue"])?;
        let latest = queue["entries"]
            .as_array()
            .ok_or("entries is an array")?
            .iter()
            .rfind(|e| e["change"] == change)
            .cloned();
        if let Some(entry) = latest {
            if entry["status"] == status {
                return Ok(entry);
            }
            let settled = ["QUEUE_STATUS_CONFLICTED", "QUEUE_STATUS_REJECTED"];
            if settled.iter().any(|s| entry["status"] == *s) {
                return Err(format!("{change} settled short of {status}: {entry:#}").into());
            }
        }
        if Instant::now() > deadline {
            return Err(format!("{change} never reached {status}: {queue:#}").into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_human_review_unblocks_a_parked_agent_change() -> TestResult {
    let homes = TempDir::new("hord-auth-homes")?;
    let home = |who: &str| -> TestResult<PathBuf> {
        let dir = homes.0.join(who);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    };
    let (op, ada, eve, bot, nobody) = (
        home("op")?,
        home("ada")?,
        home("eve")?,
        home("bot")?,
        home("nobody")?,
    );

    // The origin: a function-editing policy at head, and a user table.
    let origin = TempDir::new("hord-auth-origin")?;
    fs::create_dir_all(origin.0.join("src"))?;
    fs::write(origin.0.join("src/lib.rs"), LIB)?;
    fs::write(
        origin.0.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(origin.0.join(".hord-policy.toml"), POLICY)?;
    fs::write(origin.0.join(".gitignore"), "target/\n*.md\n")?;
    git(&origin.0, &["init", "-q", "-b", "main"])?;
    git(&origin.0, &["add", "."])?;
    git(&origin.0, &["commit", "-q", "-m", "fixture"])?;
    let git_path = origin.0.to_str().ok_or("temp path is UTF-8")?;
    json(&origin.0, &op, &["init", "--from-git", git_path])?;
    let auth = homes.0.join("auth.toml");
    let auth_arg = auth.to_str().ok_or("auth path is UTF-8")?;
    for (user, scopes) in [
        ("root", &["admin"][..]),
        ("ada", &["read", "propose", "review:human"][..]),
        ("eve", &["read", "propose"][..]),
    ] {
        let mut args = vec![
            "user",
            "add",
            user,
            "--auth-file",
            auth_arg,
            "--password-stdin",
        ];
        for scope in scopes {
            args.extend(["--scope", scope]);
        }
        json_in(&origin.0, &op, &args, &format!("{user}-pw\n"))?;
    }
    let server = serve(&origin.0, &op, &auth)?;

    // A clone of it.
    let clone = TempDir::new("hord-auth-clone")?;
    let dir = &clone.0;
    json(dir, &op, &["init"])?;
    json(dir, &op, &["remote", "add", "origin", &server.url])?;
    json(dir, &op, &["remote", "set-default", "origin"])?;

    // No token: refused.
    let refused = fails(dir, &nobody, &["queue"])?;
    assert!(refused.contains("requires a bearer token"), "{refused}");

    // The operator mints an agent token; the agent logs in with it.
    json_in(
        dir,
        &op,
        &["login", "origin", "--user", "root", "--password-stdin"],
        "root-pw\n",
    )?;
    let key_file = homes.0.join("bot.pem");
    let key_arg = key_file.to_str().ok_or("key path is UTF-8")?;
    let minted = json(
        dir,
        &op,
        &[
            "token",
            "mint",
            "--agent",
            "bot-1",
            "--model",
            "m1",
            "--harness",
            "h1",
            "--scope",
            "read",
            "--scope",
            "propose",
            "--key-out",
            key_arg,
        ],
    )?;
    let token = str_field(&minted, "token")?.to_owned();
    let bot_key = str_field(&minted, "keyId")?.to_owned();
    assert_eq!(minted["privateKeyPem"], "", "{minted:#}");
    json(
        dir,
        &bot,
        &["login", "origin", "--token", &token, "--key-file", key_arg],
    )?;

    // The agent edits a function, proposes, and submits: parked for review.
    let ws = json(dir, &bot, &["ws", "new"])?;
    let id = str_field(&ws, "id")?.to_owned();
    let checkout = PathBuf::from(str_field(&ws, "materialization")?);
    let lib = checkout.join("src/lib.rs");
    fs::write(
        &lib,
        fs::read_to_string(&lib)?.replacen("    1\n", "    10\n", 1),
    )?;
    let intent = dir.join("intent.md");
    fs::write(&intent, "---\nsummary: alpha ten\n---\nWhy.\n")?;
    let intent = intent.to_str().ok_or("intent path is UTF-8")?;
    let proposed = json(dir, &bot, &["propose", "-w", &id, "--intent", intent])?;
    let change = str_field(&proposed, "change")?.to_owned();
    json(dir, &bot, &["submit", &change])?;
    let parked = wait_for(dir, &bot, &change, PARKED)?;
    assert_eq!(parked["actor"]["agent"]["id"], "bot-1", "{parked:#}");
    assert!(
        parked["reason"]
            .as_str()
            .is_some_and(|r| r.contains("review:human")),
        "{parked:#}"
    );
    // The agent signed it, and provenance is the token's actor.
    let signed = json(dir, &bot, &["key", "verify", &change, "--key", &bot_key])?;
    assert_eq!(signed["verified"], true, "{signed:#}");
    assert_eq!(signed["actor"]["agent"]["id"], "bot-1", "{signed:#}");

    // A human without `review:human` cannot sign that review.
    json_in(
        dir,
        &eve,
        &["login", "origin", "--user", "eve", "--password-stdin"],
        "eve-pw\n",
    )?;
    let denied = fails(
        dir,
        &eve,
        &[
            "review",
            &change,
            "--as",
            "human",
            "--approve",
            "-m",
            "lgtm",
        ],
    )?;
    assert!(denied.contains("review:human"), "{denied}");
    assert!(denied.contains("permission denied"), "{denied}");

    // Ada can: the change lands.
    let login = json_in(
        dir,
        &ada,
        &["login", "origin", "--user", "ada", "--password-stdin"],
        "ada-pw\n",
    )?;
    let ada_key = str_field(&login, "keyId")?.to_owned();
    let review = json(
        dir,
        &ada,
        &[
            "review",
            &change,
            "--as",
            "human",
            "--approve",
            "-m",
            "alpha is ten now",
        ],
    )?;
    assert_eq!(review["keyId"], ada_key.as_str(), "{review:#}");
    let evidence = str_field(&review, "evidence")?.to_owned();
    let landed = wait_for(dir, &bot, &change, LANDED)?;
    let log = json(dir, &ada, &["log"])?;
    let last = log["changes"]
        .as_array()
        .and_then(|c| c.last())
        .ok_or("the log has changes")?;
    assert_eq!(last["change"], landed["landed"], "{log:#}");

    // The review verifies with Ada's key, and not with another.
    let ok = json(dir, &ada, &["key", "verify", &evidence, "--key", &ada_key])?;
    assert_eq!(ok["verified"], true, "{ok:#}");
    assert_eq!(ok["kind"], "evidence");
    assert_eq!(ok["actor"]["human"]["id"], "ada", "{ok:#}");
    let out = run(
        dir,
        &ada,
        &["key", "verify", &evidence, "--key", &bot_key, "--json"],
        "",
    )?;
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let wrong: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(wrong["verified"], false, "{wrong:#}");
    drop(server);
    Ok(())
}
