//! Cargo HEAD from the bare corpus clone.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Every regular file at HEAD. Symlinks are skipped: `Repo::bootstrap`
/// takes bytes only and has no file modes.
pub(crate) struct Corpus {
    pub commit: String,
    pub files: Vec<(String, Vec<u8>)>,
    pub symlinks: usize,
}

impl Corpus {
    /// The files whose paths are repository paths, for `Repo::bootstrap`.
    pub(crate) fn repo_files(&self) -> Vec<(hord_core::RepoPath, Vec<u8>)> {
        self.files
            .iter()
            .filter_map(|(p, b)| p.parse().ok().map(|p| (p, b.clone())))
            .collect()
    }

    pub(crate) fn file(&self, path: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, bytes)| bytes.as_slice())
    }
}

pub(crate) fn load_head(git_dir: &Path) -> Result<Corpus> {
    let commit = String::from_utf8(git(git_dir, &["rev-parse", "HEAD"])?)?
        .trim()
        .to_string();
    let listing = git(git_dir, &["ls-tree", "-r", "-z", "HEAD"])?;
    let mut entries = Vec::new();
    let mut symlinks = 0;
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record).context("ls-tree entry is not UTF-8")?;
        let (meta, path) = record.split_once('\t').context("ls-tree entry")?;
        let mut meta = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (meta.next(), meta.next(), meta.next()) else {
            bail!("ls-tree entry {record:?}");
        };
        if kind != "blob" {
            continue;
        }
        if mode == "120000" {
            symlinks += 1;
            continue;
        }
        entries.push((oid.to_string(), path.to_string()));
    }
    let blobs = cat_batch(git_dir, entries.iter().map(|(oid, _)| oid.as_str()))?;
    let files = entries
        .into_iter()
        .zip(blobs)
        .map(|((_, path), bytes)| (path, bytes))
        .collect();
    Ok(Corpus {
        commit,
        files,
        symlinks,
    })
}

/// Contents of `oids`, in order, from one `git cat-file --batch`.
fn cat_batch<'a>(git_dir: &Path, oids: impl Iterator<Item = &'a str>) -> Result<Vec<Vec<u8>>> {
    let mut input = String::new();
    for oid in oids {
        input.push_str(oid);
        input.push('\n');
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
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
        .map_err(|_| anyhow::anyhow!("cat-file writer panicked"))?
        .context("write cat-file stdin")?;
    if !output.status.success() {
        bail!(
            "git cat-file --batch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let raw = output.stdout;
    let mut out = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let end = raw[at..]
            .iter()
            .position(|b| *b == b'\n')
            .context("cat-file header")?
            + at;
        let header = std::str::from_utf8(&raw[at..end])?;
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("cat-file header {header:?}"))?;
        let start = end + 1;
        out.push(raw[start..start + size].to_vec());
        at = start + size + 1;
    }
    Ok(out)
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
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

pub(crate) fn cache_dir(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }
    if let Ok(dir) = std::env::var("HORD_CORPORA") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME")?;
    Ok(PathBuf::from(home).join(".cache/hord/corpora"))
}
