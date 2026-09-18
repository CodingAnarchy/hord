//! M0 git round-trip evaluation (spec §12).
//!
//! Checks `export(import(repo))` tree SHAs for:
//! - this hord repository
//! - `rust-lang/cargo` (full history)
//! - `tokio-rs/tokio` (last 2,000 commits)
//!
//! Also reports cargo import throughput (target ≥ 200 commits/s).
//!
//! Corpora are bare-cloned into `$HORD_CORPORA` or `~/.cache/hord/corpora`.
//!
//! ```text
//! cargo run -p hord-eval --release
//! cargo run -p hord-eval --release -- --only hord
//! ```

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use hord_core::{ChangeRecord, IntentRef};
use hord_git::{ExportCache, Store as GitStore, git_tree_sha, import_git, import_git_window};
use hord_store::Store;
use serde::Serialize;

const CARGO_URL: &str = "https://github.com/rust-lang/cargo.git";
const TOKIO_URL: &str = "https://github.com/tokio-rs/tokio.git";
const TOKIO_WINDOW: usize = 2_000;
const CARGO_THROUGHPUT_TARGET: f64 = 200.0;

#[derive(Debug, Parser)]
#[command(name = "hord-eval", about = "M0 git round-trip evaluation (spec §12)")]
struct Args {
    /// Directory for cloned corpora (default: $HORD_CORPORA or ~/.cache/hord/corpora).
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Run only this corpus: `hord`, `cargo`, or `tokio`.
    #[arg(long)]
    only: Option<String>,
    /// Do not clone; fail if a corpus is missing from the cache.
    #[arg(long)]
    offline: bool,
    /// Emit a JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct Report {
    cases: Vec<CaseReport>,
    all_passed: bool,
}

#[derive(Debug, Serialize)]
struct CaseReport {
    name: String,
    git_path: String,
    commits: u64,
    import_secs: f64,
    check_secs: f64,
    commits_per_sec: f64,
    tree_mismatches: u64,
    throughput_ok: Option<bool>,
    passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn main() {
    let args = Args::parse();
    match run(&args) {
        Ok(report) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            }
            std::process::exit(if report.all_passed { 0 } else { 1 });
        }
        Err(err) => {
            eprintln!("hord-eval: {err:#}");
            std::process::exit(2);
        }
    }
}

fn run(args: &Args) -> Result<Report> {
    let cache = args.cache.clone().unwrap_or_else(default_cache);
    fs::create_dir_all(&cache).with_context(|| format!("create cache {}", cache.display()))?;

    let mut names = vec!["hord", "cargo", "tokio"];
    if let Some(only) = &args.only {
        if !names.contains(&only.as_str()) {
            bail!("unknown corpus {only:?}; expected hord, cargo, or tokio");
        }
        names = vec![only.as_str()];
    }

    let mut cases = Vec::new();
    for name in names {
        let case = match name {
            "hord" => eval_hord(),
            "cargo" => eval_remote(
                "cargo",
                CARGO_URL,
                &cache.join("cargo.git"),
                None,
                Some(CARGO_THROUGHPUT_TARGET),
                args.offline,
            ),
            "tokio" => eval_remote(
                "tokio",
                TOKIO_URL,
                &cache.join("tokio.git"),
                Some(TOKIO_WINDOW),
                None,
                args.offline,
            ),
            _ => unreachable!(),
        };
        cases.push(case);
    }

    let all_passed = cases.iter().all(|c| c.passed);
    let report = Report { cases, all_passed };
    if !args.json {
        print_human(&report);
    }
    Ok(report)
}

fn default_cache() -> PathBuf {
    if let Ok(dir) = std::env::var("HORD_CORPORA") {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache/hord/corpora");
    }
    std::env::temp_dir().join("hord-corpora")
}

fn eval_hord() -> CaseReport {
    match find_hord_git() {
        Ok(path) => eval_git("hord", &path, None, None),
        Err(err) => CaseReport {
            name: "hord".into(),
            git_path: String::new(),
            commits: 0,
            import_secs: 0.0,
            check_secs: 0.0,
            commits_per_sec: 0.0,
            tree_mismatches: 0,
            throughput_ok: None,
            passed: false,
            error: Some(err.to_string()),
        },
    }
}

fn eval_remote(
    name: &str,
    url: &str,
    dest: &Path,
    window: Option<usize>,
    throughput_target: Option<f64>,
    offline: bool,
) -> CaseReport {
    if let Err(err) = ensure_bare_clone(url, dest, offline) {
        return CaseReport {
            name: name.into(),
            git_path: dest.display().to_string(),
            commits: 0,
            import_secs: 0.0,
            check_secs: 0.0,
            commits_per_sec: 0.0,
            tree_mismatches: 0,
            throughput_ok: None,
            passed: false,
            error: Some(err.to_string()),
        };
    }
    eval_git(name, dest, window, throughput_target)
}

fn eval_git(
    name: &str,
    git_path: &Path,
    window: Option<usize>,
    throughput_target: Option<f64>,
) -> CaseReport {
    let git_path_disp = git_path.display().to_string();
    match eval_git_inner(name, git_path, window) {
        Ok(mut report) => {
            if let Some(target) = throughput_target {
                report.throughput_ok = Some(report.commits_per_sec >= target);
                if report.throughput_ok != Some(true) {
                    report.passed = false;
                }
            }
            report
        }
        Err(err) => CaseReport {
            name: name.into(),
            git_path: git_path_disp,
            commits: 0,
            import_secs: 0.0,
            check_secs: 0.0,
            commits_per_sec: 0.0,
            tree_mismatches: 0,
            throughput_ok: None,
            passed: false,
            error: Some(format!("{err:#}")),
        },
    }
}

fn eval_git_inner(name: &str, git_path: &Path, window: Option<usize>) -> Result<CaseReport> {
    let work = scratch(&format!("{name}-store"));
    let mut store = Store::create(&work).context("create eval store")?;
    let git_path_disp = git_path.display().to_string();

    eprintln!("[{name}] importing {}", git_path.display());
    let start = Instant::now();
    match window {
        Some(max) => import_git_window(&mut store, git_path, "HEAD", max)?,
        None => import_git(&mut store, git_path)?,
    };
    let import_secs = start.elapsed().as_secs_f64().max(1e-9);
    let log = hord_store::Store::log(&store)?;
    let commits = log.len() as u64;
    let commits_per_sec = commits as f64 / import_secs;
    eprintln!("[{name}] imported {commits} commits in {import_secs:.2}s ({commits_per_sec:.1}/s)");

    let src = open_git(git_path)?;
    let hash = src.object_hash();
    let mut cache = ExportCache::default();
    let mut mismatches = 0u64;
    let check_start = Instant::now();
    for (i, change_id) in log.iter().enumerate() {
        let change: ChangeRecord = GitStore::get_object(&store, *change_id)?;
        let git_sha = change
            .intent
            .refs
            .iter()
            .find_map(|r| match r {
                IntentRef::GitCommit { sha } => Some(sha.as_str()),
                _ => None,
            })
            .context("imported change missing GitCommit ref")?;
        let expected = git_tree_oid(&src, git_sha)?;
        let exported = git_tree_sha(&store, change.result, hash, &mut cache)?;
        if exported.as_gix() != expected {
            mismatches += 1;
            eprintln!(
                "[{name}] tree SHA mismatch commit {git_sha}: expected {}, got {}",
                expected.to_hex(),
                exported.to_hex()
            );
        }
        if (i + 1) % 2000 == 0 {
            eprintln!("[{name}] checked {}/{commits} trees", i + 1);
        }
    }
    let check_secs = check_start.elapsed().as_secs_f64();
    eprintln!(
        "[{name}] checked {commits} trees in {check_secs:.2}s ({:.1}/s)",
        commits as f64 / check_secs.max(1e-9)
    );

    let passed = mismatches == 0;
    Ok(CaseReport {
        name: name.into(),
        git_path: git_path_disp,
        commits,
        import_secs,
        check_secs,
        commits_per_sec,
        tree_mismatches: mismatches,
        throughput_ok: None,
        passed,
        error: None,
    })
}

fn git_tree_oid(repo: &gix::Repository, commit_sha: &str) -> Result<gix::ObjectId> {
    let oid = gix::ObjectId::from_hex(commit_sha.as_bytes())
        .with_context(|| format!("parse git sha {commit_sha}"))?;
    let commit = repo.find_commit(oid).context("find source commit")?;
    Ok(commit.tree_id().context("source tree")?.detach())
}

fn open_git(path: &Path) -> Result<gix::Repository> {
    let mut repo = gix::open_opts(path, gix::open::Options::isolated())
        .with_context(|| format!("open git {}", path.display()))?;
    repo.object_cache_size_if_unset(4 * 1024 * 1024);
    Ok(repo)
}

fn ensure_bare_clone(url: &str, dest: &Path, offline: bool) -> Result<()> {
    if dest.join("HEAD").exists() {
        return Ok(());
    }
    if offline {
        bail!("corpus missing at {} (offline)", dest.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    eprintln!("cloning {url} -> {}", dest.display());
    let status = Command::new("git")
        .args(["clone", "--bare", url])
        .arg(dest)
        .status()
        .context("git clone")?;
    if !status.success() {
        bail!("git clone {url} failed with {status}");
    }
    Ok(())
}

fn find_hord_git() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HORD_SELF") {
        return Ok(PathBuf::from(dir));
    }
    let cwd = std::env::current_dir()?;
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output()
        .context("git rev-parse")?;
    if !output.status.success() {
        bail!("not inside a git work tree (set HORD_SELF)");
    }
    let path = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok(PathBuf::from(path))
}

fn scratch(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "hord-eval-{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("scratch dir");
    path
}

fn print_human(report: &Report) {
    println!(
        "{:<8} {:>8} {:>10} {:>10} {:>10} {:>12} result",
        "corpus", "commits", "import-s", "check-s", "comm/s", "mismatches"
    );
    for case in &report.cases {
        let result = if let Some(err) = &case.error {
            format!("FAIL ({err})")
        } else if !case.passed {
            if case.throughput_ok == Some(false) {
                "FAIL (throughput)".into()
            } else {
                "FAIL (tree SHA)".into()
            }
        } else {
            "PASS".into()
        };
        println!(
            "{:<8} {:>8} {:>10.2} {:>10.2} {:>10.1} {:>12} {result}",
            case.name,
            case.commits,
            case.import_secs,
            case.check_secs,
            case.commits_per_sec,
            case.tree_mismatches
        );
    }
    println!(
        "{}",
        if report.all_passed {
            "M0 round-trip evaluation: PASS"
        } else {
            "M0 round-trip evaluation: FAIL"
        }
    );
}
