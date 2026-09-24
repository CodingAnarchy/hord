//! Per-test coverage records: the observed `Tests(t, def)` edges (spec §3.8,
//! §7.1 item 2, ADR 0022).
//!
//! A record is made by running every test once with coverage
//! instrumentation. It is keyed by [`NodeId`], so it stays meaningful
//! across snapshots through identity (ADRs 0017–0020). It is stored as
//! `Evidence { kind: Custom("coverage") }` whose `log` is a
//! [`hord_core::Blob`] holding the canonical CBOR of the record.

use std::collections::BTreeSet;

use hord_core::{EvidenceKind, EvidenceResult, NodeId, ObjectId, SnapshotId};
use serde::{Deserialize, Serialize};

use crate::{EvidenceIndex, Result, get_evidence, get_log};

/// `Custom` evidence kind of a coverage record.
pub const COVERAGE_KIND: &str = "coverage";

/// Record format this build writes.
const FORMAT: u8 = 1;

/// The test binary (harness) a test is in.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct TestTarget {
    /// Target kind as the build tool names it: for cargo `lib`, `bin`,
    /// `test`, `bench`, `example`, or `doc`.
    pub kind: String,
    /// Target name.
    pub name: String,
}

/// One test, as the runner names it.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct TestRef {
    /// Package the test binary belongs to.
    pub package: String,
    /// The test binary.
    pub target: TestTarget,
    /// The harness's name for the test (for libtest, its path in the crate).
    pub name: String,
}

/// One test's coverage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestCoverage {
    /// The test.
    pub test: TestRef,
    /// The test's own definition, when it maps to one.
    pub node: Option<NodeId>,
    /// Indices into [`CoverageRecord::defs`] of the definitions it ran.
    pub covers: Vec<u32>,
    /// It failed (or did not run) during the coverage run; its edges may
    /// be partial.
    pub failed: bool,
}

/// Observed `Tests(t, def)` edges for one snapshot and toolchain.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CoverageRecord {
    /// Format version.
    pub format: u8,
    /// Snapshot the coverage run was made on.
    pub snapshot: SnapshotId,
    /// [`crate::Toolchain::id`] it ran with.
    pub toolchain: ObjectId,
    /// Every definition coverage attributed any code to (instrumented),
    /// sorted. A definition absent here has no coverage record.
    pub defs: Vec<NodeId>,
    /// Every test the suite had, sorted by [`TestRef`].
    pub tests: Vec<TestCoverage>,
}

impl CoverageRecord {
    /// Build a record from per-test sets. `instrumented` must include
    /// every covered node; missing ones are added.
    #[must_use]
    pub fn new(
        snapshot: SnapshotId,
        toolchain: ObjectId,
        instrumented: BTreeSet<NodeId>,
        tests: Vec<(TestRef, Option<NodeId>, BTreeSet<NodeId>, bool)>,
    ) -> Self {
        let mut all = instrumented;
        for (_, _, covers, _) in &tests {
            all.extend(covers.iter().copied());
        }
        let defs: Vec<NodeId> = all.into_iter().collect();
        let index = |n: &NodeId| -> u32 {
            let i = defs
                .binary_search(n)
                .expect("every covered node is in defs");
            u32::try_from(i).unwrap_or(u32::MAX)
        };
        let mut tests: Vec<TestCoverage> = tests
            .into_iter()
            .map(|(test, node, covers, failed)| TestCoverage {
                test,
                node,
                covers: covers.iter().map(index).collect(),
                failed,
            })
            .collect();
        tests.sort_by(|a, b| a.test.cmp(&b.test));
        Self {
            format: FORMAT,
            snapshot,
            toolchain,
            defs,
            tests,
        }
    }

    /// Whether coverage attributed any code to `node`.
    #[must_use]
    pub fn is_instrumented(&self, node: NodeId) -> bool {
        self.defs.binary_search(&node).is_ok()
    }

    /// Definitions `test` ran.
    pub fn covered_by<'a>(&'a self, test: &'a TestCoverage) -> impl Iterator<Item = NodeId> + 'a {
        test.covers
            .iter()
            .filter_map(|i| self.defs.get(*i as usize).copied())
    }

    /// Tests that ran any node of `nodes`.
    #[must_use]
    pub fn tests_covering(&self, nodes: &BTreeSet<NodeId>) -> BTreeSet<&TestRef> {
        let wanted: BTreeSet<u32> = nodes
            .iter()
            .filter_map(|n| self.defs.binary_search(n).ok())
            .filter_map(|i| u32::try_from(i).ok())
            .collect();
        if wanted.is_empty() {
            return BTreeSet::new();
        }
        self.tests
            .iter()
            .filter(|t| t.covers.iter().any(|i| wanted.contains(i)))
            .map(|t| &t.test)
            .collect()
    }

    /// The tests whose own definition is `node`.
    pub fn tests_at(&self, node: NodeId) -> impl Iterator<Item = &TestRef> + '_ {
        self.tests
            .iter()
            .filter(move |t| t.node == Some(node))
            .map(|t| &t.test)
    }

    /// Store this record as its evidence log: a [`hord_core::Blob`] of
    /// the record's canonical CBOR.
    pub fn put(&self, index: &dyn EvidenceIndex) -> Result<ObjectId> {
        crate::put_log(index, &hord_encoding::encode(self)?)
    }

    /// Load a record stored with [`Self::put`].
    pub fn get(index: &dyn EvidenceIndex, log: ObjectId) -> Result<Self> {
        Ok(hord_encoding::decode(&get_log(index, log)?)?)
    }
}

/// The newest coverage record for `toolchain` on the first snapshot of
/// `snapshots` that has one (callers pass the base, then older snapshots),
/// with the id of its evidence.
///
/// `None` means selection must fall back to everything (ADR 0022).
pub fn find_coverage(
    index: &dyn EvidenceIndex,
    snapshots: impl IntoIterator<Item = SnapshotId>,
    toolchain: ObjectId,
) -> Result<Option<(ObjectId, CoverageRecord)>> {
    for snapshot in snapshots {
        let mut best = None;
        for id in index.evidence_at(snapshot)? {
            let evidence = get_evidence(index, id)?;
            if evidence.toolchain != toolchain
                || evidence.kind != EvidenceKind::Custom(COVERAGE_KIND.to_owned())
                || evidence.result != EvidenceResult::Pass
            {
                continue;
            }
            let Some(log) = evidence.log else {
                continue;
            };
            let rank = (evidence.produced_at, id);
            if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
                best = Some((rank, id, log));
            }
        }
        if let Some((_, id, log)) = best {
            return Ok(Some((id, CoverageRecord::get(index, log)?)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use hord_core::{Actor, Timestamp};

    use super::*;
    use crate::MemoryIndex;

    fn n(v: u128) -> NodeId {
        NodeId::from_u128(v)
    }

    fn t(name: &str) -> TestRef {
        TestRef {
            package: "p".into(),
            target: TestTarget {
                kind: "test".into(),
                name: "suite".into(),
            },
            name: name.into(),
        }
    }

    fn record() -> CoverageRecord {
        CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            ObjectId::from_bytes([2; 32]),
            [n(1), n(9)].into_iter().collect(),
            vec![
                (
                    t("b"),
                    Some(n(20)),
                    [n(1), n(2)].into_iter().collect(),
                    false,
                ),
                (t("a"), None, [n(3)].into_iter().collect(), true),
            ],
        )
    }

    #[test]
    fn queries() {
        let r = record();
        assert_eq!(r.defs, vec![n(1), n(2), n(3), n(9)]);
        assert_eq!(r.tests[0].test, t("a"));
        assert!(r.is_instrumented(n(9)) && !r.is_instrumented(n(4)));
        let hit = r.tests_covering(&[n(2), n(7)].into_iter().collect());
        assert_eq!(hit.into_iter().cloned().collect::<Vec<_>>(), vec![t("b")]);
        assert!(r.tests_covering(&[n(9)].into_iter().collect()).is_empty());
        assert_eq!(r.tests_at(n(20)).cloned().collect::<Vec<_>>(), vec![t("b")]);
        assert_eq!(
            r.covered_by(&r.tests[1]).collect::<Vec<_>>(),
            vec![n(1), n(2)]
        );
    }

    #[test]
    fn stored_as_evidence_and_found_by_toolchain() {
        let index = MemoryIndex::new();
        let r = record();
        let log = r.put(&index).unwrap();
        let evidence = |toolchain: ObjectId, snapshot: u8| {
            crate::EvidenceFields {
                kind: EvidenceKind::Custom(COVERAGE_KIND.into()),
                qualifier: None,
                snapshot: ObjectId::from_bytes([snapshot; 32]),
                toolchain,
                command: "cargo llvm-cov".into(),
                scope: None,
                result: EvidenceResult::Pass,
                log: Some(log),
                cost_ms: 0,
                produced_by: Actor::Human { id: "t".into() },
                produced_at: Timestamp::from_millis(0),
            }
            .build()
        };
        index
            .put_evidence(&evidence(ObjectId::from_bytes([2; 32]), 1))
            .unwrap();
        let snaps = [ObjectId::from_bytes([5; 32]), ObjectId::from_bytes([1; 32])];
        let (_, found) = find_coverage(&index, snaps, ObjectId::from_bytes([2; 32]))
            .unwrap()
            .expect("record");
        assert_eq!(found, r);
        assert!(
            find_coverage(&index, snaps, ObjectId::from_bytes([3; 32]))
                .unwrap()
                .is_none()
        );
    }
}
