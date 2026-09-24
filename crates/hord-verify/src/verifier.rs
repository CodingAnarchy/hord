//! The [`Verifier`] contract (spec §4.3 as amended by ADR 0025) and the
//! orchestration the lander calls.

use std::path::PathBuf;

use hord_core::{Evidence, EvidenceResult, ObjectId, SnapshotId};

use crate::{EvidenceIndex, ImpactSet, Result, Toolchain, VerifyPlan, VerifyPolicy, get_evidence};

/// A directory holding exactly one snapshot's files, for a verifier to run
/// in: a workspace directory or a pristine checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkout {
    /// Root directory of the checkout.
    pub root: PathBuf,
    /// Snapshot whose files it holds.
    pub snapshot: SnapshotId,
}

/// A language's verification toolchain runner (spec §4.3, ADR 0025).
pub trait Verifier: Send + Sync {
    /// Language this verifier checks, e.g. `rust`.
    fn lang(&self) -> &str;

    /// The toolchain its evidence is produced with.
    fn toolchain(&self) -> &Toolchain;

    /// Plan the checks `policy` requires for `snapshot`, given what the
    /// change can affect. May plan everything. Does not apply reuse; see
    /// [`plan_with_reuse`].
    fn plan(
        &self,
        snapshot: SnapshotId,
        impact: &ImpactSet,
        policy: &VerifyPolicy,
    ) -> Result<VerifyPlan>;

    /// Run `plan.checks` in `checkout` and return one [`Evidence`] per
    /// check, in plan order. Logs are stored in `logs`; the evidence is not
    /// indexed (the caller does that). A check that fails is `Fail`
    /// evidence, not an error. `checkout.snapshot` must be `plan.snapshot`.
    fn run(
        &self,
        checkout: &Checkout,
        plan: &VerifyPlan,
        logs: &dyn EvidenceIndex,
    ) -> Result<Vec<Evidence>>;
}

/// Outcome of verifying a snapshot (ADR 0025).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Every check passed.
    Pass {
        /// Evidence objects, run and reused, in plan order.
        evidence: Vec<ObjectId>,
    },
    /// Some check failed: a semantic conflict (spec §6.5).
    Fail {
        /// Evidence objects, run and reused, in plan order.
        evidence: Vec<ObjectId>,
        /// The failing checks' commands and summaries.
        reason: String,
    },
}

impl Verdict {
    /// The evidence behind the verdict.
    #[must_use]
    pub fn evidence(&self) -> &[ObjectId] {
        match self {
            Self::Pass { evidence } | Self::Fail { evidence, .. } => evidence,
        }
    }

    /// Whether it passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }
}

/// [`Verifier::plan`], then [`VerifyPlan::apply_reuse`] against `index`:
/// evidence already indexed for the same key is not re-run (ADR 0025).
pub fn plan_with_reuse(
    verifier: &dyn Verifier,
    index: &dyn EvidenceIndex,
    snapshot: SnapshotId,
    impact: &ImpactSet,
    policy: &VerifyPolicy,
) -> Result<VerifyPlan> {
    verifier.plan(snapshot, impact, policy)?.apply_reuse(index)
}

/// Verify `checkout` for a change with `impact` under `policy`: plan,
/// reuse, run the rest, index the new evidence under the checkout's
/// snapshot, and return the verdict.
///
/// The toolchain object is stored too, so evidence names a resolvable
/// [`Evidence::toolchain`]. An unchanged change on an unchanged head plans
/// the same checks on the same snapshot, so every key hits and nothing
/// runs.
pub fn verify(
    verifier: &dyn Verifier,
    index: &dyn EvidenceIndex,
    checkout: &Checkout,
    impact: &ImpactSet,
    policy: &VerifyPolicy,
) -> Result<Verdict> {
    index.put_raw(&hord_encoding::encode(verifier.toolchain())?)?;
    let plan = plan_with_reuse(verifier, index, checkout.snapshot, impact, policy)?;
    let fresh = if plan.checks.is_empty() {
        Vec::new()
    } else {
        verifier.run(checkout, &plan, index)?
    };
    let mut evidence = Vec::new();
    let mut failures = Vec::new();
    for reused in &plan.reused {
        evidence.push(reused.evidence);
        if !reused.passed {
            let summary = match get_evidence(index, reused.evidence)?.result {
                EvidenceResult::Fail { summary } => summary,
                _ => String::new(),
            };
            failures.push(format!("{} (reused): {summary}", reused.check.command()));
        }
    }
    for ev in &fresh {
        evidence.push(index.put_evidence(ev)?);
        if let EvidenceResult::Fail { summary } = &ev.result {
            failures.push(format!("{}: {summary}", ev.command));
        }
    }
    Ok(if failures.is_empty() {
        Verdict::Pass { evidence }
    } else {
        Verdict::Fail {
            evidence,
            reason: failures.join("\n"),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hord_core::{Actor, EvidenceKind, RepoPath, Timestamp};

    use super::*;
    use crate::{Check, MemoryIndex};

    /// Plans one check per requirement; `fail` makes that one fail.
    struct Fake {
        toolchain: Toolchain,
        fail: &'static str,
        runs: AtomicUsize,
    }

    impl Verifier for Fake {
        fn lang(&self) -> &str {
            "fake"
        }

        fn toolchain(&self) -> &Toolchain {
            &self.toolchain
        }

        fn plan(
            &self,
            snapshot: SnapshotId,
            impact: &ImpactSet,
            policy: &VerifyPolicy,
        ) -> Result<VerifyPlan> {
            let mut plan = VerifyPlan::new(snapshot, self.toolchain.id()?);
            for req in &policy.require {
                plan.checks.push(Check {
                    requirement: req.clone(),
                    kind: EvidenceKind::Custom(req.clone()),
                    qualifier: None,
                    program: "true".into(),
                    args: vec![req.clone()],
                    env: BTreeMap::new(),
                    dir: RepoPath::default(),
                    scope: Some(impact.node_set()),
                });
            }
            Ok(plan)
        }

        fn run(
            &self,
            checkout: &Checkout,
            plan: &VerifyPlan,
            logs: &dyn EvidenceIndex,
        ) -> Result<Vec<Evidence>> {
            if checkout.snapshot != plan.snapshot {
                return Err(crate::Error::WrongCheckout {
                    planned: plan.snapshot,
                    checkout: checkout.snapshot,
                });
            }
            let log = crate::put_log(logs, b"ran")?;
            Ok(plan
                .checks
                .iter()
                .map(|c| {
                    self.runs.fetch_add(1, Ordering::Relaxed);
                    crate::EvidenceFields {
                        kind: c.kind.clone(),
                        qualifier: c.qualifier.clone(),
                        snapshot: plan.snapshot,
                        toolchain: plan.toolchain,
                        command: c.command(),
                        scope: c.scope.clone(),
                        result: if c.requirement == self.fail {
                            EvidenceResult::Fail {
                                summary: "boom".into(),
                            }
                        } else {
                            EvidenceResult::Pass
                        },
                        log: Some(log),
                        cost_ms: 1,
                        produced_by: Actor::Human { id: "t".into() },
                        produced_at: Timestamp::from_millis(1),
                    }
                    .build()
                })
                .collect())
        }
    }

    fn fake(fail: &'static str) -> Fake {
        Fake {
            toolchain: Toolchain::new("fake").with("fake", "1"),
            fail,
            runs: AtomicUsize::new(0),
        }
    }

    #[test]
    fn verify_runs_once_then_reuses_everything() {
        let index = MemoryIndex::new();
        let verifier = fake("");
        let checkout = Checkout {
            root: PathBuf::from("."),
            snapshot: ObjectId::from_bytes([4; 32]),
        };
        let policy = VerifyPolicy::default().requiring(["check", "test:selected"]);
        let impact = ImpactSet::default();
        let first = verify(&verifier, &index, &checkout, &impact, &policy).unwrap();
        assert!(first.passed());
        assert_eq!(first.evidence().len(), 2);
        assert_eq!(verifier.runs.load(Ordering::Relaxed), 2);
        // Resubmitting the unchanged change on the unchanged head runs nothing.
        let second = verify(&verifier, &index, &checkout, &impact, &policy).unwrap();
        assert_eq!(verifier.runs.load(Ordering::Relaxed), 2);
        let mut a = first.evidence().to_vec();
        let mut b = second.evidence().to_vec();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        // The toolchain object is resolvable.
        assert!(index.get_raw(verifier.toolchain.id().unwrap()).is_ok());
        // Another snapshot is another key.
        let other = Checkout {
            snapshot: ObjectId::from_bytes([5; 32]),
            ..checkout
        };
        verify(&verifier, &index, &other, &impact, &policy).unwrap();
        assert_eq!(verifier.runs.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn failures_fail_the_verdict_and_are_reused_as_failures() {
        let index = MemoryIndex::new();
        let verifier = fake("lint");
        let checkout = Checkout {
            root: PathBuf::from("."),
            snapshot: ObjectId::from_bytes([4; 32]),
        };
        let policy = VerifyPolicy::default().requiring(["check", "lint"]);
        let verdict = verify(&verifier, &index, &checkout, &ImpactSet::default(), &policy).unwrap();
        let Verdict::Fail { evidence, reason } = verdict else {
            panic!("expected failure");
        };
        assert_eq!(evidence.len(), 2);
        assert!(reason.contains("true lint: boom"), "{reason}");
        let again = verify(&verifier, &index, &checkout, &ImpactSet::default(), &policy).unwrap();
        assert!(!again.passed());
        assert_eq!(verifier.runs.load(Ordering::Relaxed), 2);
    }
}
