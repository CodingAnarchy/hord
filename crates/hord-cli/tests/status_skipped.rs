//! `hord status` reports untracked files a `Directory` propose skips: build
//! output a `.gitignore` of the base names, and the built-in `.hord/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env_remove("HORD_AGENT_MODEL")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

#[test]
fn status_lists_skipped_untracked_files() {
    let source = TempDir::new("hord-skip-git");
    fs::write(source.0.join("README.md"), "# fixture\n").unwrap();
    fs::write(source.0.join(".gitignore"), "/target\n*.log\n").unwrap();
    git(&source.0, &["init", "-q", "-b", "main"]);
    git(&source.0, &["add", "."]);
    git(&source.0, &["commit", "-q", "-m", "fixture"]);
    let repo = TempDir::new("hord-skip-repo");
    let dir = &repo.0;
    ok(dir, &["init", "--from-git", source.0.to_str().unwrap()]);
    let ws: serde_json::Value = serde_json::from_str(&ok(dir, &["ws", "new", "--json"])).unwrap();
    let id = ws["id"].as_str().unwrap();
    let checkout = PathBuf::from(ws["materialization"].as_str().unwrap());

    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    fs::write(checkout.join("target/debug/bin"), [0u8, 1, 2]).unwrap();
    fs::write(checkout.join("build.log"), "log\n").unwrap();
    fs::write(checkout.join("README.md"), "# edited\n").unwrap();

    let status: serde_json::Value =
        serde_json::from_str(&ok(dir, &["status", "-w", id, "--json"])).unwrap();
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

    let text = ok(dir, &["status", "-w", id]);
    assert!(
        text.contains("skipped (untracked, not proposed):\n  build.log\n  target/\n"),
        "{text}"
    );
}
