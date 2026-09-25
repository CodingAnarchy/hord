//! The one place verification builds [`Evidence`] values, qualifier
//! included (ADR 0026).

use std::collections::BTreeSet;

use hord_core::{
    Actor, Evidence, EvidenceKind, EvidenceResult, NodeId, ObjectId, SnapshotId, Timestamp,
};

/// Fields of an [`Evidence`] object, by name.
#[derive(Clone, Debug)]
pub struct EvidenceFields {
    /// Kind of check.
    pub kind: EvidenceKind,
    /// Requirement qualifier (ADR 0026): `selected` or `full` for tests,
    /// `None` for unqualified evidence.
    pub qualifier: Option<String>,
    /// Snapshot verified.
    pub snapshot: SnapshotId,
    /// Toolchain object id.
    pub toolchain: ObjectId,
    /// Command that produced it.
    pub command: String,
    /// What it claims to cover.
    pub scope: Option<BTreeSet<NodeId>>,
    /// Outcome.
    pub result: EvidenceResult,
    /// Log blob.
    pub log: Option<ObjectId>,
    /// Cost in milliseconds.
    pub cost_ms: u64,
    /// Producer.
    pub produced_by: Actor,
    /// When.
    pub produced_at: Timestamp,
}

impl EvidenceFields {
    /// The [`Evidence`] object.
    #[must_use]
    pub fn build(self) -> Evidence {
        Evidence {
            kind: self.kind,
            qualifier: self.qualifier,
            snapshot: self.snapshot,
            toolchain: self.toolchain,
            command: self.command,
            scope: self.scope,
            result: self.result,
            log: self.log,
            cost_ms: self.cost_ms,
            produced_by: self.produced_by,
            produced_at: self.produced_at,
        }
    }
}
