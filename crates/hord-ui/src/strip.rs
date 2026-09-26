//! The landing strip's state (spec §10.4 view 1): the lander queue folded
//! from the event stream (spec §10.5.3).
//!
//! One [`Strip`] serves both the live strip and flight-recorder playback:
//! it is a pure fold of [`proto::EventEnvelope`]s, so a recording played to
//! event `k` shows exactly what the live strip showed after that event.

use std::collections::HashMap;

use hord_api::proto;
use hord_api::proto::event::Kind;

/// Where a submission is on the strip: proposed → verifying → landed,
/// replaying, or arbitration (spec §10.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Submitted; waiting for the lander or its conflict check.
    Proposed,
    /// The lander is verifying it.
    Verifying,
    /// The replay harness is working on it (spec §6.6).
    Replaying {
        /// Attempt number, from 1.
        attempt: u32,
        /// Harness name.
        harness: String,
    },
    /// Parked for an arbiter or a reviewer (spec §6.4).
    Parked {
        /// Why.
        reason: proto::ParkReason,
        /// Details, in words.
        detail: String,
    },
    /// An arbiter resolved it (spec §6.4 rung 3).
    Arbitrated {
        /// The resolving change.
        result: String,
    },
    /// Appended to the log.
    Landed {
        /// Position in the landing log, when known (a `Landed` event
        /// carries it; a queue entry does not).
        position: Option<u64>,
    },
    /// Not landed and not replayable as is.
    Rejected {
        /// Why.
        reason: String,
    },
}

impl Stage {
    /// Whether the lander is done with it (landed, rejected, or resolved).
    #[must_use]
    pub fn settled(&self) -> bool {
        matches!(
            self,
            Self::Landed { .. } | Self::Rejected { .. } | Self::Arbitrated { .. }
        )
    }
}

/// One piece of evidence as it arrived on a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceItem {
    /// ObjectId of the Evidence object.
    pub id: String,
    /// Its kind, when the event named one.
    pub kind: Option<proto::EvidenceKind>,
    /// Its result, when the event named one.
    pub result: Option<proto::EvidenceResult>,
    /// Its qualifier (ADR 0026).
    pub qualifier: Option<String>,
}

/// One submission on the strip.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// SubmissionId, when the `Submitted` event was seen.
    pub submission: Option<u64>,
    /// The change as submitted.
    pub change: String,
    /// The id it landed under, when it landed rebased (ADR 0018).
    pub landed: Option<String>,
    /// Its author, when known.
    pub actor: Option<proto::Actor>,
    /// Its one-line intent summary, when known (events do not carry it;
    /// see [`Strip::set_summary`]).
    pub summary: Option<String>,
    /// Current stage.
    pub stage: Stage,
    /// Outcome of the lander's conflict check, once run.
    pub conflict: Option<proto::ConflictCheck>,
    /// The verification plan, once verification started.
    pub plan: Option<proto::VerifyPlanSummary>,
    /// Evidence in arrival order, without duplicates.
    pub evidence: Vec<EvidenceItem>,
    /// Replay attempts seen, in order: (attempt, harness).
    pub replays: Vec<(u32, String)>,
    /// Who arbitrated it, when arbitrated.
    pub arbitrated_by: Option<proto::Actor>,
    /// Cursor of the last event that touched the row.
    pub cursor: u64,
    /// Time of the last event that touched the row.
    pub at_ms: u64,
}

impl Row {
    fn new(change: String) -> Self {
        Self {
            submission: None,
            change,
            landed: None,
            actor: None,
            summary: None,
            stage: Stage::Proposed,
            conflict: None,
            plan: None,
            evidence: Vec::new(),
            replays: Vec::new(),
            arbitrated_by: None,
            cursor: 0,
            at_ms: 0,
        }
    }

    fn add_evidence(&mut self, item: EvidenceItem) {
        match self.evidence.iter_mut().find(|e| e.id == item.id) {
            // `Landed` lists ids only; keep what `EvidenceAttached` said.
            Some(seen) => {
                if item.kind.is_some() {
                    *seen = item;
                }
            }
            None => self.evidence.push(item),
        }
    }

    /// Fold what arrived under a landing candidate's id into this row.
    fn absorb(&mut self, other: Row) {
        if self.plan.is_none() {
            self.plan = other.plan;
        }
        for item in other.evidence {
            self.add_evidence(item);
        }
    }
}

/// The landing strip: every submission seen, in first-seen order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Strip {
    rows: Vec<Row>,
    /// Submitted and landed ids → index in `rows`.
    index: HashMap<String, usize>,
    /// Events about ids not yet tied to a submission: a landing candidate
    /// is verified under the id it will land as (a rebased record, ADR
    /// 0018), which only `Landed { submitted }` ties to its submission.
    candidates: HashMap<String, Row>,
    head: Option<String>,
    cursor: u64,
    at_ms: u64,
}

impl Strip {
    /// An empty strip.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rows in first-seen order (submission order for a stream read from
    /// the start).
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The row for a submitted or landed change id.
    #[must_use]
    pub fn row(&self, change: &str) -> Option<&Row> {
        self.index.get(change).map(|&i| &self.rows[i])
    }

    /// Head after the last `HeadMoved`.
    #[must_use]
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Cursor of the last event applied; 0 before any.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Time of the last event applied; 0 before any.
    #[must_use]
    pub fn at_ms(&self) -> u64 {
        self.at_ms
    }

    /// Label a change's row with its intent summary. Events carry no
    /// summary, so the caller supplies it (from the queue or the change).
    /// Returns whether a row was found.
    pub fn set_summary(&mut self, change: &str, summary: impl Into<String>) -> bool {
        match self.index.get(change) {
            Some(&i) => {
                self.rows[i].summary = Some(summary.into());
                true
            }
            None => false,
        }
    }

    /// Seed or refresh a row from a lander queue entry (`Queue` RPC): its
    /// stage, summary, and author. The live strip starts from the queue and
    /// then follows events, which carry no summary.
    pub fn seed(&mut self, entry: &proto::QueueEntry) {
        let i = self.row_for(&entry.change);
        // A landed entry's `landed` is the same change rebased (ADR 0018);
        // a replayed or arbitrated entry's is the resolution, which has a
        // queue entry and a row of its own.
        let rebased = (entry.status() == proto::QueueStatus::Landed)
            .then(|| entry.landed.clone())
            .flatten()
            .filter(|l| *l != entry.change);
        if let Some(landed) = &rebased {
            self.index.insert(landed.clone(), i);
        }
        let row = &mut self.rows[i];
        row.submission = Some(entry.seq);
        row.landed = rebased;
        if !entry.summary.is_empty() {
            row.summary = Some(entry.summary.clone());
        }
        if entry.actor.is_some() {
            row.actor.clone_from(&entry.actor);
        }
        row.at_ms = row.at_ms.max(entry.updated_at_ms);
        row.stage = match entry.status() {
            proto::QueueStatus::Landed => Stage::Landed {
                position: match row.stage {
                    Stage::Landed { position } => position,
                    _ => None,
                },
            },
            proto::QueueStatus::Conflicted => Stage::Parked {
                reason: if entry
                    .report
                    .as_ref()
                    .is_some_and(|r| r.verification.is_some())
                {
                    proto::ParkReason::VerificationFailed
                } else {
                    proto::ParkReason::MergeConflict
                },
                detail: String::new(),
            },
            proto::QueueStatus::Parked => {
                Stage::Parked {
                    reason: if entry.report.as_ref().is_some_and(|r| {
                        r.policy.iter().any(|p| p.requirement.starts_with("review"))
                    }) {
                        proto::ParkReason::NeedsReview
                    } else {
                        proto::ParkReason::Policy
                    },
                    detail: String::new(),
                }
            }
            proto::QueueStatus::Rejected => Stage::Rejected {
                reason: entry.reason.clone().unwrap_or_default(),
            },
            proto::QueueStatus::Replaying => {
                let last = entry.escalation.as_ref().and_then(|e| e.attempts.last());
                Stage::Replaying {
                    attempt: last.map_or(1, |a| a.attempt),
                    harness: last.map(|a| a.harness.clone()).unwrap_or_default(),
                }
            }
            proto::QueueStatus::NeedsArbitration => Stage::Parked {
                reason: proto::ParkReason::NeedsArbitration,
                detail: String::new(),
            },
            // Resolved by a replay or an arbiter: what landed is the
            // resolution.
            proto::QueueStatus::Replayed | proto::QueueStatus::Arbitrated => Stage::Arbitrated {
                result: entry.landed.clone().unwrap_or_default(),
            },
            // Queued: keep what events said (verifying, replaying).
            proto::QueueStatus::Queued | proto::QueueStatus::Unspecified => row.stage.clone(),
        };
    }

    /// Changes whose rows have no summary yet, in row order.
    #[must_use]
    pub fn unlabeled(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter(|r| r.summary.is_none())
            .map(|r| r.change.as_str())
            .collect()
    }

    /// Apply one event. Returns the index of the row it touched, if any
    /// (for a live region to re-render that row alone). An event whose
    /// cursor is not past [`Self::cursor`] is ignored, so a resumed stream
    /// that repeats events is harmless.
    pub fn apply(&mut self, envelope: &proto::EventEnvelope) -> Option<usize> {
        if envelope.cursor <= self.cursor {
            return None;
        }
        self.cursor = envelope.cursor;
        self.at_ms = envelope.at_ms;
        let kind = envelope.kind()?;
        let touched = match kind {
            Kind::Submitted(s) => {
                let i = self.row_for(&s.change);
                let row = &mut self.rows[i];
                row.submission = Some(s.submission);
                row.actor.clone_from(&s.actor);
                // A resubmission starts over.
                row.stage = Stage::Proposed;
                Some(i)
            }
            Kind::ConflictCheck(c) => {
                let i = self.row_for(&c.change);
                self.rows[i].conflict = Some(c.clone());
                Some(i)
            }
            Kind::Verifying(v) => {
                let (row, i) = self.row_or_candidate(&v.change);
                row.stage = Stage::Verifying;
                row.plan.clone_from(&v.plan);
                i
            }
            Kind::EvidenceAttached(e) => {
                let (row, i) = self.row_or_candidate(&e.change);
                row.add_evidence(EvidenceItem {
                    id: e.evidence.clone(),
                    kind: e.kind.clone(),
                    result: e.result.clone(),
                    qualifier: e.qualifier.clone(),
                });
                i
            }
            Kind::Replaying(r) => {
                let i = self.row_for(&r.change);
                let row = &mut self.rows[i];
                row.stage = Stage::Replaying {
                    attempt: r.attempt,
                    harness: r.harness.clone(),
                };
                row.replays.push((r.attempt, r.harness.clone()));
                Some(i)
            }
            Kind::Parked(p) => {
                let i = self.row_for(&p.change);
                self.rows[i].stage = Stage::Parked {
                    reason: p.reason(),
                    detail: p.detail.clone(),
                };
                Some(i)
            }
            Kind::Arbitrated(a) => {
                let i = self.row_for(&a.change);
                let row = &mut self.rows[i];
                row.stage = Stage::Arbitrated {
                    result: a.result.clone(),
                };
                row.arbitrated_by.clone_from(&a.by);
                Some(i)
            }
            Kind::Landed(l) => {
                let submitted = l.submitted.as_deref().unwrap_or(&l.change);
                let i = self.row_for(submitted);
                if l.change != submitted {
                    self.rows[i].landed = Some(l.change.clone());
                    self.index.insert(l.change.clone(), i);
                }
                if let Some(candidate) = self.candidates.remove(&l.change) {
                    self.rows[i].absorb(candidate);
                }
                let row = &mut self.rows[i];
                row.stage = Stage::Landed {
                    position: Some(l.position),
                };
                for id in &l.evidence {
                    row.add_evidence(EvidenceItem {
                        id: id.clone(),
                        kind: None,
                        result: None,
                        qualifier: None,
                    });
                }
                Some(i)
            }
            Kind::Rejected(r) => {
                let i = self.row_for(&r.change);
                self.rows[i].stage = Stage::Rejected {
                    reason: r.reason.clone(),
                };
                Some(i)
            }
            Kind::HeadMoved(h) => {
                self.head = Some(h.to.clone());
                None
            }
            // Not a change: the strip shows no git bridge checks.
            Kind::BridgeChecked(_) => None,
        };
        if let Some(i) = touched {
            self.rows[i].cursor = envelope.cursor;
            self.rows[i].at_ms = envelope.at_ms;
        }
        touched
    }

    /// Index of the row for `change`, created if new.
    fn row_for(&mut self, change: &str) -> usize {
        if let Some(&i) = self.index.get(change) {
            return i;
        }
        let mut row = Row::new(change.to_owned());
        if let Some(candidate) = self.candidates.remove(change) {
            row.absorb(candidate);
            row.stage = candidate_stage(&row);
        }
        self.rows.push(row);
        let i = self.rows.len() - 1;
        self.index.insert(change.to_owned(), i);
        i
    }

    /// The row for a submitted or landed id, else the pending candidate
    /// under that id (with no row index).
    fn row_or_candidate(&mut self, change: &str) -> (&mut Row, Option<usize>) {
        match self.index.get(change) {
            Some(&i) => (&mut self.rows[i], Some(i)),
            None => (
                self.candidates
                    .entry(change.to_owned())
                    .or_insert_with(|| Row::new(change.to_owned())),
                None,
            ),
        }
    }
}

/// A row made from a candidate's events alone has been verifying.
fn candidate_stage(row: &Row) -> Stage {
    if row.plan.is_some() {
        Stage::Verifying
    } else {
        Stage::Proposed
    }
}

#[cfg(test)]
mod tests {
    use hord_api::wire;

    use super::*;

    fn env(cursor: u64, kind: Kind) -> proto::EventEnvelope {
        proto::EventEnvelope {
            cursor,
            at_ms: cursor * 10,
            event: Some(wire::event(kind)),
        }
    }

    fn agent(id: &str) -> proto::Actor {
        proto::Actor {
            kind: Some(proto::actor::Kind::Agent(proto::Agent {
                id: id.into(),
                ..Default::default()
            })),
        }
    }

    fn submitted(cursor: u64, change: &str) -> proto::EventEnvelope {
        env(
            cursor,
            Kind::Submitted(proto::Submitted {
                submission: cursor,
                change: change.into(),
                actor: Some(agent("a")),
                voucher: None,
            }),
        )
    }

    fn pass() -> Option<proto::EvidenceResult> {
        Some(proto::EvidenceResult {
            result: Some(proto::evidence_result::Result::Pass(true)),
        })
    }

    #[test]
    fn a_rebased_landing_folds_its_candidate_into_the_submission() {
        let mut strip = Strip::new();
        strip.apply(&submitted(1, "s1"));
        assert_eq!(strip.rows()[0].stage, Stage::Proposed);
        // Verified under the id it will land as.
        assert_eq!(
            strip.apply(&env(
                2,
                Kind::Verifying(proto::Verifying {
                    change: "l1".into(),
                    plan: Some(proto::VerifyPlanSummary::default()),
                }),
            )),
            None
        );
        strip.apply(&env(
            3,
            Kind::EvidenceAttached(proto::EvidenceAttached {
                change: "l1".into(),
                evidence: "e1".into(),
                result: pass(),
                ..Default::default()
            }),
        ));
        assert_eq!(strip.rows().len(), 1, "a candidate is not a row");
        strip.apply(&env(
            4,
            Kind::Landed(proto::Landed {
                change: "l1".into(),
                position: 0,
                submitted: Some("s1".into()),
                evidence: vec!["e1".into(), "e2".into()],
            }),
        ));
        let row = strip.row("l1").expect("the landed id names the row");
        assert_eq!(row.change, "s1");
        assert_eq!(row.stage, Stage::Landed { position: Some(0) });
        assert!(row.plan.is_some());
        let ids: Vec<_> = row.evidence.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["e1", "e2"]);
        assert_eq!(row.evidence[0].result, pass(), "the richer record stays");
        assert_eq!(row.cursor, 4);
    }

    #[test]
    fn parked_replayed_and_arbitrated_rows_track_the_ladder() {
        let mut strip = Strip::new();
        strip.apply(&submitted(1, "s"));
        strip.apply(&env(
            2,
            Kind::Parked(proto::Parked {
                change: "s".into(),
                reason: proto::ParkReason::MergeConflict.into(),
                detail: "f".into(),
            }),
        ));
        assert!(matches!(
            strip.rows()[0].stage,
            Stage::Parked {
                reason: proto::ParkReason::MergeConflict,
                ..
            }
        ));
        for attempt in 1..=2 {
            strip.apply(&env(
                2 + u64::from(attempt),
                Kind::Replaying(proto::Replaying {
                    change: "s".into(),
                    attempt,
                    harness: "ref".into(),
                }),
            ));
        }
        strip.apply(&env(
            5,
            Kind::Arbitrated(proto::Arbitrated {
                change: "s".into(),
                by: Some(agent("human")),
                result: "r".into(),
                ..Default::default()
            }),
        ));
        let row = &strip.rows()[0];
        assert_eq!(row.replays.len(), 2);
        assert_eq!(row.stage, Stage::Arbitrated { result: "r".into() });
        assert!(row.stage.settled());
    }

    #[test]
    fn queue_entries_seed_rows_that_events_then_advance() {
        let mut strip = Strip::new();
        strip.seed(&proto::QueueEntry {
            seq: 4,
            change: "s".into(),
            status: proto::QueueStatus::Queued.into(),
            summary: "Add a flag".into(),
            actor: Some(agent("a")),
            ..Default::default()
        });
        strip.seed(&proto::QueueEntry {
            seq: 5,
            change: "p".into(),
            status: proto::QueueStatus::Parked.into(),
            report: Some(proto::ConflictReport {
                policy: vec![proto::PolicyViolation {
                    requirement: "review:human".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(strip.rows()[0].summary.as_deref(), Some("Add a flag"));
        assert!(matches!(
            strip.rows()[1].stage,
            Stage::Parked {
                reason: proto::ParkReason::NeedsReview,
                ..
            }
        ));
        strip.apply(&env(
            9,
            Kind::Landed(proto::Landed {
                change: "l".into(),
                position: 3,
                submitted: Some("s".into()),
                evidence: Vec::new(),
            }),
        ));
        let row = strip.row("l").expect("landed id is indexed");
        assert_eq!(row.stage, Stage::Landed { position: Some(3) });
        assert_eq!(row.summary.as_deref(), Some("Add a flag"));
    }

    #[test]
    fn a_resolution_keeps_its_own_row() {
        let mut strip = Strip::new();
        strip.seed(&proto::QueueEntry {
            seq: 1,
            change: "parked".into(),
            status: proto::QueueStatus::Arbitrated.into(),
            landed: Some("resolution".into()),
            summary: "Make two twenty-two".into(),
            ..Default::default()
        });
        strip.seed(&proto::QueueEntry {
            seq: 2,
            change: "resolution".into(),
            status: proto::QueueStatus::Landed.into(),
            summary: "Arbitrate: take it".into(),
            ..Default::default()
        });
        assert_eq!(strip.rows().len(), 2);
        assert_eq!(
            strip.rows()[0].stage,
            Stage::Arbitrated {
                result: "resolution".into()
            }
        );
        assert_eq!(
            strip.rows()[1].summary.as_deref(),
            Some("Arbitrate: take it")
        );
    }

    #[test]
    fn repeated_cursors_are_ignored_and_summaries_label_rows() {
        let mut strip = Strip::new();
        strip.apply(&submitted(1, "s"));
        assert_eq!(strip.apply(&submitted(1, "t")), None);
        assert_eq!(strip.rows().len(), 1);
        assert_eq!(strip.unlabeled(), ["s"]);
        assert!(strip.set_summary("s", "Fix the parser"));
        assert!(!strip.set_summary("t", "x"));
        assert!(strip.unlabeled().is_empty());
        strip.apply(&env(
            2,
            Kind::HeadMoved(proto::HeadMoved {
                from: None,
                to: "h".into(),
            }),
        ));
        assert_eq!(strip.head(), Some("h"));
        assert_eq!(strip.cursor(), 2);
    }
}
