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
