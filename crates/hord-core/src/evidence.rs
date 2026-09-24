//! Verification evidence (spec §3.6).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Actor, NodeId, ObjectId, SnapshotId, Timestamp};

/// A verification result keyed to an exact snapshot and toolchain.
///
/// Evidence is valid for a snapshot, not a change. When a change lands on a
/// different base, evidence is stale.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Evidence {
    /// Kind of check this object records.
    pub kind: EvidenceKind,
    /// Refinement of [`Self::kind`] that policy requirements name after a
    /// `:` (ADR 0026): `selected` or `full` for a test run, the reviewer
    /// kind from `hord review --as <kind>`, `no-regression` for a bench.
    /// Omitted from the encoding when `None`, so evidence without one keeps
    /// its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    /// Snapshot that was verified.
    pub snapshot: SnapshotId,
    /// Toolchain the check ran under.
    pub toolchain: ObjectId,
    /// Command that produced this evidence.
    pub command: String,
    /// Nodes this evidence is claimed to cover, if scoped.
    pub scope: Option<BTreeSet<NodeId>>,
    /// Pass, fail, or skip.
    pub result: EvidenceResult,
    /// [`crate::Blob`] of captured output, if stored.
    pub log: Option<ObjectId>,
    /// Wall-clock cost of producing this evidence, in milliseconds.
    pub cost_ms: u64,
    /// Who produced this evidence.
    pub produced_by: Actor,
    /// When this evidence was produced.
    pub produced_at: Timestamp,
}

/// Kind of [`Evidence`] (spec §3.6).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum EvidenceKind {
    /// Type-check / compile (`cargo check`, …).
    Check,
    /// Test run.
    Test,
    /// Benchmark.
    Bench,
    /// Linter (`clippy`, …).
    Lint,
    /// Human or agent review, signed against the snapshot.
    Review,
    /// Adapter- or policy-defined kind.
    Custom(String),
    /// The lander's attestation that it rebased `submitted` onto the head
    /// it landed on (ADR 0018). [`Evidence::snapshot`] is the landed result,
    /// and the landed record, whose `rebased_from` is `submitted`, lists
    /// this evidence. Unsigned until M5 adds lander keys.
    Rebase {
        /// The record the author submitted.
        submitted: crate::ChangeId,
    },
}

/// Outcome recorded on [`Evidence`].
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum EvidenceResult {
    /// The check succeeded.
    Pass,
    /// The check failed.
    Fail {
        /// Short summary of the failure.
        summary: String,
    },
    /// The check was not run.
    Skipped {
        /// Why it was skipped.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`Evidence`] as it was before `qualifier` (ADR 0026).
    #[derive(Serialize)]
    struct EvidenceV0 {
        kind: EvidenceKind,
        snapshot: SnapshotId,
        toolchain: ObjectId,
        command: String,
        scope: Option<BTreeSet<NodeId>>,
        result: EvidenceResult,
        log: Option<ObjectId>,
        cost_ms: u64,
        produced_by: Actor,
        produced_at: Timestamp,
    }

    fn sample(qualifier: Option<&str>) -> Evidence {
        Evidence {
            kind: EvidenceKind::Test,
            qualifier: qualifier.map(str::to_owned),
            snapshot: ObjectId::from_canonical(b"snapshot"),
            toolchain: ObjectId::from_canonical(b"toolchain"),
            command: "cargo test".into(),
            scope: Some(BTreeSet::from([NodeId::from_u128(7)])),
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 12,
            produced_by: Actor::Human { id: "ada".into() },
            produced_at: Timestamp::from_millis(1),
        }
    }

    #[test]
    fn no_qualifier_is_omitted_from_the_encoding() {
        let ev = sample(None);
        let v0 = EvidenceV0 {
            kind: ev.kind.clone(),
            snapshot: ev.snapshot,
            toolchain: ev.toolchain,
            command: ev.command.clone(),
            scope: ev.scope.clone(),
            result: ev.result.clone(),
            log: ev.log,
            cost_ms: ev.cost_ms,
            produced_by: ev.produced_by.clone(),
            produced_at: ev.produced_at,
        };
        let bytes = hord_encoding::encode(&ev).unwrap();
        assert_eq!(bytes, hord_encoding::encode(&v0).unwrap());
        let back: Evidence = hord_encoding::decode(&bytes).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn a_qualifier_is_encoded_and_changes_the_id() {
        let plain = sample(None);
        let selected = sample(Some("selected"));
        let bytes = hord_encoding::encode(&selected).unwrap();
        assert!(bytes.windows(9).any(|w| w == b"qualifier"));
        assert_ne!(bytes, hord_encoding::encode(&plain).unwrap());
        let back: Evidence = hord_encoding::decode(&bytes).unwrap();
        assert_eq!(back, selected);
    }
}
