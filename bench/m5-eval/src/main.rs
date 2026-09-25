//! `hord-eval-m5`: the M5 conflict corpus (spec §12 M5, ADR 0029).
//!
//! ```text
//! hord-eval-m5 run [--harness-cmd CMD | --no-harness] [--jobs N] [--out DIR]
//! hord-eval-m5 generate [--out corpora/m5/cases]
//! hord-eval-m5 scripted --case FILE      (run by hord-replay-ref)
//! ```
//!
//! `run` builds each case's repository with its own daemon, lands the
//! first task, submits the second with the replay harness configured, and
//! lets the escalation ladder run. It grades a case resolved by replay when
//! a replay lands and both tasks' acceptance tests pass on the landed head;
//! otherwise the case must be parked with a conflict summary, and it is
//! then resolved with a signed `Arbitrate` (the round-trip). It reports the
//! share resolved by replay (target 60% with a model), attempts, and budget
//! outcomes, and writes `report.json`, `review.md`, and `review.csv` (the
//! human "sufficient to resolve" rating).
//!
//! Without `--harness-cmd`, the harness is `hord-replay-ref` running
//! `hord-eval-m5 scripted`, which plays each case's script: deterministic,
//! for CI. The run fails on any correctness failure: an invalid case, an
//! attempt past its budget that was not killed, an accepted over-budget
//! result, a failed arbitration round-trip, or (scripted) a grade the
//! script does not predict. The 60% share needs a model and is reported.
//!
//! Needs `hord` and `hord-replay-ref` beside this binary (`cargo build
//! --release -p hord-cli -p hord-replay-ref`), git, and cargo with
//! cargo-llvm-cov (the lander's verifier runs the tests, ADR 0022).

mod corpus;
mod generate;
mod report;
mod run;
mod scripted;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use run::{Config, Harness};

#[derive(Debug, Parser)]
#[command(name = "hord-eval-m5", about = "M5 conflict corpus harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the corpus.
    Run(RunArgs),
    /// Write the corpus files from the generator.
    Generate {
        /// Directory to write the case files to.
        #[arg(long, default_value = "corpora/m5/cases")]
        out: PathBuf,
    },
    /// Play one attempt of a case's script (run by hord-replay-ref).
    Scripted {
        /// The case file.
        #[arg(long)]
        case: PathBuf,
    },
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The corpus directory.
    #[arg(long)]
    corpus: Option<PathBuf>,
    /// A model command for hord-replay-ref (for example `claude -p`),
    /// instead of the scripted harness.
    #[arg(long, value_name = "CMD", conflicts_with = "no_harness")]
    harness_cmd: Option<String>,
    /// The model name to record (with --harness-cmd).
    #[arg(long)]
    model: Option<String>,
    /// No harness: every conflict is parked for arbitration.
    #[arg(long)]
    no_harness: bool,
    /// `[replay] budget` wall-clock seconds per attempt (default 10 with
    /// the scripted harness, else 600).
    #[arg(long)]
    wall_time_secs: Option<u64>,
    /// `[replay] budget` tokens per attempt.
    #[arg(long, default_value_t = 2_000_000)]
    tokens: u64,
    /// `[land] max_replay_attempts`.
    #[arg(long, default_value_t = 2)]
    max_attempts: u64,
    /// Cases run at once.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Only these case ids.
    #[arg(long)]
    only: Vec<String>,
    /// Where the reports go (and case repositories are built).
    #[arg(long, default_value = "target/m5-eval")]
    out: PathBuf,
    /// Keep each case's repository.
    #[arg(long)]
    keep: bool,
    /// The hord binary (default: beside this one).
    #[arg(long)]
    hord: Option<PathBuf>,
    /// The hord-replay-ref binary (default: beside this one).
    #[arg(long)]
    replay_ref: Option<PathBuf>,
}

fn beside_me(name: &str) -> Result<PathBuf> {
    let me = std::env::current_exe()?;
    let path = me
        .parent()
        .context("this binary has a directory")?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !path.exists() {
        bail!(
            "{} is missing: cargo build --release -p hord-cli -p hord-replay-ref",
            path.display()
        );
    }
    Ok(path)
}

fn corpus_dir(given: Option<PathBuf>) -> PathBuf {
    given.unwrap_or_else(|| {
        let local = PathBuf::from("corpora/m5/cases");
        if local.is_dir() {
            local
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpora/m5/cases")
        }
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate { out } => {
            std::fs::create_dir_all(&out)?;
            for case in generate::corpus() {
                let name = format!("{}-{}.toml", case.id, case.kind);
                std::fs::write(out.join(name), corpus::write(&case)?)?;
            }
            println!("wrote 100 cases to {}", out.display());
            Ok(())
        }
        Command::Scripted { case } => scripted::run(&case),
        Command::Run(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run(args)),
    }
}

async fn run(args: RunArgs) -> Result<()> {
    let harness = match (&args.harness_cmd, args.no_harness) {
        (Some(cmd), _) => Harness::Command(cmd.clone()),
        (None, true) => Harness::None,
        (None, false) => Harness::Scripted,
    };
    let scripted = matches!(harness, Harness::Scripted);
    std::fs::create_dir_all(&args.out)?;
    let out = args.out.canonicalize()?;
    let cfg = Arc::new(Config {
        hord: match args.hord {
            Some(path) => path,
            None => beside_me("hord")?,
        },
        replay_ref: match args.replay_ref {
            Some(path) => path,
            None => beside_me("hord-replay-ref")?,
        },
        eval: std::env::current_exe()?,
        harness,
        model: args.model,
        wall_time_secs: args
            .wall_time_secs
            .unwrap_or(if scripted { 10 } else { 600 }),
        tokens: args.tokens,
        max_attempts: args.max_attempts,
        work: out.join("cases"),
        keep: args.keep,
    });
    std::fs::create_dir_all(&cfg.work)?;
    let mut cases = corpus::load(&corpus_dir(args.corpus))?;
    if !args.only.is_empty() {
        cases.retain(|(_, c)| args.only.contains(&c.id));
    }
    let limit = Arc::new(tokio::sync::Semaphore::new(args.jobs.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, (path, case)) in cases.into_iter().enumerate() {
        let (cfg, limit) = (Arc::clone(&cfg), Arc::clone(&limit));
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await;
            let result = run::run_case(&cfg, &path, &case).await;
            eprintln!(
                "{} {:<20} {:?} ({:.0}s)",
                result.id, result.kind, result.outcome, result.seconds
            );
            (i, case, result)
        });
    }
    let mut results = Vec::new();
    while let Some(done) = tasks.join_next().await {
        results.push(done.context("a case task panicked")?);
    }
    results.sort_by_key(|(i, _, _)| *i);
    let results: Vec<_> = results.into_iter().map(|(_, c, r)| (c, r)).collect();
    let max_attempts = usize::try_from(cfg.max_attempts).unwrap_or(usize::MAX);
    let summary = report::summarize(&results, max_attempts);
    let cases_json: Vec<_> = results.iter().map(|(_, r)| r).collect();
    std::fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "summary": summary,
            "cases": cases_json,
        }))?,
    )?;
    report::write_review(&out, &results)?;
    print!("{}", report::text(&summary, false));
    println!(
        "report: {}; review sheet: {}",
        out.join("report.json").display(),
        out.join("review.md").display()
    );
    if !summary.failures.is_empty() {
        bail!("{} correctness failure(s)", summary.failures.len());
    }
    Ok(())
}
