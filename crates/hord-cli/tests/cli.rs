//! Smoke tests for M0 CLI commands. `--json` output is the protobuf JSON
//! mapping of `hord.proto` (ADR 0024): lowerCamelCase keys, 64-bit
//! integers as strings.

mod common;

use common::TempDir;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn hord_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hord"));
    // Daemons these tests start exit soon after (ADR 0021).
    cmd.env("HORD_DAEMON_IDLE_SECS", "5");
    cmd
}

fn hord_in(dir: &Path, args: &[&str]) -> TestResult<Output> {
    hord_bin()
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("run hord {args:?}: {err}").into())
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_ok(out: &Output, args: &[&str]) {
    assert!(
        out.status.success(),
        "hord {args:?} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
}

fn assert_err(out: &Output, args: &[&str]) {
    assert!(
        !out.status.success(),
        "hord {args:?} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
}

#[test]
fn help_lists_m0_commands() -> TestResult {
    let out = hord_bin().arg("--help").output()?;
    assert_ok(&out, &["--help"]);
    let text = stdout(&out);
    for cmd in ["init", "ws", "status", "log", "blame", "query", "git"] {
        assert!(text.contains(cmd), "help missing {cmd}: {text}");
    }
    Ok(())
}

#[test]
fn git_help_lists_import_export() -> TestResult {
    let out = hord_bin().args(["git", "--help"]).output()?;
    assert_ok(&out, &["git", "--help"]);
    let text = stdout(&out);
    assert!(text.contains("import"), "{text}");
    assert!(text.contains("export"), "{text}");
    Ok(())
}

#[test]
fn init_creates_hord_dir() -> TestResult {
    let tmp = TempDir::new("hord-init")?;
    let out = hord_in(tmp.path(), &["init"])?;
    assert_ok(&out, &["init"]);
    let hord_dir = tmp.path().join(".hord");
    assert!(hord_dir.is_dir(), ".hord was not created");
    assert!(stdout(&out).contains(".hord"), "{}", stdout(&out));
    Ok(())
}

#[test]
fn init_json_reports_hord_dir() -> TestResult {
    let tmp = TempDir::new("hord-init-json")?;
    let out = hord_in(tmp.path(), &["init", "--json"])?;
    assert_ok(&out, &["init", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let hord_dir = v["hordDir"].as_str().ok_or("hordDir is a string")?;
    assert!(Path::new(hord_dir).is_dir());
    assert_eq!(
        Path::new(hord_dir).file_name().and_then(|n| n.to_str()),
        Some(".hord")
    );
    Ok(())
}

#[test]
fn init_twice_fails() -> TestResult {
    let tmp = TempDir::new("hord-init-twice")?;
    let first = hord_in(tmp.path(), &["init"])?;
    assert_ok(&first, &["init"]);
    let second = hord_in(tmp.path(), &["init"])?;
    assert_err(&second, &["init"]);
    assert!(
        stderr(&second).contains("already exists"),
        "{}",
        stderr(&second)
    );
    Ok(())
}

#[test]
fn init_from_git_missing_path_fails_without_creating_store() -> TestResult {
    let tmp = TempDir::new("hord-init-from-git-missing")?;
    let missing = tmp.path().join("no-such-git");
    let missing = missing.to_str().ok_or("temp path is UTF-8")?;
    let out = hord_in(tmp.path(), &["init", "--from-git", missing])?;
    assert_err(&out, &["init", "--from-git"]);
    assert!(!tmp.path().join(".hord").exists());
    Ok(())
}

#[test]
fn log_after_init_is_empty() -> TestResult {
    let tmp = TempDir::new("hord-log")?;
    assert_ok(&hord_in(tmp.path(), &["init"])?, &["init"]);
    let out = hord_in(tmp.path(), &["log", "--json"])?;
    assert_ok(&out, &["log", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert!(v.get("head").is_none_or(serde_json::Value::is_null));
    assert_eq!(v["changes"], serde_json::json!([]));
    Ok(())
}

#[test]
fn ws_new_prints_id_and_materialization() -> TestResult {
    let tmp = TempDir::new("hord-ws-new")?;
    assert_ok(&hord_in(tmp.path(), &["init"])?, &["init"]);
    let out = hord_in(tmp.path(), &["ws", "new", "--json"])?;
    assert_ok(&out, &["ws", "new", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let id = v["id"].as_str().ok_or("id is a string")?;
    assert_eq!(id.len(), 26, "workspace id should be a ULID");
    let materialization = v["materialization"]
        .as_str()
        .ok_or("materialization is a string")?;
    assert!(
        Path::new(materialization).is_dir(),
        "materialization path missing: {materialization}"
    );
    let base = v["base"].as_str().ok_or("base is a string")?;
    assert_eq!(base.len(), 64, "base should be a hex ObjectId");
    Ok(())
}

#[test]
fn status_after_ws_new() -> TestResult {
    let tmp = TempDir::new("hord-status")?;
    assert_ok(&hord_in(tmp.path(), &["init"])?, &["init"]);
    let created = hord_in(tmp.path(), &["ws", "new", "--json"])?;
    assert_ok(&created, &["ws", "new", "--json"]);
    let created: serde_json::Value = serde_json::from_slice(&created.stdout)?;
    let id = created["id"].as_str().ok_or("id is a string")?;

    let out = hord_in(tmp.path(), &["status", "--json", "-w", id])?;
    assert_ok(&out, &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["workspace"], id);
    assert_eq!(v["ops"], serde_json::json!([]));
    assert_eq!(v["readSet"], serde_json::json!([]));
    assert_eq!(v["writeSet"], serde_json::json!([]));
    assert_eq!(v["evidence"], serde_json::json!([]));
    assert_eq!(v["evidenceStale"], serde_json::json!(false));
    Ok(())
}

#[test]
fn status_without_init_fails_json() -> TestResult {
    let tmp = TempDir::new("hord-status-no-repo")?;
    let out = hord_in(tmp.path(), &["status", "--json"])?;
    assert_err(&out, &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stderr)?;
    let error = v["error"].as_str().ok_or("error is a string")?;
    assert!(error.contains(".hord"));
    Ok(())
}

#[test]
fn git_import_without_repo_fails() -> TestResult {
    let tmp = TempDir::new("hord-git-import-no-repo")?;
    let out = hord_in(tmp.path(), &["git", "import", "HEAD"])?;
    assert_err(&out, &["git", "import", "HEAD"]);
    Ok(())
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn init_git_fixture(dir: &Path) -> TestResult {
    run_git(dir, &["init"])?;
    run_git(dir, &["config", "user.email", "hord@example.com"])?;
    run_git(dir, &["config", "user.name", "Hord"])?;
    run_git(dir, &["config", "commit.gpgsign", "false"])?;
    fs::write(dir.join("README"), b"hello\n")?;
    run_git(dir, &["add", "README"])?;
    run_git(dir, &["commit", "-m", "init"])
}

fn run_git(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("git {args:?}: {err}"))?;
    assert!(
        out.status.success(),
        "git {args:?} failed\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// The imported change count in a `--json` report, a decimal string.
fn imported_changes(imported: &serde_json::Value) -> TestResult<u64> {
    let changes = imported["changes"]
        .as_str()
        .ok_or("imported changes is a string")?;
    Ok(changes.parse()?)
}

#[test]
fn init_from_git_tiny_fixture() -> TestResult {
    if !git_available() {
        return Ok(());
    }
    let tmp = TempDir::new("hord-from-git")?;
    init_git_fixture(tmp.path())?;
    let git_path = tmp.path().to_str().ok_or("temp path is UTF-8")?;
    let out = hord_in(tmp.path(), &["init", "--from-git", git_path, "--json"])?;
    assert_ok(&out, &["init", "--from-git"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let hord_dir = v["hordDir"].as_str().ok_or("hordDir is a string")?;
    assert!(Path::new(hord_dir).is_dir());
    let imported = v.get("imported").ok_or("init reports imported")?;
    assert!(
        imported_changes(imported)? >= 1,
        "expected at least one imported change: {imported}"
    );
    let log = hord_in(tmp.path(), &["log", "--json"])?;
    assert_ok(&log, &["log", "--json"]);
    let log: serde_json::Value = serde_json::from_slice(&log.stdout)?;
    let changes = log["changes"].as_array().ok_or("changes is an array")?;
    assert!(
        !changes.is_empty(),
        "log should be non-empty after --from-git"
    );
    Ok(())
}

#[test]
fn git_export_import_after_init_in_git_repo() -> TestResult {
    if !git_available() {
        return Ok(());
    }
    let tmp = TempDir::new("hord-git-roundtrip")?;
    init_git_fixture(tmp.path())?;
    assert_ok(&hord_in(tmp.path(), &["init"])?, &["init"]);

    let import = hord_in(tmp.path(), &["git", "import", "HEAD", "--json"])?;
    assert_ok(&import, &["git", "import", "HEAD"]);
    let imported: serde_json::Value = serde_json::from_slice(&import.stdout)?;
    assert!(imported_changes(&imported)? >= 1);

    let export = hord_in(tmp.path(), &["git", "export", "head", "--json"])?;
    assert_ok(&export, &["git", "export", "head"]);
    let exported: serde_json::Value = serde_json::from_slice(&export.stdout)?;
    assert!(
        exported.get("gitCommit").and_then(|v| v.as_str()).is_some(),
        "export should produce a git commit: {exported}"
    );
    Ok(())
}
