//! What changed since a snapshot, for per-test coverage freshness
//! (ADR 0022, amendments after the 50-commit measurement).

use std::collections::BTreeSet;

use hord_core::{NodeId, SnapshotId};
use serde::{Deserialize, Serialize};

/// A chain of snapshots in landing order, each with the nodes the change
/// that produced it wrote. A test whose coverage was taken on snapshot `S`
/// can have changed behavior only if a definition it executed is in
/// [`Drift::changed_since`]`(S)`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Drift {
    /// The first snapshot (nothing changed before it that the chain knows).
    start: Option<SnapshotId>,
    /// `(result snapshot, nodes written to reach it)`, oldest first.
    steps: Vec<(SnapshotId, BTreeSet<NodeId>)>,
}

impl Drift {
    /// A chain that starts at `snapshot`.
    #[must_use]
    pub fn new(snapshot: SnapshotId) -> Self {
        Self {
            start: Some(snapshot),
            steps: Vec::new(),
        }
    }

    /// Append a landed change: it produced `snapshot` by writing `written`
    /// (its write set: definitions and file roots edited, born, or died).
    pub fn push(&mut self, snapshot: SnapshotId, written: BTreeSet<NodeId>) {
        self.steps.push((snapshot, written));
    }

    /// The newest snapshot of the chain.
    #[must_use]
    pub fn head(&self) -> Option<SnapshotId> {
        self.steps.last().map(|(s, _)| *s).or(self.start)
    }

    /// Every node written after `snapshot`, up to the head. `None` when the
    /// chain does not contain `snapshot`: the caller must then treat
    /// everything as changed (select the test).
    #[must_use]
    pub fn changed_since(&self, snapshot: SnapshotId) -> Option<BTreeSet<NodeId>> {
        let from = if self.start == Some(snapshot) {
            0
        } else {
            self.steps.iter().rposition(|(s, _)| *s == snapshot)? + 1
        };
        Some(
            self.steps[from..]
                .iter()
                .flat_map(|(_, w)| w.iter().copied())
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use hord_core::ObjectId;

    use super::*;

    fn s(n: u8) -> SnapshotId {
        ObjectId::from_bytes([n; 32])
    }

    fn ids(v: &[u128]) -> BTreeSet<NodeId> {
        v.iter().copied().map(NodeId::from_u128).collect()
    }

    #[test]
    fn changed_since_unions_later_writes() {
        let mut d = Drift::new(s(0));
        d.push(s(1), ids(&[1]));
        d.push(s(2), ids(&[2, 3]));
        assert_eq!(d.changed_since(s(0)), Some(ids(&[1, 2, 3])));
        assert_eq!(d.changed_since(s(1)), Some(ids(&[2, 3])));
        assert_eq!(d.changed_since(s(2)), Some(BTreeSet::new()));
        assert_eq!(d.changed_since(s(7)), None);
        assert_eq!(d.head(), Some(s(2)));
    }
}
