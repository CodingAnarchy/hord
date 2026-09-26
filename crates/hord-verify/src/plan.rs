//! Verification plans and evidence reuse (spec §4.3, §7.1, ADR 0025).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{EvidenceKind, EvidenceResult, NodeId, ObjectId, Policy, RepoPath, SnapshotId};
use serde::{Deserialize, Serialize};

use crate::{EvidenceIndex, ImpactBound, Result, get_evidence};

/// What verification must produce, and how widely to look.
///
/// `require` holds `.hord/policy.toml` requirement names (spec §7.2), such
/// as `check`, `test:selected`, `test:full`, `lint`, `bench`. A verifier
/// plans the ones it knows and ignores the rest (`review:*` is signed by a
/// reviewer, not run).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerifyPolicy {
    /// Requirement names to satisfy.
    pub require: BTreeSet<String>,
    /// How far the impact set follows dependents (§6.5).
    pub bound: ImpactBound,
    /// Run everything when the impact set has more nodes than this
    /// (§7.1 item 4). `None`: no size fallback.
    pub max_impact: Option<usize>,
}

impl VerifyPolicy {
    /// The `[land]` table of `policy`: its `require` list and its
    /// `max_impact` (ADR 0022 amendments), with the default bound.
    ///
    /// Rule requirements (`[[rule]]`) depend on the change; add them with
    /// [`Self::requiring`] once `hord-policy` has matched the rules.
    #[must_use]
    pub fn from_policy(policy: &Policy) -> Self {
        Self {
            require: policy.land.require.iter().cloned().collect(),
            max_impact: policy.land.max_impact,
            ..Self::default()
        }
    }

    /// Add requirement names.
    #[must_use]
    pub fn requiring<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.require.extend(names.into_iter().map(Into::into));
        self
    }

    /// Whether `name` is required.
    #[must_use]
    pub fn requires(&self, name: &str) -> bool {
        self.require.contains(name)
    }
}

/// One command a plan runs; its [`Self::command`] and scope are the reuse
/// key together with the snapshot and toolchain.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Check {
    /// Policy requirement this check serves, e.g. `test:selected`.
    pub requirement: String,
    /// Evidence kind it produces.
    pub kind: EvidenceKind,
    /// Evidence qualifier (ADR 0026), e.g. `selected` or `full` for tests.
    pub qualifier: Option<String>,
    /// Program to run, e.g. `cargo`.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Environment set for the run, beyond the verifier's own.
    pub env: BTreeMap<String, String>,
    /// Directory to run in, relative to the checkout root.
    pub dir: RepoPath,
    /// What the evidence claims to cover: the impact set for a selection,
    /// `None` for a whole-suite run.
    pub scope: Option<BTreeSet<NodeId>>,
}

impl Check {
    /// The evidence `command`: program and arguments, space-separated,
    /// each quoted when it is empty or has whitespace or quotes, then the
    /// environment and directory when set. Deterministic, so it keys reuse.
    #[must_use]
    pub fn command(&self) -> String {
        let quote = |s: &str| -> String {
            if s.is_empty()
                || s.chars()
                    .any(|c| c.is_whitespace() || c == '"' || c == '\'')
            {
                format!("{s:?}")
            } else {
                s.to_owned()
            }
        };
        let mut parts: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", quote(v)))
            .collect();
        parts.push(quote(&self.program));
        parts.extend(self.args.iter().map(|a| quote(a)));
        let mut out = parts.join(" ");
        if !self.dir.is_root() {
            out.push_str(&format!(" (in {})", self.dir));
        }
        out
    }
}

/// A planned check whose evidence is already indexed for the same key.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Reused {
    /// The check that was not re-run.
    pub check: Check,
    /// The indexed evidence reused for it.
    pub evidence: ObjectId,
    /// Whether that evidence passed.
    pub passed: bool,
}

/// The checks that verify one snapshot (spec §4.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerifyPlan {
    /// Snapshot the checks run against.
    pub snapshot: SnapshotId,
    /// [`crate::Toolchain::id`] of the verifier that planned it.
    pub toolchain: ObjectId,
    /// Checks still to run.
    pub checks: Vec<Check>,
    /// Checks answered from the index (after [`Self::apply_reuse`]).
    pub reused: Vec<Reused>,
    /// Why the plan is what it is: fallbacks taken, selection counts.
    pub notes: Vec<String>,
}

impl VerifyPlan {
    /// An empty plan.
    #[must_use]
    pub fn new(snapshot: SnapshotId, toolchain: ObjectId) -> Self {
        Self {
            snapshot,
            toolchain,
            checks: Vec::new(),
            reused: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Move every check whose key already has indexed evidence to
    /// [`Self::reused`] (spec §7.1 item 1).
    ///
    /// Identical inputs give identical outputs, so a pass or a failure is
    /// reused; `Skipped` evidence is not. With several matches the newest
    /// (`produced_at`, then id) wins.
    pub fn apply_reuse(mut self, index: &dyn EvidenceIndex) -> Result<Self> {
        let mut remaining = Vec::new();
        for check in std::mem::take(&mut self.checks) {
            let ids = index.evidence_for_key(
                self.snapshot,
                self.toolchain,
                &check.command(),
                check.scope.as_ref(),
            )?;
            let mut best = None;
            for id in ids {
                let evidence = get_evidence(index, id)?;
                let passed = match evidence.result {
                    EvidenceResult::Pass => true,
                    EvidenceResult::Fail { .. } => false,
                    EvidenceResult::Skipped { .. } => continue,
                };
                let rank = (evidence.produced_at, id);
                if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
                    best = Some((rank, id, passed));
                }
            }
            match best {
                Some((_, evidence, passed)) => self.reused.push(Reused {
                    check,
                    evidence,
                    passed,
                }),
                None => remaining.push(check),
            }
        }
        self.checks = remaining;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hord_core::{Actor, LandPolicy, ReplayPolicy, Timestamp};

    use super::*;
    use crate::MemoryIndex;

    fn check(args: &[&str]) -> Check {
        Check {
            requirement: "check".into(),
            kind: EvidenceKind::Check,
            qualifier: None,
            program: "cargo".into(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            env: BTreeMap::new(),
            dir: RepoPath::default(),
            scope: None,
        }
    }

    #[test]
    fn command_quotes_and_orders_deterministically() {
        let mut c = check(&["test", "--", "a b", ""]);
        c.env.insert("B".into(), "2".into());
        c.env.insert("A".into(), "x y".into());
        c.dir = RepoPath::from_str("crates/x").expect("parse test path");
        assert_eq!(
            c.command(),
            "A=\"x y\" B=2 cargo test -- \"a b\" \"\" (in crates/x)"
        );
        assert_eq!(check(&["check"]).command(), "cargo check");
    }

    #[test]
    fn reuse_takes_the_newest_pass_or_fail_and_skips_skipped() {
        let index = MemoryIndex::new();
        let snapshot = ObjectId::from_bytes([1; 32]);
        let toolchain = ObjectId::from_bytes([2; 32]);
        let evidence = |c: &Check, result, at| {
            crate::EvidenceFields {
                kind: c.kind.clone(),
                qualifier: c.qualifier.clone(),
                snapshot,
                toolchain,
                command: c.command(),
                scope: c.scope.clone(),
                result,
                log: None,
                cost_ms: 0,
                produced_by: Actor::Human { id: "t".into() },
                produced_at: Timestamp::from_millis(at),
            }
            .build()
        };
        let (a, b, c) = (check(&["a"]), check(&["b"]), check(&["c"]));
        index
            .put_evidence(&evidence(&a, EvidenceResult::Pass, 1))
            .expect("put evidence");
        let newest = index
            .put_evidence(&evidence(
                &a,
                EvidenceResult::Fail {
                    summary: "x".into(),
                },
                2,
            ))
            .expect("put evidence");
        index
            .put_evidence(&evidence(
                &b,
                EvidenceResult::Skipped { reason: "r".into() },
                1,
            ))
            .expect("put evidence");
        let mut plan = VerifyPlan::new(snapshot, toolchain);
        plan.checks = vec![a.clone(), b.clone(), c.clone()];
        let plan = plan.apply_reuse(&index).expect("apply reuse");
        assert_eq!(plan.checks, vec![b, c]);
        assert_eq!(plan.reused.len(), 1);
        assert_eq!(plan.reused[0].evidence, newest);
        assert!(!plan.reused[0].passed);
    }

    #[test]
    fn policy_requirements() {
        let policy = Policy {
            land: LandPolicy {
                require: vec!["check".into(), "test:selected".into()],
                strict_reads: false,
                max_write_set: Some(200),
                max_replay_attempts: 2,
                max_impact: Some(40),
            },
            rules: Vec::new(),
            replay: ReplayPolicy::default(),
        };
        let p = VerifyPolicy::from_policy(&policy).requiring(["lint"]);
        assert!(p.requires("check") && p.requires("test:selected") && p.requires("lint"));
        assert!(!p.requires("test:full"));
        assert_eq!(p.bound, ImpactBound::default());
        assert_eq!(p.max_impact, Some(40));
    }
}
