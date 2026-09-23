//! `Cargo.lock` concurrent dependency additions (spec §12 M3).
//!
//! From cargo's own `Cargo.lock` at HEAD, two InMemory workspaces on the same
//! base each add a different new registry dependency: a `"name"` line in the
//! root `cargo` package's `dependencies` and a `[[package]]` entry. Both
//! submit; `land_local` drains. Pass: both land and the head `Cargo.lock` is
//! byte-equal to the file with both additions in Cargo's canonical order
//! (packages by name, dependency lines by name), which is what `cargo` would
//! write. Two cases: names that sort far apart, and names that sort into the
//! same gap (adjacent lines in both places).

use std::path::Path;

use anyhow::{Context, Result, bail};
use hord_core::{ChangeId, RepoPath};
use hord_txn::{BeginOptions, QueueStatus, Repo, RepoOptions};
use serde::Serialize;

use crate::sim::{actor, intent, status_name};

const ROOT: &str = "[[package]]\nname = \"cargo\"\nversion = ";
const PACKAGE: &str = "[[package]]\nname = \"";
const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[derive(Debug, Serialize)]
pub(crate) struct LockCase {
    pub case: &'static str,
    pub deps: [&'static str; 2],
    pub statuses: Vec<String>,
    pub conflicts: Vec<String>,
    pub head_matches: bool,
    pub first_difference: Option<String>,
    pub pass: bool,
}

/// Deterministic, well-formed stand-in checksum for a made-up package.
fn checksum(name: &str) -> String {
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = String::new();
    for round in 0..4u64 {
        for byte in name.bytes().chain(round.to_le_bytes()) {
            a ^= u64::from(byte);
            a = a.wrapping_mul(0x0100_0000_01b3);
        }
        out.push_str(&format!("{a:016x}"));
    }
    out
}

/// `lock` with `name 1.0.0` added the way Cargo would write it.
pub(crate) fn add_dependency(lock: &str, name: &str) -> Result<String> {
    // Root package's dependency list.
    let root = lock.find(ROOT).context("no root `cargo` package")?;
    let list = root
        + lock[root..]
            .find("dependencies = [\n")
            .context("root package has no dependencies")?
        + "dependencies = [\n".len();
    let mut at = list;
    loop {
        let end = lock[at..].find('\n').context("unterminated dependencies")? + at;
        let line = &lock[at..end];
        if line == "]" {
            break;
        }
        let dep = line.trim().trim_end_matches(',').trim_matches('"');
        let dep_name = dep.split(' ').next().unwrap_or(dep);
        if dep_name == name {
            bail!("{name} already a dependency");
        }
        if dep_name > name {
            break;
        }
        at = end + 1;
    }
    let mut out = String::with_capacity(lock.len() + 256);
    out.push_str(&lock[..at]);
    out.push_str(&format!(" \"{name}\",\n"));
    out.push_str(&lock[at..]);

    // `[[package]]` in name order.
    let block = format!(
        "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"{REGISTRY}\"\nchecksum = \"{}\"\n",
        checksum(name)
    );
    let mut insert = None;
    let mut search = 0;
    while let Some(found) = out[search..].find(PACKAGE) {
        let start = search + found;
        let name_start = start + PACKAGE.len();
        let name_end = out[name_start..].find('"').context("package name")? + name_start;
        let existing = &out[name_start..name_end];
        if existing == name {
            bail!("package {name} already locked");
        }
        if existing > name {
            insert = Some(start);
            break;
        }
        search = name_end;
    }
    Ok(match insert {
        Some(start) => format!("{}{block}\n{}", &out[..start], &out[start..]),
        None => format!("{out}\n{block}"),
    })
}

async fn add_in_workspace(repo: Repo, path: RepoPath, name: &'static str) -> Result<ChangeId> {
    let mut ws = repo
        .begin(BeginOptions::at_head(actor(&format!("m3-lock-{name}"))))
        .await?;
    let current = ws.read_file(&path).await?.context("no Cargo.lock")?;
    let text = std::str::from_utf8(current.as_slice())?;
    ws.write_file(&path, add_dependency(text, name)?).await?;
    let proposal = ws
        .propose(intent(&format!("add dependency {name}")))
        .await?;
    repo.submit(proposal.change).await?;
    Ok(proposal.change)
}

fn first_difference(got: &str, want: &str) -> Option<String> {
    let (got_lines, want_lines): (Vec<_>, Vec<_>) = (got.lines().collect(), want.lines().collect());
    for i in 0..got_lines.len().max(want_lines.len()) {
        let (g, w) = (got_lines.get(i), want_lines.get(i));
        if g != w {
            return Some(format!(
                "line {}: got {:?}, want {:?}",
                i + 1,
                g.copied().unwrap_or("<eof>"),
                w.copied().unwrap_or("<eof>")
            ));
        }
    }
    (got != want).then(|| "trailing bytes differ".to_string())
}

async fn case(
    dir: &Path,
    label: &'static str,
    deps: [&'static str; 2],
    manifest: &[u8],
    lock: &str,
) -> Result<LockCase> {
    let store = hord_store::Store::create(dir.join(label)).context("create store")?;
    let repo = Repo::from_store(store, RepoOptions::default()).await?;
    let lock_path: RepoPath = "Cargo.lock".parse()?;
    repo.bootstrap(
        vec![
            ("Cargo.toml".parse()?, manifest.to_vec()),
            (lock_path.clone(), lock.as_bytes().to_vec()),
        ],
        intent("import cargo Cargo.toml and Cargo.lock"),
        actor("m3-lock-seed"),
    )
    .await?;

    let want = add_dependency(&add_dependency(lock, deps[0])?, deps[1])?;
    if want != add_dependency(&add_dependency(lock, deps[1])?, deps[0])? {
        bail!("expected lockfile depends on insertion order");
    }

    let (a, b) = tokio::join!(
        tokio::spawn(add_in_workspace(repo.clone(), lock_path.clone(), deps[0])),
        tokio::spawn(add_in_workspace(repo.clone(), lock_path.clone(), deps[1])),
    );
    let ids = [a.context("lock task")??, b.context("lock task")??];
    let done = repo.land_local().await?;
    let mut statuses = Vec::new();
    let mut conflicts = Vec::new();
    let mut all_landed = true;
    for id in ids {
        let entry = done
            .iter()
            .find(|e| e.change == id)
            .context("change not processed")?;
        all_landed &= matches!(entry.status, QueueStatus::Landed { .. });
        statuses.push(status_name(&entry.status));
        if let Some(report) = &entry.report {
            conflicts.extend(report.conflicts.iter().map(|c| format!("set {:?}", c.kind)));
            conflicts.extend(
                report
                    .merge
                    .iter()
                    .map(|m| format!("merge {:?}: {}", m.severity, m.reason)),
            );
        }
    }
    let mut ws = repo
        .begin(BeginOptions::at_head(actor("m3-lock-check")))
        .await?;
    let head = ws
        .read_file(&lock_path)
        .await?
        .context("no Cargo.lock at head")?;
    let got = String::from_utf8_lossy(head.as_slice()).into_owned();
    let head_matches = got == want;
    Ok(LockCase {
        case: label,
        deps,
        statuses,
        conflicts,
        head_matches,
        first_difference: first_difference(&got, &want),
        pass: all_landed && head_matches,
    })
}

pub(crate) async fn run(dir: &Path, manifest: &[u8], lock: &[u8]) -> Result<Vec<LockCase>> {
    let lock = std::str::from_utf8(lock).context("Cargo.lock is not UTF-8")?;
    Ok(vec![
        case(dir, "far", ["aaa-hord-sim", "zzz-hord-sim"], manifest, lock).await?,
        case(
            dir,
            "adjacent",
            ["hord-sim-a", "hord-sim-b"],
            manifest,
            lock,
        )
        .await?,
    ])
}
