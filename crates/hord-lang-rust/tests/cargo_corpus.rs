//! Optional corpus check: every `.rs` blob at HEAD of the cargo git mirror.

use std::path::{Path, PathBuf};
use std::process::Command;

use hord_lang::LangAdapter;
use hord_lang_rust::RustAdapter;

fn cargo_git_dir() -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME");
    PathBuf::from(home).join(".cache/hord/corpora/cargo.git")
}

fn git(git_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}

/// Parse every `.rs` file at HEAD of `~/.cache/hord/corpora/cargo.git`.
///
/// Does not clone. Run with `cargo test -p hord-lang-rust -- --ignored`.
#[test]
#[ignore = "requires ~/.cache/hord/corpora/cargo.git"]
fn cargo_head_rs_blobs_are_lossless() -> Result<(), Box<dyn std::error::Error>> {
    let git_dir = cargo_git_dir();
    assert!(
        git_dir.exists(),
        "missing cargo corpus at {}",
        git_dir.display()
    );

    let listing = git(&git_dir, &["ls-tree", "-r", "-z", "HEAD"])?;
    let adapter = RustAdapter;
    let mut checked = 0usize;
    for entry in listing.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        // `100644 blob <sha>\t<path>`
        let entry = std::str::from_utf8(entry).expect("ls-tree utf-8");
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if !path.ends_with(".rs") {
            continue;
        }
        let sha = meta
            .split_whitespace()
            .nth(2)
            .ok_or_else(|| format!("bad ls-tree line: {entry}"))?;
        let bytes = git(&git_dir, &["cat-file", "blob", sha])?;
        let tree = adapter
            .parse(&bytes)
            .map_err(|e| format!("parse {path} ({sha}): {e}"))?;
        let projected = adapter.project(&tree);
        assert_eq!(projected.as_slice(), bytes.as_slice(), "lossless {path}");
        checked += 1;
    }
    assert!(checked > 0, "no .rs blobs at HEAD of {}", git_dir.display());
    Ok(())
}
