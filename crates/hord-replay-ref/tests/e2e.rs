//! End to end (spec §6.4 rung 2, §6.6): the daemon's lander runs
//! `hord-replay-ref` from `.hord/replay.toml` on a hard conflict; the
//! reference harness runs its command in the replay workspace, proposes
//! through `hord`, and the replay lands.
//!
//! Needs the `hord` binary next to this crate's (`cargo build -p hord-cli`,
//! which `cargo test --workspace` does), or its path in `HORD_BIN`.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const LIB: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n";

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
        // Pristine checkouts are read-only (ADR 0016); the daemon exits
        // once `.hord/` is gone.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The `hord` binary: `HORD_BIN`, else next to this crate's binary.
fn hord_bin() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("HORD_BIN") {
        return Ok(PathBuf::from(path));
    }
    let harness = Path::new(env!("CARGO_BIN_EXE_hord-replay-ref"));
    let hord = harness
        .parent()
        .ok_or("the harness binary has a directory")?
        .join("hord");
    if !hord.exists() {
        return Err(format!(
            "{} is missing: build it first (cargo build -p hord-cli) or set HORD_BIN",
            hord.display()
        )
        .into());
    }
    Ok(hord)
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Hord {
    bin: PathBuf,
    dir: PathBuf,
}

impl Hord {
    fn run(&self, args: &[&str]) -> TestResult<String> {
        let out = Command::new(&self.bin)
            .args(args)
            .current_dir(&self.dir)
            .env("HORD_ACTOR", "tester")
            .env("HORD_DAEMON_IDLE_SECS", "20")
            .env_remove("HORD_NO_DAEMON")
            .env_remove("HORD_AGENT_MODEL")
            .output()?;
        if !out.status.success() {
            return Err(format!("hord {args:?}: {}", describe(&out)).into());
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    fn json(&self, args: &[&str]) -> TestResult<serde_json::Value> {
        let mut args = args.to_vec();
        args.push("--json");
        let text = self.run(&args)?;
        serde_json::from_str(&text).map_err(|err| format!("{args:?}: {err}\n{text}").into())
    }
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
    if !out.status.success() {
        return Err(format!("git {args:?}: {}", describe(&out)).into());
    }
    Ok(())
}

/// Propose from workspace `ws` (at `checkout`) with `beta` returning `n`.
fn propose_beta(
    hord: &Hord,
    notes: &Path,
    ws: &str,
    checkout: &Path,
    n: u32,
) -> TestResult<String> {
    fs::write(
        checkout.join("src/lib.rs"),
        LIB.replace("    2\n", &format!("    {n}\n")),
    )?;
    let intent = notes.join(format!("beta-{n}.md"));
    fs::write(
        &intent,
        format!(
            "---\nsummary: beta returns {n}\nacceptance:\n  - test: beta_is_{n}\n---\nMake beta return {n}.\n"
        ),
    )?;
    let intent = intent.to_str().ok_or("intent path is UTF-8")?;
    let proposed = hord.json(&["propose", "-w", ws, "--intent", intent])?;
    Ok(str_field(&proposed, "change")?.to_owned())
}

fn new_workspace(hord: &Hord) -> TestResult<(String, PathBuf)> {
    let ws = hord.json(&["ws", "new"])?;
    Ok((
        str_field(&ws, "id")?.to_owned(),
        PathBuf::from(str_field(&ws, "materialization")?),
    ))
}

/// A git repository with `src/lib.rs` at `dir`, imported into hord, with
/// `.hord/replay.toml` running `hord-replay-ref` whose "model" saves its
/// prompt to `prompt_copy`, makes beta return 21 on the new base, and
/// reports its usage.
fn setup(hord: &Hord, prompt_copy: &Path) -> TestResult {
    let dir = &hord.dir;
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("src/lib.rs"), LIB)?;
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(dir.join(".gitignore"), "target/\n")?;
    git(dir, &["init", "-q", "-b", "main"])?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-q", "-m", "fixture"])?;
    let git_path = dir.to_str().ok_or("temp path is UTF-8")?;
    hord.run(&["init", "--from-git", git_path])?;
    let cmd = format!(
        "cat > '{}' && printf 'pub fn alpha() -> u32 {{\\n    1\\n}}\\n\\npub fn beta() -> u32 {{\\n    21\\n}}\\n' > src/lib.rs && printf '{{\"tokens\": 42, \"cost_usd\": 0.01}}' > \"$HORD_REPLAY_USAGE\"",
        prompt_copy.display()
    );
    let harness = env!("CARGO_BIN_EXE_hord-replay-ref");
    let config = format!(
        "harness = [{:?}, \"--cmd\", {:?}, \"--hord\", {:?}, \"--model\", \"scripted\"]\n",
        harness,
        cmd,
        hord.bin.display().to_string()
    );
    fs::write(dir.join(".hord").join("replay.toml"), config)?;
    Ok(())
}

/// a makes beta 20 and lands; b makes beta 21 on the same base and
/// conflicts; the harness replays b, and the replay lands.
fn conflict_is_replayed(hord: &Hord, notes: &Path, prompt_copy: &Path) -> TestResult {
    let (ws_a, checkout_a) = new_workspace(hord)?;
    let (ws_b, checkout_b) = new_workspace(hord)?;
    let a = propose_beta(hord, notes, &ws_a, &checkout_a, 20)?;
    let b = propose_beta(hord, notes, &ws_b, &checkout_b, 21)?;
    hord.run(&["land", "--local", &a])?;
    hord.run(&["submit", &b])?;

    let deadline = Instant::now() + Duration::from_secs(90);
    let entry = loop {
        let queue = hord.json(&["queue"])?;
        let entries = queue["entries"].as_array().ok_or("entries")?;
        let entry = entries
            .iter()
            .rev()
            .find(|e| e["change"] == b.as_str())
            .cloned()
            .ok_or("b's entry")?;
        if entry["status"] == "QUEUE_STATUS_REPLAYED" {
            break entry;
        }
        if entry["status"] == "QUEUE_STATUS_NEEDS_ARBITRATION" || Instant::now() > deadline {
            return Err(format!("b was not replayed: {queue:#}").into());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let landed = str_field(&entry, "landed")?.to_owned();
    let head = hord.json(&["log"])?;
    assert_eq!(head["head"], landed.as_str(), "{head:#}");

    let conflicts = hord.json(&["conflicts", &b])?;
    let attempt = &conflicts["entry"]["escalation"]["attempts"][0];
    assert_eq!(
        attempt["outcome"], "REPLAY_OUTCOME_PROPOSED",
        "{conflicts:#}"
    );
    assert_eq!(attempt["tokens"], "42", "{attempt:#}");
    assert_eq!(attempt["costMicros"], "10000", "{attempt:#}");
    assert_eq!(attempt["model"], "scripted", "{attempt:#}");
    let summary = &conflicts["entry"]["escalation"]["summary"]["text"];
    assert!(
        summary
            .as_str()
            .is_some_and(|s| s.contains("beta returns 20")),
        "{conflicts:#}"
    );

    let prompt = fs::read_to_string(prompt_copy)?;
    assert!(prompt.contains("beta returns 21"), "{prompt}");
    assert!(prompt.contains("beta_is_21"), "{prompt}");
    assert!(
        prompt.contains("beta returns 20"),
        "the landed side: {prompt}"
    );
    Ok(())
}

#[test]
fn the_reference_harness_resolves_a_conflict_through_the_daemon() -> TestResult {
    let repo = TempDir::new("hord-replay-ref")?;
    let notes = TempDir::new("hord-replay-ref-notes")?;
    let hord = Hord {
        bin: hord_bin()?,
        dir: repo.0.clone(),
    };
    let prompt_copy = notes.0.join("prompt.txt");
    setup(&hord, &prompt_copy)?;
    conflict_is_replayed(&hord, &notes.0, &prompt_copy)
}

/// Kills the server when the test ends.
struct Serving(std::process::Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spec §10.5.1: under `hord serve --root`, each hosted repository's lander
/// runs its own `.hord/replay.toml` harness, and `hord` commands in the
/// repository (the harness's `hord propose` too) reach the server on the
/// repository's local endpoint.
#[test]
fn serve_root_runs_each_repositorys_replay_harness() -> TestResult {
    let root = TempDir::new("hord-replay-ref-root")?;
    let notes = TempDir::new("hord-replay-ref-notes")?;
    let bin = hord_bin()?;
    let hord = Hord {
        bin: bin.clone(),
        dir: root.0.join("alpha"),
    };
    let prompt_copy = notes.0.join("prompt.txt");
    setup(&hord, &prompt_copy)?;
    // A second hosted repository with no harness.
    let other = Hord {
        bin: bin.clone(),
        dir: root.0.join("beta"),
    };
    setup(&other, &notes.0.join("unused.txt"))?;
    fs::remove_file(other.dir.join(".hord").join("replay.toml"))?;

    let mut child = Command::new(&bin)
        .args(["serve", "--root"])
        .arg(&root.0)
        .args(["--bind", "127.0.0.1:0"])
        .env_remove("HORD_NO_DAEMON")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().ok_or("the server's stderr")?;
    let serving = Serving(child);
    let mut first = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stderr), &mut first)?;
    assert!(first.starts_with("hord serve: http://"), "{first}");
    assert!(first.contains("alpha") && first.contains("beta"), "{first}");
    let endpoint = PathBuf::from(hord_api::local::endpoint(&hord.dir.canonicalize()?)?);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !endpoint.exists() {
        if Instant::now() > deadline {
            return Err(format!("{} never appeared", endpoint.display()).into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    conflict_is_replayed(&hord, &notes.0, &prompt_copy)?;
    drop(serving);
    Ok(())
}
