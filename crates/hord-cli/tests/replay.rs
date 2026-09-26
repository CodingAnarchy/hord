//! `hord replay` (spec §6.6) takes its budget from head's `[replay] budget`
//! (ADR 0028), and each flag overrides one field.

#![cfg(unix)]

mod common;

use common::TempDir;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const LIB: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n";
/// One second and ten tokens per attempt.
const POLICY: &str = "[replay]\nbudget = { wall_time_secs = 1, tokens = 10 }\n";

fn hord(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_DAEMON_IDLE_SECS", "10")
        .env_remove("HORD_NO_DAEMON")
        .env_remove("HORD_AGENT_MODEL")
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "hord {args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn json(dir: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let text = hord(dir, &args)?;
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
    if !out.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(())
}

/// A conflicted change `b` (no harness configured, so it waits).
fn conflicted(dir: &Path, notes: &Path) -> TestResult<String> {
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("src/lib.rs"), LIB)?;
    fs::write(dir.join(".hord-policy.toml"), POLICY)?;
    git(dir, &["init", "-q", "-b", "main"])?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-q", "-m", "fixture"])?;
    hord(
        dir,
        &["init", "--from-git", dir.to_str().ok_or("UTF-8 path")?],
    )?;
    let mut changes = Vec::new();
    for n in [20, 21] {
        let ws = json(dir, &["ws", "new"])?;
        let id = ws["id"].as_str().ok_or("workspace id")?.to_owned();
        let checkout = PathBuf::from(ws["materialization"].as_str().ok_or("checkout")?);
        fs::write(
            checkout.join("src/lib.rs"),
            LIB.replace("    2\n", &format!("    {n}\n")),
        )?;
        let intent = notes.join(format!("{n}.md"));
        fs::write(&intent, format!("---\nsummary: beta returns {n}\n---\n"))?;
        let proposed = json(
            dir,
            &[
                "propose",
                "-w",
                &id,
                "--intent",
                intent.to_str().ok_or("UTF-8")?,
            ],
        )?;
        changes.push(proposed["change"].as_str().ok_or("change")?.to_owned());
    }
    hord(dir, &["land", "--local", &changes[0]])?;
    hord(dir, &["land", "--local", &changes[1]])?;
    let status = json(dir, &["conflicts", &changes[1]])?;
    assert_eq!(
        status["entry"]["status"], "QUEUE_STATUS_CONFLICTED",
        "{status:#}"
    );
    Ok(changes[1].clone())
}

#[test]
fn hord_replay_takes_heads_budget_and_flags_override_fields() -> TestResult {
    let repo = TempDir::new("hord-replay-budget")?;
    let notes = TempDir::new("hord-replay-budget-notes")?;
    let dir = &repo.0;
    let b = conflicted(dir, &notes.0)?;

    // Head's one second: a harness that sleeps is killed.
    let started = Instant::now();
    let run = json(dir, &["replay", &b, "--harness", "sleep 5"])?;
    assert!(started.elapsed() < Duration::from_secs(4), "{run:#}");
    let attempt = &run["attempt"];
    assert_eq!(attempt["outcome"], "REPLAY_OUTCOME_KILLED", "{run:#}");
    assert!(
        attempt["detail"]
            .as_str()
            .is_some_and(|d| d.contains("1000 ms")),
        "{run:#}"
    );

    // Head's ten tokens: a result reporting fifty is over budget.
    let fifty = "echo '{\"gaveUp\": {\"reason\": \"scripted\"}, \"tokens\": \"50\"}'";
    let run = json(dir, &["replay", &b, "--harness", fifty])?;
    assert_eq!(
        run["attempt"]["outcome"], "REPLAY_OUTCOME_OVER_BUDGET",
        "{run:#}"
    );

    // --tokens replaces that field only: fifty is now within budget.
    let run = json(dir, &["replay", &b, "--harness", fifty, "--tokens", "100"])?;
    assert_eq!(
        run["attempt"]["outcome"], "REPLAY_OUTCOME_GAVE_UP",
        "{run:#}"
    );

    // --wall-time-secs replaces the time: the sleeper outlives one second.
    let run = json(
        dir,
        &[
            "replay",
            &b,
            "--harness",
            "sleep 1.5; echo '{\"gaveUp\": {\"reason\": \"slow\"}}'",
            "--wall-time-secs",
            "4",
        ],
    )?;
    assert_eq!(
        run["attempt"]["outcome"], "REPLAY_OUTCOME_GAVE_UP",
        "{run:#}"
    );
    Ok(())
}
