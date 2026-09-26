//! `hord audit` in a local repository (spec §12 M6): a window holding the
//! git import, which did not come through the lander, fails with a non-zero
//! exit, as text and as the `AuditReport` JSON mapping; a window after it passes, noting that no
//! bridge checks were recorded.

mod common;

use common::TempDir;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn hord(dir: &Path, home: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_HOME", home)
        .env("HORD_ACTOR", "tester")
        .env("HORD_NO_DAEMON", "1")
        .env_remove("HORD_AGENT_MODEL")
        .output()?)
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
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

#[test]
fn audit_fails_on_the_import_and_passes_after_it() -> TestResult {
    let root = TempDir::new("hord-audit-cli")?;
    let (home, repo) = (root.0.join("home"), root.0.join("repo"));
    fs::create_dir_all(&home)?;
    fs::create_dir_all(repo.join("src"))?;
    fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-q", "-m", "fixture"])?;
    let from = repo.to_str().ok_or("temp path is UTF-8")?;
    let init = hord(&repo, &home, &["init", "--from-git", from])?;
    assert!(init.status.success(), "{}", describe(&init));

    let out = hord(&repo, &home, &["audit", "--since", "1970-01-01", "--json"])?;
    assert!(!out.status.success(), "{}", describe(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(report["ok"], false, "{report:#}");
    let violations = report["violations"].as_array().ok_or("violations")?;
    assert!(
        violations
            .iter()
            .any(|v| v["criterion"] == "AUDIT_CRITERION_UNRECORDED_LANDING"),
        "{report:#}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("audit failed"));

    let text = hord(&repo, &home, &["audit", "--since", "1970-01-01"])?;
    assert!(!text.status.success());
    let stdout = String::from_utf8(text.stdout)?;
    assert!(stdout.contains("violations:"), "{stdout}");
    assert!(stdout.contains("store edit"), "{stdout}");

    let later = hord(
        &repo,
        &home,
        &["audit", "--since", "2999-01-01", "--until", "2999-01-02"],
    )?;
    assert!(later.status.success(), "{}", describe(&later));
    let stdout = String::from_utf8(later.stdout)?;
    assert!(stdout.contains("0 landed"), "{stdout}");
    assert!(stdout.contains("no bridge checks recorded"), "{stdout}");
    assert!(stdout.contains("ok: no violations"), "{stdout}");

    let bad = hord(&repo, &home, &["audit", "--since", "last week"])?;
    assert!(!bad.status.success());
    Ok(())
}
