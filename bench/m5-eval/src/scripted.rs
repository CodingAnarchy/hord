//! `hord-eval-m5 scripted --case <file>`: the deterministic stand-in for a
//! model, which `hord-replay-ref --cmd` runs in the replay workspace (its
//! working directory) on each attempt. It plays the case's [`Step`] for
//! `HORD_REPLAY_ATTEMPT` and reports usage in `HORD_REPLAY_USAGE`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::corpus::{self, Step};

/// Tokens a normal scripted attempt reports.
const TOKENS: u64 = 1_000;
/// Tokens an over-budget attempt reports: more than any budget allows.
const TOO_MANY_TOKENS: u64 = 1_000_000_000_000;
/// How long a sleeping attempt sleeps: past any budget.
const SLEEP: Duration = Duration::from_secs(3_600);

fn write_files(files: &BTreeMap<String, String>) -> Result<()> {
    for (path, text) in files {
        let path = Path::new(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn report_usage(tokens: u64) -> Result<()> {
    if let Ok(path) = std::env::var("HORD_REPLAY_USAGE") {
        std::fs::write(
            path,
            format!("{{\"tokens\": {tokens}, \"model\": \"scripted\"}}"),
        )?;
    }
    Ok(())
}

/// Play attempt `HORD_REPLAY_ATTEMPT` of `case_path` in the current
/// directory.
pub fn run(case_path: &Path) -> Result<()> {
    let case = corpus::read(case_path)?;
    let attempt: usize = std::env::var("HORD_REPLAY_ATTEMPT")
        .context("HORD_REPLAY_ATTEMPT is not set: run under hord-replay-ref")?
        .parse()
        .context("HORD_REPLAY_ATTEMPT")?;
    let resolution = || {
        case.resolution
            .as_ref()
            .with_context(|| format!("{} has no resolution", case.id))
    };
    match case.step(attempt) {
        Step::Resolve => {
            write_files(resolution()?)?;
            report_usage(TOKENS)
        }
        Step::OverBudget => {
            write_files(resolution()?)?;
            report_usage(TOO_MANY_TOKENS)
        }
        Step::Wrong => {
            write_files(&case.second().writes())?;
            report_usage(TOKENS)
        }
        Step::Sleep => {
            std::thread::sleep(SLEEP);
            bail!("slept through the budget without being killed")
        }
        Step::GiveUp => {
            report_usage(TOKENS)?;
            bail!("the script gives up on attempt {attempt}")
        }
    }
}
