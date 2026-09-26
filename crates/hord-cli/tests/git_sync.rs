//! `hord git sync` (spec §9, ADR 0036): one pass, the divergence check's
//! exit code, and the repair, against a local repository and a bare
//! mirror.

mod common;

use common::TempDir;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn hord(dir: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hord"))
        .arg("--no-daemon")
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env_remove("HORD_AGENT_MODEL")
        .output()?)
}

fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()?;
    assert!(out.status.success(), "git {args:?}: {}", describe(&out));
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[test]
fn once_check_and_repair() -> TestResult {
    let dir = TempDir::new("hord-git-sync")?;
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo)?;
    fs::write(repo.join("README.md"), "hello\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-q", "-m", "first"])?;
    let out = hord(&repo, &["init", "--from-git", "."])?;
    assert!(out.status.success(), "{}", describe(&out));

    let mirror = dir.0.join("mirror.git");
    git(&dir.0, &["init", "-q", "--bare", "mirror.git"])?;
    // A TOML literal string: Windows paths keep their backslashes.
    let config = format!("remote = '{}'\n", mirror.display());
    fs::write(repo.join(".hord/bridge.toml"), config)?;

    let out = hord(&repo, &["git", "sync", "--once", "--json"])?;
    assert!(out.status.success(), "{}", describe(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        report["exported"].as_array().map(Vec::len),
        Some(1),
        "{report}"
    );
    let pushed = report["pushed"]
        .as_str()
        .ok_or("pushed a commit")?
        .to_owned();
    let main = git(&mirror, &["rev-parse", "refs/heads/main"])?;
    assert_eq!(main, pushed);
    let message = git(&mirror, &["log", "-1", "--format=%B", "main"])?;
    assert!(message.contains("Hord-Change: "), "{message}");

    let out = hord(&repo, &["git", "sync", "--check"])?;
    assert!(out.status.success(), "{}", describe(&out));

    // Someone pushes to main by hand.
    let manual = git(
        &mirror,
        &["commit-tree", "main^{tree}", "-p", "main", "-m", "manual"],
    )?;
    git(&mirror, &["update-ref", "refs/heads/main", &manual])?;
    let out = hord(&repo, &["git", "sync", "--check", "--json"])?;
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let check: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(check["diverged"], true, "{check}");
    assert_eq!(check["actual"].as_str(), Some(manual.as_str()));
    assert_eq!(check["trigger"], "BRIDGE_CHECK_TRIGGER_CHECK");
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/main"])?,
        manual,
        "--check changes nothing"
    );

    let out = hord(&repo, &["git", "sync", "--repair"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(git(&mirror, &["rev-parse", "refs/heads/main"])?, pushed);
    let out = hord(&repo, &["git", "sync", "--check"])?;
    assert!(out.status.success(), "{}", describe(&out));

    // `--help` says plainly what `--repair` does.
    let out = hord(&repo, &["git", "sync", "--help"])?;
    let help = String::from_utf8(out.stdout)?;
    assert!(help.contains("FORCE-PUSH"), "{help}");
    Ok(())
}
