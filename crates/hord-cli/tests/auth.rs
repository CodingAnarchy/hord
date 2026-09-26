//! Spec §12 M5 review round-trip through the CLI against `hord serve
//! --auth` (spec §10.5.4): a policy rule requiring `review:human` parks an
//! agent's change; `hord review --as human --approve` with a human token
//! unblocks it and it lands; the review verifies with the reviewer's key
//! and not with another. Along the way: a request without a token is
//! refused, and a token without `review:human` cannot sign that review.

mod common;

use common::TempDir;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hord_api::proto::event::Kind;
use hord_api::{ChangesBackend, proto, wire};
use hord_core::sign::{PublicKey, SigningKey};
use hord_core::{Bytes, ObjectId, Signature};
use hord_remote::RemoteRepo;
use hord_txn::{Arbitration, verify_arbitration};

const LIB: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n";
/// Editing a function needs a human's review, and nothing else.
const POLICY: &str = "[[rule]]\nname = \"functions need a human\"\n\
    when = { touches_kind = \"function_item\", paths = [\"src/**\"] }\n\
    require = [\"review:human\"]\n";
const LANDED: &str = "QUEUE_STATUS_LANDED";
const PARKED: &str = "QUEUE_STATUS_PARKED";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

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

/// A server requiring tokens over an origin whose policy needs
/// `review:human` for function edits; a clone of it; users `root`
/// (admin), `ada` (review:human), and `eve` (no review); and agent `bot-1`
/// logged in with a minted token. Each identity has its own `~/.hord`.
struct World {
    homes: TempDir,
    _origin: TempDir,
    clone: TempDir,
    server: Serve,
    _op: PathBuf,
    ada: PathBuf,
    eve: PathBuf,
    bot: PathBuf,
    nobody: PathBuf,
    judge: PathBuf,
    bot_key: String,
}

fn world() -> TestResult<World> {
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
    let judge = home("judge")?;

    // The origin: a function-editing policy at head, and a user table.
    let origin = TempDir::new("hord-auth-origin")?;
    fs::create_dir_all(origin.0.join("src"))?;
    fs::write(origin.0.join("src/lib.rs"), LIB)?;
    fs::create_dir_all(origin.0.join("docs"))?;
    fs::write(origin.0.join("docs/notes.txt"), "notes\n")?;
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
        ("judge", &["read", "arbitrate"][..]),
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

    Ok(World {
        homes,
        _origin: origin,
        clone,
        server,
        _op: op,
        ada,
        eve,
        bot,
        nobody,
        judge,
        bot_key,
    })
}

impl World {
    /// Log `user` in with the user table, from their own home; their key id.
    fn login(&self, home: &Path, user: &str) -> TestResult<String> {
        let login = json_in(
            &self.clone.0,
            home,
            &["login", "origin", "--user", user, "--password-stdin"],
            &format!("{user}-pw\n"),
        )?;
        Ok(str_field(&login, "keyId")?.to_owned())
    }

    /// As `home`: a workspace at head, `from` replaced by `to` in `file`,
    /// proposed and submitted. The change id.
    fn submit(
        &self,
        home: &Path,
        file: &str,
        from: &str,
        to: &str,
        summary: &str,
    ) -> TestResult<String> {
        let dir = &self.clone.0;
        let ws = json(dir, home, &["ws", "new"])?;
        let id = str_field(&ws, "id")?.to_owned();
        let path = PathBuf::from(str_field(&ws, "materialization")?).join(file);
        let text = fs::read_to_string(&path)?;
        assert!(text.contains(from), "{file}: {text}");
        fs::write(&path, text.replacen(from, to, 1))?;
        let intent = self
            .homes
            .0
            .join(format!("{}.md", summary.replace(' ', "-")));
        fs::write(&intent, format!("---\nsummary: {summary}\n---\nWhy.\n"))?;
        let intent = intent.to_str().ok_or("intent path is UTF-8")?;
        let proposed = json(dir, home, &["propose", "-w", &id, "--intent", intent])?;
        let change = str_field(&proposed, "change")?.to_owned();
        json(dir, home, &["submit", &change])?;
        Ok(change)
    }
}

#[test]
fn a_human_review_unblocks_a_parked_agent_change() -> TestResult {
    let w = world()?;
    let (dir, ada, eve, bot) = (&w.clone.0, &w.ada, &w.eve, &w.bot);
    let bot_key = &w.bot_key;

    // No token: refused.
    let refused = fails(dir, &w.nobody, &["queue"])?;
    assert!(refused.contains("requires a bearer token"), "{refused}");

    // The agent edits a function, proposes, and submits: parked for review.
    let change = w.submit(bot, "src/lib.rs", "    1\n", "    10\n", "alpha ten")?;
    let parked = wait_for(dir, bot, &change, PARKED)?;
    assert_eq!(parked["actor"]["agent"]["id"], "bot-1", "{parked:#}");
    assert!(
        parked["reason"]
            .as_str()
            .is_some_and(|r| r.contains("review:human")),
        "{parked:#}"
    );
    // The agent signed it, and provenance is the token's actor.
    let signed = json(dir, bot, &["key", "verify", &change, "--key", bot_key])?;
    assert_eq!(signed["verified"], true, "{signed:#}");
    assert_eq!(signed["actor"]["agent"]["id"], "bot-1", "{signed:#}");

    // A human without `review:human` cannot sign that review.
    w.login(eve, "eve")?;
    let denied = fails(
        dir,
        eve,
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
    let ada_key = w.login(ada, "ada")?;
    let review = json(
        dir,
        ada,
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
    let landed = wait_for(dir, bot, &change, LANDED)?;
    let log = json(dir, ada, &["log"])?;
    let last = log["changes"]
        .as_array()
        .and_then(|c| c.last())
        .ok_or("the log has changes")?;
    assert_eq!(last["change"], landed["landed"], "{log:#}");

    // The review verifies with Ada's key, and not with another.
    let ok = json(dir, ada, &["key", "verify", &evidence, "--key", &ada_key])?;
    assert_eq!(ok["verified"], true, "{ok:#}");
    assert_eq!(ok["kind"], "evidence");
    assert_eq!(ok["actor"]["human"]["id"], "ada", "{ok:#}");
    let out = run(
        dir,
        ada,
        &["key", "verify", &evidence, "--key", bot_key, "--json"],
        "",
    )?;
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let wrong: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(wrong["verified"], false, "{wrong:#}");
    drop(w);
    Ok(())
}

/// ADR 0031: head moves between the agent's proposal and the human's
/// review. The review is signed against the change's own result; the lander
/// rebases the change cleanly onto the new head (a different file), counts
/// the review, and lands it.
#[test]
fn a_review_carries_across_a_clean_rebase() -> TestResult {
    let w = world()?;
    let (dir, ada, eve, bot) = (&w.clone.0, &w.ada, &w.eve, &w.bot);
    let change = w.submit(bot, "src/lib.rs", "    1\n", "    10\n", "alpha ten")?;
    let parked = wait_for(dir, bot, &change, PARKED)?;
    let base = parked["report"]["head"].clone();

    // Head moves: Eve's edit touches no function, so it needs no review.
    w.login(eve, "eve")?;
    let notes = w.submit(eve, "docs/notes.txt", "notes\n", "more notes\n", "notes")?;
    wait_for(dir, eve, &notes, LANDED)?;
    let head = json(dir, eve, &["log"])?;
    let head = head["changes"]
        .as_array()
        .and_then(|c| c.last())
        .map(|c| c["change"].clone())
        .ok_or("the log has changes")?;
    assert_eq!(head, notes.as_str());
    assert_ne!(base, head);

    // Ada reviews the change as the agent proposed it; it lands rebased.
    let ada_key = w.login(ada, "ada")?;
    let review = json(
        dir,
        ada,
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
    let landed = wait_for(dir, bot, &change, LANDED)?;
    assert_ne!(landed["landed"], change.as_str(), "rebased: {landed:#}");
    let report = &landed["report"];
    assert_eq!(report["merge"], serde_json::json!([]), "{landed:#}");
    assert_eq!(report["adapterMerged"], serde_json::json!([]), "{landed:#}");
    let evidence = str_field(&review, "evidence")?;
    let ok = json(dir, ada, &["key", "verify", evidence, "--key", &ada_key])?;
    assert_eq!(ok["verified"], true, "{ok:#}");
    Ok(())
}

/// `hord arbitrate` against `hord serve --auth` (spec §6.4 rung 3,
/// §10.5.4): a token without `arbitrate` is refused; the judge's
/// `--pick theirs` lands a resolution with both parents, and the
/// `Arbitrated` event's signature verifies with the judge's key only.
#[test]
fn hord_arbitrate_resolves_a_parked_change_with_a_signed_decision() -> TestResult {
    let w = world()?;
    let (dir, bot, eve, judge) = (&w.clone.0, &w.bot, &w.eve, &w.judge);

    // Two agent edits to the same line of a file no rule covers, from the
    // same head: the first lands, the second conflicts.
    let mut changes = Vec::new();
    let workspaces = [
        json(dir, bot, &["ws", "new"])?,
        json(dir, bot, &["ws", "new"])?,
    ];
    for (ws, (text, summary)) in workspaces.iter().zip([
        ("alpha notes\n", "alpha notes"),
        ("beta notes\n", "beta notes"),
    ]) {
        let id = str_field(ws, "id")?.to_owned();
        let notes = PathBuf::from(str_field(ws, "materialization")?).join("docs/notes.txt");
        fs::write(&notes, text)?;
        let intent = w.homes.0.join(format!("{}.md", summary.replace(' ', "-")));
        fs::write(&intent, format!("---\nsummary: {summary}\n---\nWhy.\n"))?;
        let intent = intent.to_str().ok_or("intent path is UTF-8")?;
        let proposed = json(dir, bot, &["propose", "-w", &id, "--intent", intent])?;
        changes.push(str_field(&proposed, "change")?.to_owned());
    }
    let (first, second) = (&changes[0], &changes[1]);
    json(dir, bot, &["submit", first])?;
    wait_for(dir, bot, first, LANDED)?;
    json(dir, bot, &["submit", second])?;
    wait_for(dir, bot, second, "QUEUE_STATUS_CONFLICTED")?;

    // Eve may not arbitrate.
    w.login(eve, "eve")?;
    let denied = fails(dir, eve, &["arbitrate", second, "--pick", "theirs"])?;
    assert!(denied.contains("requires scope arbitrate"), "{denied}");

    // The judge takes the parked change.
    let judge_key = w.login(judge, "judge")?;
    let reply = json(dir, judge, &["arbitrate", second, "--pick", "theirs"])?;
    let resolution = str_field(&reply, "change")?.to_owned();
    let entry = wait_for(dir, judge, second, "QUEUE_STATUS_ARBITRATED")?;
    assert_eq!(entry["landed"], resolution.as_str(), "{entry:#}");
    let log = json(dir, judge, &["log"])?;
    let landed = log["changes"]
        .as_array()
        .and_then(|c| c.last())
        .ok_or("the log has changes")?;
    assert_eq!(landed["change"], resolution.as_str(), "{log:#}");
    let parents = landed["parents"].as_array().ok_or("parents")?;
    for parent in [first, second] {
        assert!(parents.iter().any(|p| p == parent.as_str()), "{landed:#}");
    }

    // The signed Arbitrated event, read with the judge's token.
    let credentials: toml::Value =
        toml::from_str(&fs::read_to_string(judge.join("credentials.toml"))?)?;
    let token = credentials["remotes"][w.server.url.as_str()]["token"]
        .as_str()
        .ok_or("the judge's token")?
        .to_owned();
    let url = w.server.url.clone();
    let parked = second.clone();
    let event = tokio::runtime::Runtime::new()?.block_on(async move {
        let remote = RemoteRepo::connect_with_token(&url, &token).await?;
        for _ in 0..300 {
            let view = ChangesBackend::get_change(
                &remote.changes(),
                proto::GetChangeRequest {
                    change: parked.clone(),
                },
            )
            .await?;
            let found = view.history.iter().find_map(|e| match e.kind() {
                Some(Kind::Arbitrated(a)) => Some(a.clone()),
                _ => None,
            });
            if let Some(found) = found {
                return Ok::<_, Box<dyn std::error::Error>>(found);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("no Arbitrated event".into())
    })?;
    assert_eq!(event.by.as_ref().map(wire::actor_id), Some("judge"));
    let signature = Signature {
        key_id: event.key_id.clone().ok_or("the event names its key")?,
        bytes: Bytes::new(event.signature.clone().ok_or("the event is signed")?),
    };
    let parked: ObjectId = second.parse()?;
    let judge_key: PublicKey = judge_key.parse()?;
    verify_arbitration(parked, &Arbitration::PickTheirs, &signature, &judge_key)?;
    let other = SigningKey::generate()?.public();
    assert!(verify_arbitration(parked, &Arbitration::PickTheirs, &signature, &other).is_err());
    Ok(())
}

/// `hord` through the repository's per-repo daemon (ADR 0021), started on
/// demand, as the identity whose `~/.hord` is `home`.
fn daemon_json(dir: &Path, home: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let out = hord(dir, home, &args)
        .env_remove("HORD_NO_DAEMON")
        .env("HORD_DAEMON_IDLE_SECS", "10")
        .output()?;
    assert!(out.status.success(), "hord {args:?}: {}", describe(&out));
    let text = String::from_utf8(out.stdout)?;
    serde_json::from_str(&text).map_err(|err| format!("{args:?}: {err}\n{text}").into())
}

/// Spec §10.5.4: a proposal made through the local daemon (or
/// `--no-daemon`) is signed with the user's key in `~/.hord/keys/`,
/// created on first use; the signature verifies with that key only, and
/// the daemon accepts the signed change and lands it.
#[test]
fn daemon_proposals_are_signed_with_the_users_key() -> TestResult {
    let home = TempDir::new("hord-auth-daemon-home")?;
    let repo = TempDir::new("hord-auth-daemon")?;
    let dir = &repo.0;
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("src/lib.rs"), LIB)?;
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(dir.join(".gitignore"), "target/\n*.md\n")?;
    git(dir, &["init", "-q", "-b", "main"])?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-q", "-m", "fixture"])?;
    let git_path = dir.to_str().ok_or("temp path is UTF-8")?;
    daemon_json(dir, &home.0, &["init", "--from-git", git_path])?;
    let key_file = home.0.join("keys/tester.pem");
    assert!(!key_file.exists());

    let propose = |no_daemon: bool, from: &str, to: &str, summary: &str| -> TestResult<String> {
        let mut extra: Vec<&str> = Vec::new();
        if no_daemon {
            extra.push("--no-daemon");
        }
        let with = |args: &[&str]| -> Vec<String> {
            extra.iter().chain(args).map(|a| (*a).to_owned()).collect()
        };
        let args = with(&["ws", "new"]);
        let ws = daemon_json(
            dir,
            &home.0,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        let id = str_field(&ws, "id")?.to_owned();
        let lib = PathBuf::from(str_field(&ws, "materialization")?).join("src/lib.rs");
        fs::write(&lib, fs::read_to_string(&lib)?.replacen(from, to, 1))?;
        let intent = dir.join(format!("{summary}.md"));
        fs::write(&intent, format!("---\nsummary: {summary}\n---\nWhy.\n"))?;
        let intent = intent.to_str().ok_or("intent path is UTF-8")?;
        let args = with(&["propose", "-w", &id, "--intent", intent]);
        let proposed = daemon_json(
            dir,
            &home.0,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        Ok(str_field(&proposed, "change")?.to_owned())
    };

    // Through the daemon: the key is created, and the signature verifies
    // with it and not with another key.
    let change = propose(false, "    1\n", "    10\n", "alpha")?;
    assert!(key_file.exists(), "no key at {}", key_file.display());
    let shown = daemon_json(dir, &home.0, &["key", "show"])?;
    let key = str_field(&shown, "keyId")?.to_owned();
    let verified = daemon_json(dir, &home.0, &["key", "verify", &change, "--key", &key])?;
    assert_eq!(verified["verified"], true, "{verified:#}");
    assert_eq!(verified["kind"], "change");
    let other = SigningKey::generate()?.public().key_id();
    let out = hord(
        dir,
        &home.0,
        &["key", "verify", &change, "--key", &other, "--json"],
    )
    .env_remove("HORD_NO_DAEMON")
    .output()?;
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));

    // The daemon accepts the signed change and lands it.
    daemon_json(dir, &home.0, &["submit", &change])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let queue = daemon_json(dir, &home.0, &["queue"])?;
        let entry = queue["entries"]
            .as_array()
            .and_then(|e| e.iter().rfind(|e| e["change"] == change.as_str()))
            .cloned()
            .ok_or_else(|| format!("{change} is queued: {queue:#}"))?;
        if entry["status"] == LANDED {
            break;
        }
        assert!(entry["status"] == "QUEUE_STATUS_QUEUED", "{entry:#}");
        assert!(
            Instant::now() < deadline,
            "{change} never landed: {entry:#}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // `--no-daemon` signs with the same key.
    let direct = propose(true, "    2\n", "    20\n", "beta")?;
    let verified = daemon_json(dir, &home.0, &["key", "verify", &direct, "--key", &key])?;
    assert_eq!(verified["verified"], true, "{verified:#}");
    Ok(())
}
