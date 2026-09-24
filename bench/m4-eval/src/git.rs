//! The bare cargo corpus: first-parent history, trees, blobs, and
//! per-worker checkouts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// The last `n` first-parent commits of `HEAD` that have a parent, oldest
/// first.
pub(crate) fn first_parent_commits(dir: &Path, n: usize) -> Result<Vec<String>> {
    let out = git(
        dir,
        &[
            "rev-list",
            "--first-parent",
            "--min-parents=1",
            "-n",
            &n.to_string(),
            "HEAD",
        ],
    )?;
    let mut commits: Vec<String> = String::from_utf8(out)?.lines().map(str::to_owned).collect();
    commits.reverse();
    Ok(commits)
}

/// First parent of `commit`.
pub(crate) fn parent(dir: &Path, commit: &str) -> Result<String> {
    Ok(
        String::from_utf8(git(dir, &["rev-parse", &format!("{commit}^1")])?)?
            .trim()
            .to_owned(),
    )
}

/// Every regular file of `commit` (symlinks and submodules skipped):
/// `(path, bytes)`.
pub(crate) fn tree_files(dir: &Path, commit: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let listing = git(dir, &["ls-tree", "-r", "-z", commit])?;
    let mut entries = Vec::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record).context("ls-tree entry is not UTF-8")?;
        let (meta, path) = record.split_once('\t').context("ls-tree entry")?;
        let mut meta = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (meta.next(), meta.next(), meta.next()) else {
            bail!("ls-tree entry {record:?}");
        };
        if kind != "blob" || mode == "120000" {
            continue;
        }
        entries.push((oid.to_owned(), path.to_owned()));
    }
    let blobs = cat_batch(dir, entries.iter().map(|(oid, _)| oid.as_str()))?;
    Ok(entries
        .into_iter()
        .zip(blobs)
        .map(|((_, path), bytes)| (path, bytes))
        .collect())
}

/// One changed path of a first-parent diff: `None` content means deleted
/// (or turned into a symlink).
pub(crate) type Changed = (String, Option<Vec<u8>>);

/// Files that differ between `from` and `to`, with `to`'s contents.
pub(crate) fn diff(dir: &Path, from: &str, to: &str) -> Result<Vec<Changed>> {
    let out = git(
        dir,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--no-commit-id",
            from,
            to,
        ],
    )?;
    let mut fields = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    let mut entries = Vec::new();
    while let Some(meta) = fields.next() {
        let meta = std::str::from_utf8(meta)?;
        let path = std::str::from_utf8(fields.next().context("diff-tree path")?)?.to_owned();
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (new_mode, new_oid, status) = (parts[1], parts[3], parts[4]);
        if status.starts_with('D') || new_mode == "120000" || new_mode == "160000" {
            entries.push((path, None));
        } else {
            entries.push((path, Some(new_oid.to_owned())));
        }
    }
    let wanted: Vec<&str> = entries.iter().filter_map(|(_, o)| o.as_deref()).collect();
    let mut blobs = cat_batch(dir, wanted.into_iter())?.into_iter();
    Ok(entries
        .into_iter()
        .map(|(path, oid)| (path, oid.map(|_| blobs.next().unwrap_or_default())))
        .collect())
}

/// Contents of `oids`, in order, from one `git cat-file --batch`.
fn cat_batch<'a>(dir: &Path, oids: impl Iterator<Item = &'a str>) -> Result<Vec<Vec<u8>>> {
    let mut input = String::new();
    for oid in oids {
        input.push_str(oid);
        input.push('\n');
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("git cat-file --batch")?;
    let mut stdin = child.stdin.take().context("cat-file stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().context("git cat-file --batch")?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("cat-file writer panicked"))??;
    if !output.status.success() {
        bail!("git cat-file --batch failed");
    }
    let mut out = Vec::new();
    let mut rest = output.stdout.as_slice();
    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .context("cat-file header")?;
        let header = std::str::from_utf8(&rest[..nl])?;
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("cat-file header {header:?}"))?;
        let body = &rest[nl + 1..nl + 1 + size];
        out.push(body.to_vec());
        rest = &rest[nl + 1 + size + 1..];
    }
    Ok(out)
}

/// A worker's own checkout of the corpus (a shared clone).
pub(crate) struct Checkout {
    pub root: PathBuf,
}

impl Checkout {
    /// Clone `corpus` into `root` once, without checking anything out.
    pub(crate) fn open(corpus: &Path, root: &Path) -> Result<Self> {
        if !root.join(".git").exists() {
            if let Some(parent) = root.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let status = Command::new("git")
                .args(["clone", "-q", "--shared", "--no-checkout"])
                .arg(corpus)
                .arg(root)
                .status()?;
            if !status.success() {
                bail!("git clone {}", corpus.display());
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()?;
        if !out.status.success() {
            bail!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Check out `commit` exactly, dropping local edits and untracked files.
    pub(crate) fn checkout(&self, commit: &str) -> Result<()> {
        self.run(&["checkout", "-q", "-f", "--detach", commit])?;
        self.run(&["clean", "-q", "-ffdx"])
    }

    /// Drop local edits to `path`.
    pub(crate) fn restore(&self, path: &str) -> Result<()> {
        self.run(&["checkout", "-q", "--", path])
    }
}
