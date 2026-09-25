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

#[test]
fn the_reference_harness_resolves_a_conflict_through_the_daemon() -> TestResult {
    let bin = hord_bin()?;
    let repo = TempDir::new("hord-replay-ref")?;
    let notes = TempDir::new("hord-replay-ref-notes")?;
    let dir = repo.0.clone();
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("src/lib.rs"), LIB)?;
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(dir.join(".gitignore"), "target/\n")?;
    git(&dir, &["init", "-q", "-b", "main"])?;
    git(&dir, &["add", "."])?;
    git(&dir, &["commit", "-q", "-m", "fixture"])?;
    let hord = Hord {
        bin: bin.clone(),
        dir: dir.clone(),
    };
    let git_path = dir.to_str().ok_or("temp path is UTF-8")?;
    hord.run(&["init", "--from-git", git_path])?;

    // The "model": saves its prompt, makes beta return 21 on the new base,
    // and reports its usage.
    let prompt_copy = notes.0.join("prompt.txt");
    let cmd = format!(
        "cat > '{}' && printf 'pub fn alpha() -> u32 {{\\n    1\\n}}\\n\\npub fn beta() -> u32 {{\\n    21\\n}}\\n' > src/lib.rs && printf '{{\"tokens\": 42, \"cost_usd\": 0.01}}' > \"$HORD_REPLAY_USAGE\"",
        prompt_copy.display()
    );
    let harness = env!("CARGO_BIN_EXE_hord-replay-ref");
    let config = format!(
        "harness = [{:?}, \"--cmd\", {:?}, \"--hord\", {:?}, \"--model\", \"scripted\"]\n",
        harness,
        cmd,
        bin.display().to_string()
    );
    fs::write(dir.join(".hord").join("replay.toml"), config)?;

    let (ws_a, checkout_a) = new_workspace(&hord)?;
    let (ws_b, checkout_b) = new_workspace(&hord)?;
    let a = propose_beta(&hord, &notes.0, &ws_a, &checkout_a, 20)?;
    let b = propose_beta(&hord, &notes.0, &ws_b, &checkout_b, 21)?;
    hord.run(&["land", "--local", &a])?;
    hord.run(&["submit", &b])?;

    // b conflicts with a; the harness replays it on a's result.
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

    let prompt = fs::read_to_string(&prompt_copy)?;
    assert!(prompt.contains("beta returns 21"), "{prompt}");
    assert!(prompt.contains("beta_is_21"), "{prompt}");
    assert!(
        prompt.contains("beta returns 20"),
        "the landed side: {prompt}"
    );
    Ok(())
}
