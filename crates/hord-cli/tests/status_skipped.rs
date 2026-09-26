//! `hord status` reports untracked files a `Directory` propose skips: build
//! output a `.gitignore` of the base names, and the built-in `.hord/`.

mod common;

use common::TempDir;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn ok(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        // A daemon this test starts exits soon after (ADR 0021).
        .env("HORD_DAEMON_IDLE_SECS", "5")
        .env_remove("HORD_AGENT_MODEL")
        .output()?;
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
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
    assert!(out.status.success(), "git {args:?}");
    Ok(())
}

#[test]
fn status_lists_skipped_untracked_files() -> TestResult {
    let source = TempDir::new("hord-skip-git")?;
    fs::write(source.0.join("README.md"), "# fixture\n")?;
    fs::write(source.0.join(".gitignore"), "/target\n*.log\n")?;
    git(&source.0, &["init", "-q", "-b", "main"])?;
    git(&source.0, &["add", "."])?;
    git(&source.0, &["commit", "-q", "-m", "fixture"])?;
    let repo = TempDir::new("hord-skip-repo")?;
    let dir = &repo.0;
    let source_path = source.0.to_str().ok_or("temp path is UTF-8")?;
    ok(dir, &["init", "--from-git", source_path])?;
    let ws: serde_json::Value = serde_json::from_str(&ok(dir, &["ws", "new", "--json"])?)?;
    let id = ws["id"].as_str().ok_or("workspace id is a string")?;
    let checkout = PathBuf::from(
        ws["materialization"]
            .as_str()
            .ok_or("materialization is a string")?,
    );

    fs::create_dir_all(checkout.join("target/debug"))?;
    fs::write(checkout.join("target/debug/bin"), [0u8, 1, 2])?;
    fs::write(checkout.join("build.log"), "log\n")?;
    fs::write(checkout.join("README.md"), "# edited\n")?;

    let status: serde_json::Value =
        serde_json::from_str(&ok(dir, &["status", "-w", id, "--json"])?)?;
    assert_eq!(
        status["skipped"],
        serde_json::json!(["build.log", "target/"]),
        "{status:#}"
    );
    let ops = status["ops"].to_string();
    assert!(
        ops.contains("README.md") && !ops.contains("target"),
        "{ops}"
    );

    let text = ok(dir, &["status", "-w", id])?;
    assert!(
        text.contains("skipped (untracked, not proposed):\n  build.log\n  target/\n"),
        "{text}"
    );
    Ok(())
}
