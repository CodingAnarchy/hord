//! The M5 corpus report (spec §12 M5): the share resolved by replay,
//! attempts and budget outcomes, the arbitration round-trip, and the
//! review sheet a human rates ("sufficient to resolve").

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::corpus::{Case, Step};
use crate::run::{CaseResult, Outcome};

/// The spec's target share of cases resolved by replay, with a model.
pub const TARGET_SHARE: f64 = 0.60;

/// Totals over a run.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    /// Cases run.
    pub cases: usize,
    /// Cases that do what the corpus needs.
    pub valid: usize,
    /// Resolved by replay, acceptance tests passing (ADR 0029).
    pub resolved_by_replay: usize,
    /// A replay landed but an acceptance test fails.
    pub replay_failed_acceptance: usize,
    /// Parked for arbitration.
    pub parked: usize,
    /// `resolved_by_replay / valid`.
    pub resolved_share: f64,
    /// Replay attempts, by outcome.
    pub attempts: BTreeMap<String, usize>,
    /// Attempts killed at their wall-clock budget.
    pub killed: usize,
    /// Results rejected for reporting usage over budget.
    pub over_budget: usize,
    /// Results rejected for changing a protected acceptance test (ADR
    /// 0034).
    pub tampered: usize,
    /// Cost of every attempt, in US dollars, as the harness reported it.
    pub total_cost_usd: f64,
    /// Tokens of every attempt, as the harness reported them.
    pub total_tokens: u64,
    /// Wall-clock time of every attempt, in seconds.
    pub attempt_seconds: f64,
    /// Wall-clock time of every case (build, landing, replays, grading),
    /// in seconds.
    pub case_seconds: f64,
    /// Attempts by the model that ran them (ADR 0029).
    pub models: BTreeMap<String, usize>,
    /// Conflicts the lander saw, by kind (`hard`, `semantic`).
    pub observed_conflicts: BTreeMap<String, usize>,
    /// Correctness failures, each a reason the run fails.
    pub failures: Vec<String>,
}

/// Totals and correctness failures for `results` (with their cases, for
/// the scripted harness's expectations).
pub fn summarize(results: &[(Case, CaseResult)], max_attempts: usize) -> Summary {
    let mut s = Summary {
        cases: results.len(),
        ..Summary::default()
    };
    for (case, r) in results {
        match &r.outcome {
            Outcome::ResolvedByReplay => s.resolved_by_replay += 1,
            Outcome::ReplayFailedAcceptance { .. } => s.replay_failed_acceptance += 1,
            Outcome::Parked => s.parked += 1,
            Outcome::Invalid { why } => s.failures.push(format!("{}: invalid case: {why}", r.id)),
            Outcome::Error { why } => s.failures.push(format!("{}: {why}", r.id)),
        }
        if !matches!(r.outcome, Outcome::Invalid { .. } | Outcome::Error { .. }) {
            s.valid += 1;
        }
        if let Some(kind) = &r.observed_conflict {
            *s.observed_conflicts.entry(kind.clone()).or_default() += 1;
        }
        s.case_seconds += r.seconds;
        for a in &r.attempts {
            *s.attempts.entry(a.outcome.clone()).or_default() += 1;
            s.total_cost_usd += a.cost_usd.unwrap_or(0.0);
            s.total_tokens += a.tokens.unwrap_or(0);
            s.attempt_seconds += a.elapsed_ms as f64 / 1000.0;
            if let Some(model) = &a.model {
                *s.models.entry(model.clone()).or_default() += 1;
            }
            match a.outcome.as_str() {
                "killed" => s.killed += 1,
                "over_budget" => s.over_budget += 1,
                "tampered" => s.tampered += 1,
                _ => {}
            }
        }
        for v in &r.budget_violations {
            s.failures.push(format!("{}: budget: {v}", r.id));
        }
        if let Some(round_trip) = &r.arbitration
            && !round_trip.ok
        {
            s.failures.push(format!(
                "{}: arbitration round-trip: {}",
                r.id, round_trip.detail
            ));
        }
        if let Some(expected) = r.expected_resolved {
            scripted_expectations(case, r, expected, max_attempts, &mut s.failures);
        }
    }
    s.resolved_share = if s.valid == 0 {
        0.0
    } else {
        s.resolved_by_replay as f64 / s.valid as f64
    };
    s
}

/// With the scripted harness the grading is known in advance: a case the
/// script resolves is resolved by replay, the rest are parked, and a
/// sleeping or over-budget step shows up as a killed or rejected attempt.
fn scripted_expectations(
    case: &Case,
    r: &CaseResult,
    expected: bool,
    max_attempts: usize,
    failures: &mut Vec<String>,
) {
    if matches!(r.outcome, Outcome::Invalid { .. } | Outcome::Error { .. }) {
        return;
    }
    let resolved = r.outcome == Outcome::ResolvedByReplay;
    if resolved != expected {
        failures.push(format!(
            "{}: graded {:?}, but the script {} it",
            r.id,
            r.outcome,
            if expected {
                "resolves"
            } else {
                "does not resolve"
            }
        ));
    }
    let played: Vec<Step> = case.script.iter().copied().take(max_attempts).collect();
    let until = played
        .iter()
        .position(|s| *s == Step::Resolve)
        .map_or(played.len(), |i| i + 1);
    for (step, outcome) in [
        (Step::Sleep, "killed"),
        (Step::OverBudget, "over_budget"),
        (Step::Tamper, "tampered"),
    ] {
        if played[..until].contains(&step) && !r.attempts.iter().any(|a| a.outcome == outcome) {
            failures.push(format!(
                "{}: the script's {step:?} attempt was not {outcome}: {:?}",
                r.id, r.attempts
            ));
        }
    }
}

/// The text report.
pub fn text(summary: &Summary, gated_share: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "M5 conflict corpus: {} cases ({} valid)",
        summary.cases, summary.valid
    );
    let _ = writeln!(
        out,
        "resolved by replay: {} ({:.1}%, target {:.0}%{})",
        summary.resolved_by_replay,
        summary.resolved_share * 100.0,
        TARGET_SHARE * 100.0,
        if gated_share {
            ""
        } else {
            ", reported, not gated"
        }
    );
    let _ = writeln!(
        out,
        "replay landed, acceptance failed: {}",
        summary.replay_failed_acceptance
    );
    let _ = writeln!(out, "parked for arbitration: {}", summary.parked);
    let _ = writeln!(out, "conflicts seen: {:?}", summary.observed_conflicts);
    let _ = writeln!(
        out,
        "attempts: {:?}; killed at budget: {}; over budget: {}; tampered with acceptance tests: {}",
        summary.attempts, summary.killed, summary.over_budget, summary.tampered
    );
    let _ = writeln!(
        out,
        "spent: ${:.4}, {} tokens, {:.0}s in attempts ({:.0}s in cases); models {:?}",
        summary.total_cost_usd,
        summary.total_tokens,
        summary.attempt_seconds,
        summary.case_seconds,
        summary.models
    );
    if summary.failures.is_empty() {
        let _ = writeln!(
            out,
            "correctness: budget enforced, every parked case resolved from the workbench, grading as expected"
        );
    } else {
        let _ = writeln!(out, "FAILURES ({}):", summary.failures.len());
        for failure in &summary.failures {
            let _ = writeln!(out, "  {failure}");
        }
    }
    out
}

/// The review sheet: every parked case's conflict summary, for a human to
/// rate as sufficient to resolve (spec §12 M5: ≥ 90%). Markdown, plus a
/// CSV to fill in.
pub fn write_review(out: &Path, results: &[(Case, CaseResult)]) -> Result<()> {
    let mut md = String::from(
        "# M5 conflict summaries: review sheet\n\nRate each summary: is it sufficient for a person to resolve the conflict? Record the rating in `review.csv` (`sufficient` = yes or no).\n",
    );
    let mut csv = String::from("id,kind,ambiguous,sufficient,notes\n");
    for (case, r) in results {
        if r.outcome != Outcome::Parked {
            continue;
        }
        let _ = writeln!(md, "\n## {} ({}, {})\n", r.id, r.kind, r.designed_conflict);
        for task in &case.tasks {
            let _ = writeln!(
                md,
                "- Task {}: {} (acceptance: `{}`)",
                task.name, task.summary, task.test
            );
        }
        let _ = writeln!(
            md,
            "\n```text\n{}\n```\n\nSufficient to resolve: [ ] yes  [ ] no",
            r.summary.as_deref().unwrap_or("(no summary)").trim_end()
        );
        let _ = writeln!(csv, "{},{},{},,", r.id, r.kind, r.ambiguous);
    }
    std::fs::write(out.join("review.md"), md)?;
    std::fs::write(out.join("review.csv"), csv)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Attempt;

    fn result(case: &Case, outcome: Outcome, attempts: &[&str]) -> CaseResult {
        CaseResult {
            id: case.id.clone(),
            kind: case.kind.clone(),
            designed_conflict: case.conflict.clone(),
            observed_conflict: Some(case.conflict.clone()),
            ambiguous: case.ambiguous,
            intents: [case.first().summary.clone(), case.second().summary.clone()],
            outcome,
            attempts: attempts
                .iter()
                .enumerate()
                .map(|(i, o)| Attempt {
                    attempt: u32::try_from(i + 1).unwrap_or(u32::MAX),
                    outcome: (*o).into(),
                    elapsed_ms: 1_500,
                    tokens: Some(100),
                    cost_usd: Some(0.25),
                    model: Some("m".into()),
                    tampered: Vec::new(),
                    detail: None,
                })
                .collect(),
            budget_violations: Vec::new(),
            summary: None,
            arbitration: None,
            expected_resolved: Some(case.scripted_resolves(2)),
            seconds: 0.0,
        }
    }

    /// The scripted run's grading is checked against the script: a case
    /// graded otherwise, or a sleeping attempt that was not killed, fails.
    #[test]
    fn scripted_grading_is_checked() -> anyhow::Result<()> {
        let corpus = crate::generate::corpus();
        let sleeper = corpus
            .iter()
            .find(|c| c.script == [Step::Sleep, Step::Resolve])
            .ok_or_else(|| anyhow::anyhow!("a sleeping case"))?;
        let good = result(sleeper, Outcome::ResolvedByReplay, &["killed", "proposed"]);
        assert!(summarize(&[(sleeper.clone(), good)], 2).failures.is_empty());
        let unkilled = result(sleeper, Outcome::ResolvedByReplay, &["gave_up", "proposed"]);
        assert_eq!(
            summarize(&[(sleeper.clone(), unkilled)], 2).failures.len(),
            1
        );
        // Run totals come from the attempts' reported usage.
        let totals = summarize(
            &[(
                sleeper.clone(),
                result(sleeper, Outcome::ResolvedByReplay, &["killed", "proposed"]),
            )],
            2,
        );
        assert_eq!(totals.total_tokens, 200);
        assert!((totals.total_cost_usd - 0.5).abs() < 1e-9);
        assert!((totals.attempt_seconds - 3.0).abs() < 1e-9);
        assert_eq!(totals.models.get("m"), Some(&2));
        let parked = result(sleeper, Outcome::Parked, &["killed", "proposed"]);
        let summary = summarize(&[(sleeper.clone(), parked)], 2);
        assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
        assert_eq!(summary.parked, 1);
        Ok(())
    }
}
