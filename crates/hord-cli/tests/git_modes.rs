//! Symlinks and executable files through a whole round trip: `init
//! --from-git`, `ws new`, `status`, `propose`, `land --local`, and `git
//! export` (spec §9: every tree SHA is reproduced).
//!
//! Unix only: the fixture makes symlinks and sets exec bits.

#![cfg(unix)]

mod common;

use common::{TempDir, git_command};

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn run(dir: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_NO_DAEMON", "1")
        .env("HORD_HOME", dir.join(".hord-home"))
        .env_remove("HORD_AGENT_MODEL")
        .output()
        .map_err(|err| format!("run hord {args:?}: {err}"))?)
}

fn json(dir: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let out = run(dir, &args)?;
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_slice(&out.stdout).map_err(|err| format!("{args:?}: {err}"))?)
}

fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = git_command()
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()?;
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

fn set_exec(path: &Path, exec: bool) -> TestResult {
    let mode = if exec { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn is_exec(path: &Path) -> TestResult<bool> {
    Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o111 != 0)
}

/// The tree of the commit `hord git export head` writes.
fn export_tree(dir: &Path) -> TestResult<String> {
    let exported = json(dir, &["git", "export", "head"])?;
    let commit = exported["gitCommit"]
        .as_str()
        .ok_or_else(|| format!("export reports a git commit: {exported}"))?;
    git(dir, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// `git ls-tree -r` of `tree`: path → (mode, blob sha).
fn ls_tree(dir: &Path, tree: &str) -> TestResult<Vec<(String, String, String)>> {
    let listing = git(dir, &["ls-tree", "-r", tree])?;
    let mut out = Vec::new();
    for line in listing.lines() {
        let (meta, path) = line.split_once('\t').ok_or("ls-tree line has a tab")?;
        let mut parts = meta.split(' ');
        let mode = parts.next().ok_or("ls-tree mode")?.to_owned();
        let _kind = parts.next();
        let sha = parts.next().ok_or("ls-tree sha")?.to_owned();
        out.push((path.to_owned(), mode, sha));
    }
    Ok(out)
}

fn mode_of(listing: &[(String, String, String)], path: &str) -> Option<String> {
    listing
        .iter()
        .find(|(p, _, _)| p == path)
        .map(|(_, mode, _)| mode.clone())
}

/// `hord ws new`, returning (id, checkout path).
fn ws_new(dir: &Path) -> TestResult<(String, PathBuf)> {
    let v = json(dir, &["ws", "new"])?;
    Ok((
        v["id"].as_str().ok_or("workspace id")?.to_owned(),
        PathBuf::from(
            v["materialization"]
                .as_str()
                .ok_or("workspace materialization")?,
        ),
    ))
}

/// A git repository with an executable, a symlink to a file, a symlink to
/// a directory, and a relative directory symlink one level down (the shape
/// of hord's own `.claude/skills`), imported into hord in place.
fn setup() -> TestResult<TempDir> {
    let tmp = TempDir::new("hord-git-modes")?;
    let dir = tmp.path();
    fs::create_dir_all(dir.join("docs"))?;
    fs::create_dir_all(dir.join("tools/skills"))?;
    fs::create_dir_all(dir.join(".agent"))?;
    fs::write(dir.join("README.md"), "# fixture\n")?;
    fs::write(dir.join("docs/guide.md"), "guide\n")?;
    fs::write(dir.join("tools/skills/hord.md"), "skill\n")?;
    fs::write(dir.join("run.sh"), "#!/bin/sh\necho run\n")?;
    set_exec(&dir.join("run.sh"), true)?;
    symlink("README.md", dir.join("link-file"))?;
    symlink("docs", dir.join("link-dir"))?;
    symlink("../tools/skills", dir.join(".agent/skills"))?;
    fs::write(dir.join(".gitignore"), ".hord/\n.hord-home/\n")?;
    git(dir, &["init", "-q", "-b", "main"])?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-q", "-m", "fixture"])?;
    json(dir, &["init", "--from-git", "."])?;
    Ok(tmp)
}

#[test]
fn symlinks_and_exec_bits_survive_the_round_trip() -> TestResult {
    let tmp = setup()?;
    let dir = tmp.path();

    // Export reproduces the imported tree.
    let head_tree = git(dir, &["rev-parse", "HEAD^{tree}"])?;
    assert_eq!(export_tree(dir)?, head_tree);

    // Materialization: real symlinks, and the exec bit.
    let (ws, checkout) = ws_new(dir)?;
    assert_eq!(
        fs::read_link(checkout.join("link-file"))?,
        Path::new("README.md")
    );
    assert_eq!(fs::read_link(checkout.join("link-dir"))?, Path::new("docs"));
    assert_eq!(
        fs::read_link(checkout.join(".agent/skills"))?,
        Path::new("../tools/skills")
    );
    assert_eq!(
        fs::read_to_string(checkout.join("link-dir/guide.md"))?,
        "guide\n"
    );
    assert_eq!(
        fs::read_to_string(checkout.join(".agent/skills/hord.md"))?,
        "skill\n"
    );
    assert!(is_exec(&checkout.join("run.sh"))?);
    assert!(!is_exec(&checkout.join("README.md"))?);

    // An untouched checkout has nothing to propose.
    let status = json(dir, &["status", "-w", &ws])?;
    assert_eq!(status["ops"], serde_json::json!([]), "{status:#}");

    // Mode-only and symlink edits are read back from the workspace.
    set_exec(&checkout.join("README.md"), true)?;
    set_exec(&checkout.join("run.sh"), false)?;
    fs::remove_file(checkout.join("link-file"))?;
    symlink("docs/guide.md", checkout.join("link-file"))?;
    fs::remove_file(checkout.join("link-dir"))?;
    symlink("run.sh", checkout.join("new-link"))?;
    fs::write(checkout.join("tool.sh"), "#!/bin/sh\n")?;
    set_exec(&checkout.join("tool.sh"), true)?;
    let intent = dir.join("intent.md");
    fs::write(
        &intent,
        "---\nsummary: modes\nrefs: []\nacceptance: []\n---\n\nModes and links.\n",
    )?;
    let intent = intent.to_str().ok_or("UTF-8 intent path")?;
    let proposed = json(dir, &["propose", "-w", &ws, "--intent", intent])?;
    let change = proposed["change"].as_str().ok_or("proposed change id")?;
    json(dir, &["submit", change])?;
    let landed = json(dir, &["land", "--local"])?;
    assert_eq!(landed["head"], change, "{landed:#}");

    let tree = export_tree(dir)?;
    let listing = ls_tree(dir, &tree)?;
    assert_eq!(mode_of(&listing, "README.md").as_deref(), Some("100755"));
    assert_eq!(mode_of(&listing, "run.sh").as_deref(), Some("100644"));
    assert_eq!(mode_of(&listing, "tool.sh").as_deref(), Some("100755"));
    assert_eq!(mode_of(&listing, "link-file").as_deref(), Some("120000"));
    assert_eq!(mode_of(&listing, "new-link").as_deref(), Some("120000"));
    assert_eq!(
        mode_of(&listing, ".agent/skills").as_deref(),
        Some("120000")
    );
    assert_eq!(mode_of(&listing, "link-dir"), None);
    assert_eq!(
        mode_of(&listing, "docs/guide.md").as_deref(),
        Some("100644")
    );
    let target = git(dir, &["cat-file", "-p", &format!("{tree}:link-file")])?;
    assert_eq!(target, "docs/guide.md");

    // The landed result checks out with the new links and modes.
    let (_, after) = ws_new(dir)?;
    assert_eq!(
        fs::read_link(after.join("link-file"))?,
        Path::new("docs/guide.md")
    );
    assert_eq!(fs::read_link(after.join("new-link"))?, Path::new("run.sh"));
    assert!(fs::symlink_metadata(after.join("link-dir")).is_err());
    assert!(is_exec(&after.join("README.md"))?);
    assert!(!is_exec(&after.join("run.sh"))?);
    assert!(is_exec(&after.join("tool.sh"))?);
    Ok(())
}
