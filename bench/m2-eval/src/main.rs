//! M2 validation (spec §12).
//!
//! - Identity: 500 function pairs from consecutive cargo commits. A pair is
//!   "the same function" when its qualified name is unchanged. Gate: ≥ 97%
//!   keep their `NodeId`. Rename precision is separate: ≥ 95% of hord
//!   function renames still share half their CST-leaf tokens. That labeler
//!   is not ADR 0007's tree-edit distance.
//! - References: 300 functions at cargo HEAD. Expected names are unique
//!   definition simple-names that occur as identifier, type, or field
//!   leaves in the body. Gate: hord recall ≥ 95%. This is a lexical
//!   stand-in. The spec's rust-analyzer comparison is reported separately
//!   and is not measured unless that binary runs.
//! - Blame: warm `Store::node_history` lookups over cargo's `src/` and
//!   `crates/` definitions. Gate: under 50 ms.
//!
//! ```text
//! cargo run -p hord-eval-m2 --release --offline -- --skip-rust-analyzer
//! ```

#![forbid(unsafe_code)]

mod git;
mod identity;
mod snapshot;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;

const IDENTITY_SAMPLE: usize = 500;
const REFERENCE_SAMPLE: usize = 300;
const MAX_COMMITS: usize = 400;

#[derive(Debug, Parser)]
#[command(name = "hord-eval-m2", about = "M2 validation (spec §12)")]
struct Args {
    /// Bare corpus directory (default: $HORD_CORPORA or ~/.cache/hord/corpora).
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Do not run the rust-analyzer reference oracle.
    #[arg(long)]
    skip_rust_analyzer: bool,
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(&args) {
        eprintln!("m2 eval error: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    let git_dir = cache_dir(args.cache.as_deref())?.join("cargo.git");
    if !git_dir.is_dir() {
        anyhow::bail!("missing cargo corpus at {}", git_dir.display());
    }

    let identity = identity::measure(&git_dir, IDENTITY_SAMPLE, MAX_COMMITS)?;
    let identity_pass = identity::identity_ok(&identity, IDENTITY_SAMPLE);
    println!(
        "[identity] commits {} labeled {} stable {}/{} rename-precision {}/{} {}",
        identity.commits,
        identity.labeled,
        identity.stable,
        identity.labeled,
        identity.precise_renames,
        identity.hord_renames,
        pass_fail(identity_pass)
    );

    eprintln!("[snapshot] parsing cargo src/ and crates/");
    let files = snapshot::load_head(&git_dir)?;
    let references = snapshot::references(&files, REFERENCE_SAMPLE);
    let lexical_pass = snapshot::references_ok(&references, REFERENCE_SAMPLE);
    println!(
        "[references] oracle lexical labeled {} recall {}/{} {} (stand-in for rust-analyzer)",
        references.labeled,
        references.hit,
        references.expected,
        pass_fail(lexical_pass)
    );

    if args.skip_rust_analyzer || !rust_analyzer_runs() {
        println!(
            "[references-ra] not measured (spec oracle is rust-analyzer find-references; binary unavailable or skipped)"
        );
    } else {
        println!(
            "[references-ra] binary present; find-references sample is not wired yet, not a gate"
        );
    }

    let blame = snapshot::blame(&files)?;
    let blame_pass = snapshot::blame_ok(&blame);
    println!(
        "[blame] defs {} warm-max {:.3} ms {}",
        blame.defs,
        blame.max_ms,
        pass_fail(blame_pass)
    );

    let ok = identity_pass && lexical_pass && blame_pass;
    println!("m2 {}", pass_fail(ok));
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn rust_analyzer_runs() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn pass_fail(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

fn cache_dir(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }
    if let Ok(dir) = std::env::var("HORD_CORPORA") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME")?;
    Ok(PathBuf::from(home).join(".cache/hord/corpora"))
}
