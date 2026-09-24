//! Bare-repo reads for the cargo corpus.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

pub(crate) fn rev_list(dir: &Path) -> Result<Vec<String>> {
    let raw = git(dir, &["rev-list", "HEAD"])?;
    Ok(String::from_utf8(raw)?
        .lines()
        .map(str::to_string)
        .collect())
}

pub(crate) fn parent(dir: &Path, commit: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["rev-parse", &format!("{commit}^")])
        .output()
        .context("git rev-parse parent")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()))
}

/// `(old_path, new_path)` for modified or renamed `.rs` files.
pub(crate) fn changed_rust(
    dir: &Path,
    parent: &str,
    commit: &str,
) -> Result<Vec<(String, String)>> {
    let raw = git(
        dir,
        &[
            "diff-tree",
            "-M",
            "-r",
            "-z",
            "--name-status",
            parent,
            commit,
        ],
    )?;
    Ok(parse_name_status(&raw))
}

pub(crate) fn blob(dir: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    git(dir, &["cat-file", "blob", &format!("{commit}:{path}")])
}

/// Paths at HEAD ending in `suffix`, in `ls-tree` order.
pub(crate) fn head_files(dir: &Path, suffix: &str) -> Result<Vec<String>> {
    let raw = git(dir, &["ls-tree", "-r", "--name-only", "HEAD"])?;
    Ok(String::from_utf8(raw)?
        .lines()
        .filter(|path| path.ends_with(suffix))
        .map(str::to_string)
        .collect())
}

fn parse_name_status(raw: &[u8]) -> Vec<(String, String)> {
    let parts: Vec<&[u8]> = raw
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let status = String::from_utf8_lossy(parts[i]);
        i += 1;
        if status.starts_with('R') || status.starts_with('C') {
            if i + 1 >= parts.len() {
                break;
            }
            let old = String::from_utf8_lossy(parts[i]).into_owned();
            let new = String::from_utf8_lossy(parts[i + 1]).into_owned();
            i += 2;
            if old.ends_with(".rs") || new.ends_with(".rs") {
                out.push((old, new));
            }
        } else if let Some(path) = parts.get(i) {
            i += 1;
            if status.starts_with('M') {
                let path = String::from_utf8_lossy(path).into_owned();
                if path.ends_with(".rs") {
                    out.push((path.clone(), path));
                }
            }
        }
    }
    out
}
