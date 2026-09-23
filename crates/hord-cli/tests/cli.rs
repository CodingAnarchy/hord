//! Smoke tests for M0 CLI commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn hord_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hord"))
}

fn hord_in(dir: &Path, args: &[&str]) -> Output {
    hord_bin()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("run hord {args:?}: {err}"))
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

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn help_lists_m0_commands() {
    let out = hord_bin().arg("--help").output().unwrap();
    assert_ok(&out, &["--help"]);
    let text = stdout(&out);
    for cmd in ["init", "ws", "status", "log", "blame", "query", "git"] {
        assert!(text.contains(cmd), "help missing {cmd}: {text}");
    }
}

#[test]
fn git_help_lists_import_export() {
    let out = hord_bin().args(["git", "--help"]).output().unwrap();
    assert_ok(&out, &["git", "--help"]);
    let text = stdout(&out);
    assert!(text.contains("import"), "{text}");
    assert!(text.contains("export"), "{text}");
}

#[test]
fn init_creates_hord_dir() {
    let tmp = TempDir::new("hord-init");
    let out = hord_in(tmp.path(), &["init"]);
    assert_ok(&out, &["init"]);
    let hord_dir = tmp.path().join(".hord");
    assert!(hord_dir.is_dir(), ".hord was not created");
    assert!(stdout(&out).contains(".hord"), "{}", stdout(&out));
}

#[test]
fn init_json_reports_hord_dir() {
    let tmp = TempDir::new("hord-init-json");
    let out = hord_in(tmp.path(), &["init", "--json"]);
    assert_ok(&out, &["init", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let hord_dir = v["hord_dir"].as_str().expect("hord_dir");
    assert!(Path::new(hord_dir).is_dir());
    assert_eq!(
        Path::new(hord_dir).file_name().and_then(|n| n.to_str()),
        Some(".hord")
    );
}

#[test]
fn init_twice_fails() {
    let tmp = TempDir::new("hord-init-twice");
    let first = hord_in(tmp.path(), &["init"]);
    assert_ok(&first, &["init"]);
    let second = hord_in(tmp.path(), &["init"]);
    assert_err(&second, &["init"]);
    assert!(
        stderr(&second).contains("already exists"),
        "{}",
        stderr(&second)
    );
}

#[test]
fn init_from_git_missing_path_fails_without_creating_store() {
    let tmp = TempDir::new("hord-init-from-git-missing");
    let missing = tmp.path().join("no-such-git");
    let out = hord_in(
        tmp.path(),
        &["init", "--from-git", missing.to_str().unwrap()],
    );
    assert_err(&out, &["init", "--from-git"]);
    assert!(!tmp.path().join(".hord").exists());
}

#[test]
fn log_after_init_is_empty() {
    let tmp = TempDir::new("hord-log");
    assert_ok(&hord_in(tmp.path(), &["init"]), &["init"]);
    let out = hord_in(tmp.path(), &["log", "--json"]);
    assert_ok(&out, &["log", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(v["head"].is_null());
    assert_eq!(v["log"], serde_json::json!([]));
}

#[test]
fn ws_new_prints_id_and_materialization() {
    let tmp = TempDir::new("hord-ws-new");
    assert_ok(&hord_in(tmp.path(), &["init"]), &["init"]);
    let out = hord_in(tmp.path(), &["ws", "new", "--json"]);
    assert_ok(&out, &["ws", "new", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let id = v["id"].as_str().expect("id");
    assert_eq!(id.len(), 26, "workspace id should be a ULID");
    let materialization = v["materialization"].as_str().expect("materialization");
    assert!(
        Path::new(materialization).is_dir(),
        "materialization path missing: {materialization}"
    );
    let base = v["base"].as_str().expect("base");
    assert_eq!(base.len(), 64, "base should be a hex ObjectId");
}

#[test]
fn status_after_ws_new() {
    let tmp = TempDir::new("hord-status");
    assert_ok(&hord_in(tmp.path(), &["init"]), &["init"]);
    let created = hord_in(tmp.path(), &["ws", "new", "--json"]);
    assert_ok(&created, &["ws", "new", "--json"]);
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let id = created["id"].as_str().unwrap();

    let out = hord_in(tmp.path(), &["status", "--json", "-w", id]);
    assert_ok(&out, &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["workspace"], id);
    assert_eq!(v["ops"], serde_json::json!([]));
    assert_eq!(v["read_set"], serde_json::json!([]));
    assert_eq!(v["write_set"], serde_json::json!([]));
    assert_eq!(v["evidence"], serde_json::json!([]));
    assert_eq!(v["evidence_stale"], serde_json::json!(false));
}

#[test]
fn status_without_init_fails_json() {
    let tmp = TempDir::new("hord-status-no-repo");
    let out = hord_in(tmp.path(), &["status", "--json"]);
    assert_err(&out, &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).expect("json error");
    assert!(v["error"].as_str().unwrap().contains(".hord"));
}

#[test]
fn git_import_without_repo_fails() {
    let tmp = TempDir::new("hord-git-import-no-repo");
    let out = hord_in(tmp.path(), &["git", "import", "HEAD"]);
    assert_err(&out, &["git", "import", "HEAD"]);
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn init_git_fixture(dir: &Path) {
    run_git(dir, &["init"]);
    run_git(dir, &["config", "user.email", "hord@example.com"]);
    run_git(dir, &["config", "user.name", "Hord"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("README"), b"hello\n").unwrap();
    run_git(dir, &["add", "README"]);
    run_git(dir, &["commit", "-m", "init"]);
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(
        out.status.success(),
        "git {args:?} failed\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn init_from_git_tiny_fixture() {
    if !git_available() {
        return;
    }
    let tmp = TempDir::new("hord-from-git");
    init_git_fixture(tmp.path());
    let git_path = tmp.path().to_str().unwrap();
    let out = hord_in(tmp.path(), &["init", "--from-git", git_path, "--json"]);
    assert_ok(&out, &["init", "--from-git"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(Path::new(v["hord_dir"].as_str().unwrap()).is_dir());
    let imported = v.get("imported").expect("imported");
    assert!(
        imported["changes"].as_u64().unwrap() >= 1,
        "expected at least one imported change: {imported}"
    );
    let log = hord_in(tmp.path(), &["log", "--json"]);
    assert_ok(&log, &["log", "--json"]);
    let log: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    assert!(
        !log["log"].as_array().unwrap().is_empty(),
        "log should be non-empty after --from-git"
    );
}

#[test]
fn git_export_import_after_init_in_git_repo() {
    if !git_available() {
        return;
    }
    let tmp = TempDir::new("hord-git-roundtrip");
    init_git_fixture(tmp.path());
    assert_ok(&hord_in(tmp.path(), &["init"]), &["init"]);

    let import = hord_in(tmp.path(), &["git", "import", "HEAD", "--json"]);
    assert_ok(&import, &["git", "import", "HEAD"]);
    let imported: serde_json::Value = serde_json::from_slice(&import.stdout).unwrap();
    assert!(imported["changes"].as_u64().unwrap() >= 1);

    let export = hord_in(tmp.path(), &["git", "export", "head", "--json"]);
    assert_ok(&export, &["git", "export", "head"]);
    let exported: serde_json::Value = serde_json::from_slice(&export.stdout).unwrap();
    assert!(
        exported
            .get("git_commit")
            .and_then(|v| v.as_str())
            .is_some(),
        "export should produce a git commit: {exported}"
    );
}
