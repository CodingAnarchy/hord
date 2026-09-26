//! Per-test coverage records: the observed `Tests(t, def)` edges (spec §3.8,
//! §7.1 item 2, ADR 0022).
//!
//! A record is made by running every test once with coverage
//! instrumentation. It is keyed by [`NodeId`], so it stays meaningful
//! across snapshots through identity (ADRs 0017–0020). It is stored as
//! `Evidence { kind: Custom("coverage") }` whose `log` is a
//! [`hord_core::Blob`] holding the canonical CBOR of the record.

use std::collections::{BTreeMap, BTreeSet};

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
    /// Snapshot this test's coverage was taken on, when it differs from
    /// [`CoverageRecord::snapshot`] (ADR 0022: coverage is fresh per test;
    /// see [`CoverageRecord::merge`]). `None`: the record's snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotId>,
    /// The instrumented runs this entry is the union of, oldest first (at
    /// most [`RUNS_PER_RECORD`]; ADR 0022: a record is the union of a test's
    /// last three runs). `covers` is their union and `snapshot` the oldest
    /// run's. Empty: one run, described by `covers` and `snapshot` alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<TestRun>,
}

/// One instrumented run of a test, inside a [`TestCoverage`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestRun {
    /// Snapshot the run was on.
    pub snapshot: SnapshotId,
    /// Indices into [`CoverageRecord::defs`] of the definitions it ran.
    pub covers: Vec<u32>,
}

/// How many of a test's latest instrumented runs its record unites (ADR
/// 0022, 2026-09-24): one run of a nondeterministic test can miss functions
/// another run executes.
pub const RUNS_PER_RECORD: usize = 3;

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
                snapshot: None,
                runs: Vec::new(),
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

    /// `test`'s runs, oldest first, each with its snapshot and the
    /// definitions it executed. An entry without [`TestCoverage::runs`] is
    /// one run: its snapshot and coverage.
    #[must_use]
    pub fn runs(&self, test: &TestCoverage) -> Vec<(SnapshotId, BTreeSet<NodeId>)> {
        if test.runs.is_empty() {
            return vec![(self.test_snapshot(test), self.covered_by(test).collect())];
        }
        test.runs
            .iter()
            .map(|r| {
                let nodes = r
                    .covers
                    .iter()
                    .filter_map(|i| self.defs.get(*i as usize).copied())
                    .collect();
                (r.snapshot, nodes)
            })
            .collect()
    }

    /// Whether `test` is stale under `drift` (ADR 0022, staleness ends once
    /// the test re-ran on the change): some run r executed a definition
    /// that changed after r's snapshot, and no later run of the test has a
    /// snapshot at or after that change.
    ///
    /// Runs are in landing order, so a change is unexcused exactly when it
    /// came after the newest run: the test is stale when anything changed
    /// since its newest run's snapshot intersects what *any* of its runs
    /// executed (the nondeterministic path seen only in an older run still
    /// counts). Changes between its runs are excused, since the newest run
    /// already ran on them. A newest run whose snapshot the chain does not
    /// know counts as stale. Coverage selection, by contrast, uses the
    /// union of the runs ([`Self::tests_covering`]).
    #[must_use]
    pub fn is_stale(&self, test: &TestCoverage, drift: &crate::Drift) -> bool {
        let runs = self.runs(test);
        let Some((newest, _)) = runs.last() else {
            return true;
        };
        match drift.changed_since(*newest) {
            None => true,
            Some(changed) => runs
                .iter()
                .any(|(_, executed)| !executed.is_disjoint(&changed)),
        }
    }

    /// The snapshot `test`'s coverage was taken on.
    #[must_use]
    pub fn test_snapshot(&self, test: &TestCoverage) -> SnapshotId {
        test.snapshot.unwrap_or(self.snapshot)
    }

    /// This record refreshed by `newer`, a run of some tests (ADR 0022:
    /// every verification run refreshes the coverage of each test it ran),
    /// keeping each test's last [`RUNS_PER_RECORD`] runs.
    #[must_use]
    pub fn merge(&self, newer: &CoverageRecord) -> CoverageRecord {
        self.merge_keeping(newer, RUNS_PER_RECORD)
    }

    /// [`Self::merge`], uniting each test's last `keep` runs (at least 1).
    ///
    /// A test in `newer` gets `newer`'s run appended to its runs here, the
    /// oldest dropped past `keep`; its coverage is the union of the kept
    /// runs, its snapshot the oldest kept run's (so drift is measured from
    /// there), and `failed` is the newest run's. Every other test keeps its
    /// entry. The result's snapshot and toolchain are `newer`'s, and its
    /// instrumented definitions are the union.
    #[must_use]
    pub fn merge_keeping(&self, newer: &CoverageRecord, keep: usize) -> CoverageRecord {
        let keep = keep.max(1);
        let mut defs: BTreeSet<NodeId> = self.defs.iter().copied().collect();
        defs.extend(newer.defs.iter().copied());
        // Every test's runs as node sets, oldest first.
        type Runs = Vec<(SnapshotId, BTreeSet<NodeId>)>;
        let runs_of = |record: &CoverageRecord, t: &TestCoverage| -> Runs { record.runs(t) };
        let mut entries: BTreeMap<TestRef, (TestCoverage, Runs)> = BTreeMap::new();
        for t in &self.tests {
            entries.insert(t.test.clone(), (t.clone(), runs_of(self, t)));
        }
        for t in &newer.tests {
            let fresh = runs_of(newer, t);
            match entries.get_mut(&t.test) {
                Some((entry, runs)) => {
                    runs.extend(fresh);
                    let drop = runs.len().saturating_sub(keep);
                    runs.drain(..drop);
                    entry.failed = t.failed;
                    entry.node = t.node.or(entry.node);
                }
                None => {
                    let mut runs = fresh;
                    let drop = runs.len().saturating_sub(keep);
                    runs.drain(..drop);
                    entries.insert(t.test.clone(), (t.clone(), runs));
                }
            }
        }
        let defs: Vec<NodeId> = defs.into_iter().collect();
        let index = |nodes: &BTreeSet<NodeId>| -> Vec<u32> {
            nodes
                .iter()
                .filter_map(|n| defs.binary_search(n).ok())
                .filter_map(|i| u32::try_from(i).ok())
                .collect()
        };
        let tests: Vec<TestCoverage> = entries
            .into_values()
            .map(|(mut t, runs)| {
                let union: BTreeSet<NodeId> =
                    runs.iter().flat_map(|(_, n)| n.iter().copied()).collect();
                let oldest = runs.first().map_or(newer.snapshot, |(s, _)| *s);
                t.covers = index(&union);
                t.snapshot = (oldest != newer.snapshot).then_some(oldest);
                t.runs = if runs.len() > 1 {
                    runs.iter()
                        .map(|(s, n)| TestRun {
                            snapshot: *s,
                            covers: index(n),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                t
            })
            .collect();
        CoverageRecord {
            format: FORMAT,
            snapshot: newer.snapshot,
            toolchain: newer.toolchain,
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
    newest_coverage(index, snapshots, Some(toolchain))
}

/// [`find_coverage`], for any toolchain when `toolchain` is `None`: the
/// observed `Tests` edges the repository browser shows (spec §3.8), where
/// no toolchain is at hand.
pub fn newest_coverage(
    index: &dyn EvidenceIndex,
    snapshots: impl IntoIterator<Item = SnapshotId>,
    toolchain: Option<ObjectId>,
) -> Result<Option<(ObjectId, CoverageRecord)>> {
    for snapshot in snapshots {
        let mut best = None;
        for id in index.evidence_at(snapshot)? {
            let evidence = get_evidence(index, id)?;
            if toolchain.is_some_and(|t| t != evidence.toolchain)
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
    fn a_record_unites_the_last_three_runs_and_dates_from_the_oldest() {
        let tc = ObjectId::from_bytes([2; 32]);
        let snap = |n: u8| ObjectId::from_bytes([n; 32]);
        let run = |s: u8, covers: &[u128]| {
            CoverageRecord::new(
                snap(s),
                tc,
                BTreeSet::new(),
                vec![(t("x"), None, covers.iter().copied().map(n).collect(), false)],
            )
        };
        // A nondeterministic path: run 1 enters f(1), run 2 does not.
        let r1 = run(10, &[1, 2]);
        let ledger = r1.merge(&run(11, &[2]));
        let x = &ledger.tests[0];
        assert_eq!(ledger.covered_by(x).collect::<Vec<_>>(), vec![n(1), n(2)]);
        assert_eq!(
            ledger.test_snapshot(x),
            snap(10),
            "drift from the oldest run"
        );
        assert_eq!(
            ledger.tests_covering(&[n(1)].into_iter().collect()).len(),
            1
        );
        // Third and fourth runs: run 1 falls out of the union.
        let ledger = ledger.merge(&run(12, &[3])).merge(&run(13, &[4]));
        let x = &ledger.tests[0];
        assert_eq!(
            ledger.covered_by(x).collect::<Vec<_>>(),
            vec![n(2), n(3), n(4)]
        );
        assert_eq!(ledger.test_snapshot(x), snap(11));
        assert_eq!(x.runs.len(), 3);
        // Keeping one run is the plain refresh.
        let single = r1.merge_keeping(&run(11, &[2]), 1);
        assert_eq!(
            single.covered_by(&single.tests[0]).collect::<Vec<_>>(),
            vec![n(2)]
        );
        assert_eq!(single.test_snapshot(&single.tests[0]), snap(11));
        // Round-trips through canonical CBOR.
        let bytes = hord_encoding::encode(&ledger).expect("encode");
        assert_eq!(
            hord_encoding::decode::<CoverageRecord>(&bytes).expect("decode"),
            ledger
        );
    }

    #[test]
    fn a_change_before_the_run_that_executed_it_is_not_stale() {
        use crate::Drift;
        let tc = ObjectId::from_bytes([2; 32]);
        let snap = |n: u8| ObjectId::from_bytes([n; 32]);
        let run = |s: u8, covers: &[u128]| {
            CoverageRecord::new(
                snap(s),
                tc,
                BTreeSet::new(),
                vec![(t("x"), None, covers.iter().copied().map(n).collect(), false)],
            )
        };
        // Run 1 (at s10) entered f(1) on a nondeterministic path; run 2 (at
        // s11) entered only g(2).
        let ledger = run(10, &[1, 2]).merge(&run(11, &[2]));
        let x = &ledger.tests[0];
        // h(3) changes at s11. Only run 2, taken at s11 (after the change),
        // executed it.
        let mut drift = Drift::new(snap(10));
        drift.push(snap(11), [n(3)].into_iter().collect());
        let later = run(10, &[1]).merge(&run(11, &[1, 3]));
        let y = &later.tests[0];
        // A change that predates the run that executed it: not stale.
        assert!(!later.is_stale(y, &drift));
        // The union-from-oldest reading would have selected it.
        let changed = drift
            .changed_since(later.test_snapshot(y))
            .expect("known snapshot");
        assert!(later.covered_by(y).any(|n| changed.contains(&n)));
        // The nondeterministic path still selects: f(1), seen only in run 1,
        // changes after run 1's snapshot.
        drift.push(snap(12), [n(1)].into_iter().collect());
        assert!(ledger.is_stale(x, &drift));
        // Nothing either run executed changed: not stale.
        let mut quiet = Drift::new(snap(10));
        quiet.push(snap(11), BTreeSet::new());
        quiet.push(snap(12), [n(9)].into_iter().collect());
        assert!(!ledger.is_stale(x, &quiet));
        // A run on a snapshot the chain does not know is stale.
        assert!(ledger.is_stale(x, &Drift::new(snap(99))));
    }

    #[test]
    fn staleness_ends_once_the_test_re_ran_on_the_change() {
        use crate::Drift;
        let tc = ObjectId::from_bytes([2; 32]);
        let snap = |n: u8| ObjectId::from_bytes([n; 32]);
        let run = |s: u8, covers: &[u128]| {
            CoverageRecord::new(
                snap(s),
                tc,
                BTreeSet::new(),
                vec![(t("x"), None, covers.iter().copied().map(n).collect(), false)],
            )
        };
        // Run 1 at s10 executed f(1) and core(5); run 2 at s12 executed
        // only core(5).
        let ledger = run(10, &[1, 5]).merge(&run(12, &[5]));
        let x = &ledger.tests[0];
        let chain = |writes: &[(u8, &[u128])]| {
            let mut d = Drift::new(snap(10));
            for (s, w) in writes {
                d.push(snap(*s), w.iter().copied().map(n).collect());
            }
            d
        };
        // Dropped: core(5) changed at s11 and the test re-ran on it at s12.
        // Run by run (the earlier reading), run 1 would keep it stale until
        // it aged out.
        assert!(!ledger.is_stale(x, &chain(&[(11, &[5]), (12, &[])])));
        // Dropped: the nondeterministic path f(1) changed at s11, before the
        // test passed again at s12 without entering it.
        assert!(!ledger.is_stale(x, &chain(&[(11, &[1]), (12, &[])])));
        // Kept: f(1), seen only in run 1, changes after the newest run.
        assert!(ledger.is_stale(x, &chain(&[(11, &[]), (12, &[]), (13, &[1])])));
        // Kept: core(5) changes after the newest run.
        assert!(ledger.is_stale(x, &chain(&[(11, &[]), (12, &[]), (13, &[5])])));
        // Not stale: what changed after the newest run, nothing ran.
        assert!(!ledger.is_stale(x, &chain(&[(11, &[]), (12, &[]), (13, &[9])])));
        // An older run the chain does not know is excused by the newest run.
        let unknown_older = run(3, &[1]).merge(&run(12, &[5]));
        let y = &unknown_older.tests[0];
        assert!(!unknown_older.is_stale(y, &chain(&[(11, &[1]), (12, &[])])));
        // A newest run the chain does not know is stale.
        assert!(ledger.is_stale(x, &Drift::new(snap(99))));
    }

    #[test]
    fn merge_refreshes_the_tests_a_run_touched() {
        let old = record();
        let s2 = ObjectId::from_bytes([9; 32]);
        let run = CoverageRecord::new(
            s2,
            ObjectId::from_bytes([2; 32]),
            [n(5)].into_iter().collect(),
            vec![(t("b"), Some(n(20)), [n(5)].into_iter().collect(), false)],
        );
        let merged = old.merge_keeping(&run, 1);
        assert_eq!(merged.snapshot, s2);
        let b = merged
            .tests
            .iter()
            .find(|x| x.test == t("b"))
            .expect("the merged record keeps every test");
        assert_eq!(merged.test_snapshot(b), s2);
        assert_eq!(merged.covered_by(b).collect::<Vec<_>>(), vec![n(5)]);
        let a = merged
            .tests
            .iter()
            .find(|x| x.test == t("a"))
            .expect("the merged record keeps every test");
        assert_eq!(merged.test_snapshot(a), ObjectId::from_bytes([1; 32]));
        assert_eq!(merged.covered_by(a).collect::<Vec<_>>(), vec![n(3)]);
        assert!(merged.is_instrumented(n(9)) && merged.is_instrumented(n(5)));
        // Merging twice keeps the older test's own snapshot.
        let s3 = ObjectId::from_bytes([8; 32]);
        let empty = CoverageRecord::new(
            s3,
            ObjectId::from_bytes([2; 32]),
            BTreeSet::new(),
            Vec::new(),
        );
        let again = merged.merge_keeping(&empty, 1);
        let a = again
            .tests
            .iter()
            .find(|x| x.test == t("a"))
            .expect("the merged record keeps every test");
        assert_eq!(again.test_snapshot(a), ObjectId::from_bytes([1; 32]));
        let b = again
            .tests
            .iter()
            .find(|x| x.test == t("b"))
            .expect("the merged record keeps every test");
        assert_eq!(again.test_snapshot(b), s2);
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
        let log = r.put(&index).expect("store the coverage record as a log");
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
            .expect("put evidence");
        let snaps = [ObjectId::from_bytes([5; 32]), ObjectId::from_bytes([1; 32])];
        let (_, found) = find_coverage(&index, snaps, ObjectId::from_bytes([2; 32]))
            .expect("find coverage")
            .expect("a coverage record exists for this toolchain");
        assert_eq!(found, r);
        assert!(
            find_coverage(&index, snaps, ObjectId::from_bytes([3; 32]))
                .expect("find coverage")
                .is_none()
        );
    }
}
