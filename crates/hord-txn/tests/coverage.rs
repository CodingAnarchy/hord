//! Fresh per-test coverage at landing (ADR 0022, amendments after the
//! 50-commit measurement), with the default cargo verifier on a small real
//! crate: every verification run is instrumented, its per-test records are
//! merged into the newest record and stored on the landed snapshot, and the
//! next landing selects from them.

mod common;

use std::collections::BTreeSet;
use std::env;
use std::process::Command;

use common::*;
use hord_core::{Evidence, EvidenceKind, SnapshotId};
use hord_txn::{QueueStatus, Repo, RepoOptions};
use hord_verify::{COVERAGE_KIND, CoverageRecord, RUNS_PER_RECORD, get_evidence};
use hord_verify_rust::detect_toolchain;

const LIB: &str = "\
mod other;

pub use other::beta;

pub fn alpha() -> u32 {
    1
}

pub fn gamma() -> u32 {
    alpha() + 10
}

#[cfg(test)]
mod tests {
    #[test]
    fn t_alpha() {
        assert_eq!(super::alpha(), 1);
    }

    #[test]
    fn t_beta() {
        assert_eq!(super::beta(), 2);
    }

    #[test]
    fn t_gamma() {
        assert_eq!(super::gamma(), 11);
    }
}
";

const OTHER: &str = "\
pub fn beta() -> u32 {
    2
}
";

const CARGO_TOML: &str =
    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";

/// Tests are required for `src/lib.rs` and `Cargo.toml` only: a change to
/// `src/other.rs` lands unverified, so coverage drifts.
const POLICY: &str = "\
[[rule]]
name = \"lib and manifest are tested\"
when = { paths = [\"src/lib.rs\", \"Cargo.toml\"] }
require = [\"test:selected\"]
";

fn llvm_cov_installed() -> bool {
    Command::new("cargo")
        .args(["llvm-cov", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Replace `from` with `to` in `file` at head, land it, and return the
/// landed snapshot.
async fn land(repo: &Repo, file: &str, from: &str, to: &str) -> TestResult<SnapshotId> {
    let mut ws = begin(repo, "dev").await?;
    let text = String::from_utf8(
        ws.read_file(&path(file))
            .await?
            .ok_or_else(|| format!("{file} at head"))?
            .as_slice()
            .to_vec(),
    )?;
    edit(&mut ws, file, &text, from, to).await?;
    let proposal = ws.propose(intent(&format!("{file}: {to}"))).await?;
    repo.submit(proposal.change).await?;
    let entry = repo
        .land_local()
        .await?
        .into_iter()
        .find(|e| e.change == proposal.change)
        .ok_or("change processed")?;
    assert_eq!(
        entry.status,
        QueueStatus::Landed {
            landed: proposal.change
        },
        "{entry:#?}"
    );
    Ok(proposal.record.result)
}

/// Every piece of evidence indexed on `snapshot`.
fn evidence_on(repo: &Repo, snapshot: SnapshotId) -> TestResult<Vec<Evidence>> {
    let store = repo.store();
    Ok(store
        .evidence_at(snapshot)?
        .into_iter()
        .map(|id| get_evidence(store, id))
        .collect::<Result<_, _>>()?)
}

/// The coverage record stored on `snapshot`, if any.
fn coverage_at(repo: &Repo, snapshot: SnapshotId) -> TestResult<Option<CoverageRecord>> {
    let Some(ev) = evidence_on(repo, snapshot)?
        .into_iter()
        .find(|ev| ev.kind == EvidenceKind::Custom(COVERAGE_KIND.into()))
    else {
        return Ok(None);
    };
    let log = ev.log.ok_or("coverage evidence has a log")?;
    Ok(Some(CoverageRecord::get(repo.store(), log)?))
}

/// The coverage record stored on `snapshot`, which must exist.
fn coverage_on(repo: &Repo, snapshot: SnapshotId) -> TestResult<CoverageRecord> {
    Ok(coverage_at(repo, snapshot)?.ok_or("coverage stored on the landed snapshot")?)
}

/// The tests with a run on `snapshot`: the ones that landing ran.
fn ran_on(record: &CoverageRecord, snapshot: SnapshotId) -> BTreeSet<String> {
    record
        .tests
        .iter()
        .filter(|t| record.runs(t).iter().any(|(s, _)| *s == snapshot))
        .map(|t| {
            t.test
                .name
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// The instrumented test evidence's qualifier on `snapshot` (`selected` or
/// `full`).
fn test_qualifier(repo: &Repo, snapshot: SnapshotId) -> TestResult<Option<String>> {
    Ok(evidence_on(repo, snapshot)?
        .into_iter()
        .find(|ev| ev.kind == EvidenceKind::Test && ev.command.starts_with("hord-coverage"))
        .and_then(|ev| ev.qualifier))
}

fn names(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// (1) coverage is refreshed after a landing; (2) the next change selects
/// only the tests covering what it wrote; (3) a test whose recorded
/// function changed since its run (a landing that ran no tests) is selected
/// by drift; (4) a fallback run (no record yet, then a manifest edit)
/// refreshes every test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn landings_refresh_coverage_and_select_from_it() -> TestResult {
    if detect_toolchain(&env::temp_dir()).is_err() || !llvm_cov_installed() {
        eprintln!("no Rust toolchain or cargo-llvm-cov; skipped");
        return Ok(());
    }
    let files = [
        ("Cargo.toml", CARGO_TOML),
        ("src/lib.rs", LIB),
        ("src/other.rs", OTHER),
        (".hord-policy.toml", POLICY),
    ];
    let t = repo_with(&files, RepoOptions::default()).await?;
    let all = names(&["t_alpha", "t_beta", "t_gamma"]);

    // No record yet: the fallback runs every test, under coverage (4, 1).
    let s1 = land(&t.repo, "src/lib.rs", "    1\n}", "    2 - 1\n}").await?;
    let record = coverage_on(&t.repo, s1)?;
    assert_eq!(ran_on(&record, s1), all, "{record:#?}");
    assert_eq!(test_qualifier(&t.repo, s1)?.as_deref(), Some("full"));

    // gamma's edit selects only the test that ran gamma (2); the others
    // keep their runs from s1.
    let s2 = land(&t.repo, "src/lib.rs", "alpha() + 10", "10 + alpha()").await?;
    let record = coverage_on(&t.repo, s2)?;
    assert_eq!(ran_on(&record, s2), names(&["t_gamma"]), "{record:#?}");
    assert_eq!(ran_on(&record, s1), all, "earlier runs are kept");
    assert_eq!(test_qualifier(&t.repo, s2)?.as_deref(), Some("selected"));

    // beta changes in a landing that requires (and runs) no tests.
    let s3 = land(&t.repo, "src/other.rs", "    2\n", "    1 + 1\n").await?;
    assert!(coverage_at(&t.repo, s3)?.is_none(), "nothing ran");

    // gamma again: t_gamma covers it, and t_beta is stale (beta changed
    // since its run on s1); t_alpha is neither (3).
    let s4 = land(&t.repo, "src/lib.rs", "10 + alpha()", "alpha() + 10").await?;
    let record = coverage_on(&t.repo, s4)?;
    assert_eq!(
        ran_on(&record, s4),
        names(&["t_beta", "t_gamma"]),
        "{record:#?}"
    );

    // A manifest edit is a global trigger: the package runs in full, and
    // every test is refreshed (4).
    let s5 = land(
        &t.repo,
        "Cargo.toml",
        "version = \"0.1.0\"",
        "version = \"0.1.1\"",
    )
    .await?;
    let record = coverage_on(&t.repo, s5)?;
    assert_eq!(ran_on(&record, s5), all, "{record:#?}");
    // Each test keeps its last three runs.
    for test in &record.tests {
        assert!(record.runs(test).len() <= RUNS_PER_RECORD);
    }
    Ok(())
}
