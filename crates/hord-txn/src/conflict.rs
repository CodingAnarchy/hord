//! Serializability check (spec §6.3) and the [`ConflictReport`].

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{ChangeId, ChangeRecord, NodeId, RepoPath, SnapshotId};
use serde::{Deserialize, Serialize};

use crate::files::coarse_paths;

/// Which rule of spec §6.3 a [`SetConflict`] broke.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    /// Both changes wrote the same definition or blob-tier path.
    WriteWrite,
    /// The change read something a landed change wrote.
    ReadWrite,
    /// The change wrote something a landed change read. Reported only with
    /// [`crate::RepoConfig::strict_reads`].
    WriteRead,
}

/// Overlap between the change and one landed change (spec §6.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SetConflict {
    /// Rule that fired.
    pub kind: ConflictKind,
    /// Landed change it collided with.
    pub landed: ChangeId,
    /// Definitions in the intersection.
    pub nodes: Vec<NodeId>,
    /// Files in the intersection: blob-tier or coarse whole-file writes, and
    /// file roots (glue edits, ADR 0015).
    pub paths: Vec<RepoPath>,
}

/// Hard or soft outcome of a structural merge (spec §5.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MergeSeverity {
    /// Could not be merged; the change does not land.
    Hard,
    /// Merged by a deterministic tie-break; flagged for the verifier.
    Soft,
}

/// One file's structural-rebase conflict (spec §5.2, §6.4 rung 1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergeConflict {
    /// File that was merged.
    pub path: RepoPath,
    /// Hard (parked) or soft (landed, flagged).
    pub severity: MergeSeverity,
    /// Definitions in contention, when the merge names them.
    pub nodes: Vec<NodeId>,
    /// Why, in words.
    pub reason: String,
}

/// A file that a purpose-built adapter merge (for example a lockfile merge,
/// ADR 0013) resolved during the rebase, with no hard or soft conflict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AdapterMerge {
    /// The merged file.
    pub path: RepoPath,
    /// Ids that belong to the file: its root and every definition it has at
    /// the change's base, at head, or in the change's result.
    pub nodes: Vec<NodeId>,
}

/// Machine-readable account of a change's conflicts (`hord conflicts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictReport {
    /// The change that was checked (as submitted).
    pub change: ChangeId,
    /// Its base snapshot.
    pub base: SnapshotId,
    /// Head the check ran against; `None` on an empty log.
    pub head: Option<ChangeId>,
    /// Changes landed after the base, in landing order (spec §6.3 `L`).
    pub checked_against: Vec<ChangeId>,
    /// Whether write-read conflicts were checked.
    pub strict_reads: bool,
    /// Set overlaps with landed changes.
    pub conflicts: Vec<SetConflict>,
    /// Per-file structural-rebase outcomes other than a clean merge.
    pub merge: Vec<MergeConflict>,
    /// Why verification failed, if it did (a semantic conflict, spec §6.5).
    pub verification: Option<String>,
    /// Files resolved by a purpose-built adapter merge with no conflict.
    /// Empty for reports recorded before this field existed.
    #[serde(default)]
    pub adapter_merged: Vec<AdapterMerge>,
    /// Why head's policy denied the change (ADR 0026): every unmet
    /// requirement, machine-readable. Empty when it allowed it or was not
    /// reached.
    #[serde(default)]
    pub policy: Vec<hord_policy::Violation>,
}

impl ConflictReport {
    /// A report on `change` (based on `base`) with nothing found yet.
    pub(crate) fn empty(change: ChangeId, base: SnapshotId) -> Self {
        Self {
            change,
            base,
            head: None,
            checked_against: Vec::new(),
            strict_reads: false,
            conflicts: Vec::new(),
            merge: Vec::new(),
            verification: None,
            adapter_merged: Vec::new(),
            policy: Vec::new(),
        }
    }

    /// No set overlap, no merge conflict, no verification failure, no
    /// policy denial.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
            && self.merge.is_empty()
            && self.verification.is_none()
            && self.policy.is_empty()
    }

    /// Whether any merge conflict is hard.
    #[must_use]
    pub fn has_hard(&self) -> bool {
        self.merge.iter().any(|m| m.severity == MergeSeverity::Hard)
    }

    /// Every definition named anywhere in the report.
    #[must_use]
    pub fn nodes(&self) -> BTreeSet<NodeId> {
        self.conflicts
            .iter()
            .flat_map(|c| c.nodes.iter().copied())
            .chain(self.merge.iter().flat_map(|m| m.nodes.iter().copied()))
            .collect()
    }

    /// Whether every set conflict lies in files an adapter merge resolved
    /// (each node is one of those files' ids, each path one of those files)
    /// and no soft merge conflict is outside them (ADR 0013 fail-closed
    /// exemption). Vacuously true for a clean report.
    #[must_use]
    pub fn only_adapter_merged(&self) -> bool {
        let paths: BTreeSet<&RepoPath> = self.adapter_merged.iter().map(|m| &m.path).collect();
        let nodes: BTreeSet<NodeId> = self
            .adapter_merged
            .iter()
            .flat_map(|m| m.nodes.iter().copied())
            .collect();
        self.conflicts.iter().all(|c| {
            c.nodes.iter().all(|n| nodes.contains(n)) && c.paths.iter().all(|p| paths.contains(p))
        }) && self.merge.iter().all(|m| paths.contains(&m.path))
    }

    /// Whether any overlap of `kind` was found.
    #[must_use]
    pub fn has(&self, kind: ConflictKind) -> bool {
        self.conflicts.iter().any(|c| c.kind == kind)
    }
}

/// Sets of one change as the check compares them.
#[derive(Clone, Debug)]
pub(crate) struct Footprint {
    pub change: ChangeId,
    pub result: SnapshotId,
    pub reads: BTreeSet<NodeId>,
    /// `write_set` plus path ids of coarse writes.
    pub writes: BTreeSet<NodeId>,
    /// Path ids of coarse (whole-file) writes. A Tier 0 import has only these.
    pub coarse: BTreeSet<NodeId>,
    /// Path ids of every file the change modified (tree diff), coarse or not.
    pub touched: BTreeSet<NodeId>,
    /// Path ids → path, for reports.
    pub labels: BTreeMap<NodeId, RepoPath>,
}

impl Footprint {
    /// `touched` is every file `base` → `result` changed.
    pub fn of(change: ChangeId, record: &ChangeRecord, touched: &[RepoPath]) -> Self {
        let mut labels = BTreeMap::new();
        let mut ids = |path: &RepoPath| {
            let id = NodeId::file_root(path);
            labels.insert(id, path.clone());
            id
        };
        let touched: BTreeSet<NodeId> = touched.iter().map(&mut ids).collect();
        let coarse: BTreeSet<NodeId> = coarse_paths(record).iter().map(&mut ids).collect();
        let mut writes = record.write_set.clone();
        writes.extend(coarse.iter().copied());
        Self {
            change,
            result: record.result,
            reads: record.read_set.clone(),
            writes,
            coarse,
            touched,
            labels,
        }
    }
}

/// Compare `change` with each landed change in `landed` (spec §6.3).
///
/// Write-write is `write_set` against `write_set` (a glue edit writes the
/// file root, so two glue edits of one file collide), plus a coarse write on
/// either side against any modification of the same file on the other.
pub(crate) fn check(
    change: &Footprint,
    landed: &[std::sync::Arc<Footprint>],
    strict: bool,
) -> Vec<SetConflict> {
    let mut out = Vec::new();
    for other in landed {
        let labels = |ids: BTreeSet<NodeId>| -> (Vec<NodeId>, Vec<RepoPath>) {
            let mut nodes = Vec::new();
            let mut paths = BTreeSet::new();
            for id in ids {
                match other.labels.get(&id).or_else(|| change.labels.get(&id)) {
                    Some(path) => {
                        paths.insert(path.clone());
                    }
                    None => nodes.push(id),
                }
            }
            (nodes, paths.into_iter().collect())
        };
        let mut ww: BTreeSet<NodeId> = change.writes.intersection(&other.writes).copied().collect();
        ww.extend(change.touched.intersection(&other.coarse));
        ww.extend(change.coarse.intersection(&other.touched));
        // Read overlaps that are not already write-write.
        let beyond_ww = |a: &BTreeSet<NodeId>, b: &BTreeSet<NodeId>| -> BTreeSet<NodeId> {
            a.intersection(b)
                .filter(|id| !ww.contains(id))
                .copied()
                .collect()
        };
        let rw = beyond_ww(&change.reads, &other.writes);
        let wr = if strict {
            beyond_ww(&change.writes, &other.reads)
        } else {
            BTreeSet::new()
        };
        for (kind, ids) in [
            (ConflictKind::WriteWrite, ww),
            (ConflictKind::ReadWrite, rw),
            (ConflictKind::WriteRead, wr),
        ] {
            if !ids.is_empty() {
                let (nodes, paths) = labels(ids);
                out.push(SetConflict {
                    kind,
                    landed: other.change,
                    nodes,
                    paths,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hord_core::{Actor, Intent, ObjectId, Op, Provenance, Timestamp};

    use super::*;

    fn nid(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    fn record(reads: &[u128], writes: &[u128], ops: Vec<Op>) -> ChangeRecord {
        ChangeRecord {
            base: ObjectId::from_bytes([1; 32]),
            result: ObjectId::from_bytes([2; 32]),
            parents: Vec::new(),
            ops,
            intent: Intent::from_summary(""),
            provenance: Provenance {
                actor: Actor::Human { id: "t".into() },
                toolchain: ObjectId::from_bytes([3; 32]),
                created_at: Timestamp::from_millis(0),
                session: None,
                parent_intent: None,
            },
            read_set: reads.iter().copied().map(nid).collect(),
            write_set: writes.iter().copied().map(nid).collect(),
            identity_deltas: Vec::new(),
            evidence: Vec::new(),
            signature: None,
            rebased_from: None,
        }
    }

    fn fp(n: u8, record: &ChangeRecord, touched: &[&str]) -> Footprint {
        let touched: Vec<RepoPath> = touched
            .iter()
            .map(|p| p.parse().expect("parse test path literal"))
            .collect();
        Footprint::of(ObjectId::from_bytes([n; 32]), record, &touched)
    }

    fn blob(path: &str) -> Op {
        Op::Blob {
            path: path.parse().expect("parse test path literal"),
            from: Some(ObjectId::from_bytes([4; 32])),
            to: Some(ObjectId::from_bytes([5; 32])),
        }
    }

    fn replace(n: u128) -> Op {
        Op::Replace {
            node: nid(n),
            from: ObjectId::from_bytes([0; 32]),
            to: ObjectId::from_bytes([0; 32]),
        }
    }

    #[test]
    fn disjoint_sets_do_not_conflict() {
        let a = fp(1, &record(&[1], &[2], vec![replace(2)]), &["a.rs"]);
        let b = fp(2, &record(&[3], &[4], vec![replace(4)]), &["a.rs"]);
        assert!(check(&a, &[Arc::new(b)], true).is_empty());
    }

    #[test]
    fn each_rule_fires() {
        let change = fp(1, &record(&[5], &[2], vec![replace(2)]), &["a.rs"]);
        let ww = fp(2, &record(&[], &[2], vec![replace(2)]), &["a.rs"]);
        let rw = fp(3, &record(&[], &[5], vec![replace(5)]), &["b.rs"]);
        let wr = fp(4, &record(&[2], &[9], vec![replace(9)]), &["c.rs"]);
        let landed = [Arc::new(ww), Arc::new(rw), Arc::new(wr)];
        let kinds: Vec<_> = check(&change, &landed, false)
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, [ConflictKind::WriteWrite, ConflictKind::ReadWrite]);
        let kinds: Vec<_> = check(&change, &landed, true)
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ConflictKind::WriteWrite,
                ConflictKind::ReadWrite,
                ConflictKind::WriteRead
            ]
        );
    }

    #[test]
    fn coarse_write_conflicts_with_any_touch_of_the_file() -> Result<(), Box<dyn std::error::Error>>
    {
        // A Tier 0 import that rewrote a.rs as a blob.
        let import = fp(2, &record(&[], &[], vec![blob("a.rs")]), &["a.rs"]);
        let change = fp(1, &record(&[], &[7], vec![replace(7)]), &["a.rs"]);
        for found in [
            check(&change, &[Arc::new(import.clone())], false),
            check(&import, &[Arc::new(change)], false),
        ] {
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].kind, ConflictKind::WriteWrite);
            assert_eq!(found[0].paths, vec!["a.rs".parse::<RepoPath>()?]);
            assert!(found[0].nodes.is_empty());
        }
        Ok(())
    }

    #[test]
    fn glue_edits_conflict_only_with_glue_edits() -> Result<(), Box<dyn std::error::Error>> {
        let path: RepoPath = "a.rs".parse()?;
        let root = NodeId::file_root(&path);
        let glue = |n: u8| {
            let record = ChangeRecord {
                write_set: [root].into(),
                ..record(&[], &[], vec![])
            };
            fp(n, &record, &["a.rs"])
        };
        let def_edit = fp(3, &record(&[], &[7], vec![replace(7)]), &["a.rs"]);
        assert!(check(&def_edit, &[Arc::new(glue(1))], true).is_empty());
        let found = check(&glue(2), &[Arc::new(glue(1))], false);
        assert_eq!(found[0].kind, ConflictKind::WriteWrite);
        assert_eq!(found[0].paths, vec![path]);
        Ok(())
    }
}
