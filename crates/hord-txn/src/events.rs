//! The repository's event log (spec §10.5.3): lander and verifier events,
//! each with a monotonic, persisted [`EventCursor`].
//!
//! Events live in `.hord/events.redb`, apart from the store's index: they
//! are derived and never read by landing, so losing the tail loses no
//! repository state. Appends commit without fsync and the log is made
//! durable when the lander goes idle and on drop, so the cost per landing
//! is one small redb commit. A subscriber reads the recorded events after
//! its cursor, then follows live ones through a broadcast channel; a
//! subscriber that falls behind the channel re-reads from the log.

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use hord_api::proto::event::Kind;
use hord_api::{ApiError, EventCursor, EventStream, proto, wire};
use hord_core::{Actor, ChangeId, Evidence, ObjectId};
use prost::Message;
use redb::{Database, Durability, ReadableTable, TableDefinition};
use tokio::sync::{broadcast, mpsc};

use crate::conflict::{ConflictReport, MergeSeverity};
use crate::escalation::Arbiter;
use crate::lander::{QueueEntry, QueueStatus};
use crate::repo::{Inner, lock, now};
use crate::{Error, Result};

/// File name of the event log under `.hord/`.
pub(crate) const EVENTS_FILE: &str = "events.redb";

const EVENTS: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("events");

/// Live events buffered per subscriber before it must re-read the log.
const LIVE_BUFFER: usize = 1024;
/// Recorded events read per batch while replaying a subscriber's backlog.
const REPLAY_BATCH: usize = 512;

fn db_err(err: impl std::fmt::Display) -> Error {
    Error::EventLog(err.to_string())
}

/// The persisted event log and its live fan-out.
pub(crate) struct EventLog {
    db: Database,
    /// Cursor of the last recorded event (0: none). Held while appending
    /// and broadcasting, so live order is cursor order.
    last: Mutex<EventCursor>,
    live: broadcast::Sender<proto::EventEnvelope>,
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog")
            .field("last", &*lock(&self.last))
            .finish_non_exhaustive()
    }
}

impl EventLog {
    /// Open (or create) `<hord_dir>/events.redb`.
    pub(crate) fn open(hord_dir: &Path) -> Result<Self> {
        let db = Database::create(hord_dir.join(EVENTS_FILE)).map_err(db_err)?;
        let txn = db.begin_write().map_err(db_err)?;
        let last = txn
            .open_table(EVENTS)
            .map_err(db_err)?
            .last()
            .map_err(db_err)?
            .map_or(0, |(k, _)| k.value());
        txn.commit().map_err(db_err)?;
        let (live, _) = broadcast::channel(LIVE_BUFFER);
        Ok(Self {
            db,
            last: Mutex::new(last),
            live,
        })
    }

    /// Record `events` in one commit, in order, and broadcast them.
    pub(crate) fn append(
        &self,
        events: Vec<proto::event::Kind>,
    ) -> Result<Vec<proto::EventEnvelope>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let mut last = lock(&self.last);
        let at_ms = now().as_millis();
        let envelopes: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(i, kind)| proto::EventEnvelope {
                cursor: *last + 1 + i as u64,
                at_ms,
                event: Some(hord_api::wire::event(kind)),
            })
            .collect();
        let mut txn = self.db.begin_write().map_err(db_err)?;
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(EVENTS).map_err(db_err)?;
            for envelope in &envelopes {
                table
                    .insert(envelope.cursor, envelope.encode_to_vec().as_slice())
                    .map_err(db_err)?;
            }
        }
        txn.commit().map_err(db_err)?;
        *last = envelopes.last().map_or(*last, |e| e.cursor);
        for envelope in &envelopes {
            // No subscribers is not an error.
            let _ = self.live.send(envelope.clone());
        }
        Ok(envelopes)
    }

    /// Make every recorded event durable.
    pub(crate) fn sync(&self) -> Result<()> {
        let _last = lock(&self.last);
        let mut txn = self.db.begin_write().map_err(db_err)?;
        txn.set_durability(Durability::Immediate);
        txn.commit().map_err(db_err)
    }

    /// Cursor of the last recorded event; 0 when there is none.
    pub(crate) fn last(&self) -> EventCursor {
        *lock(&self.last)
    }

    /// Up to `limit` recorded events with a cursor greater than `after`.
    pub(crate) fn read_after(
        &self,
        after: EventCursor,
        limit: usize,
    ) -> Result<Vec<proto::EventEnvelope>> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let table = txn.open_table(EVENTS).map_err(db_err)?;
        let mut out = Vec::new();
        for row in table.range(after.saturating_add(1)..).map_err(db_err)? {
            if out.len() >= limit {
                break;
            }
            let (_, value) = row.map_err(db_err)?;
            let envelope = proto::EventEnvelope::decode(value.value())
                .map_err(|err| Error::EventLog(format!("undecodable event: {err}")))?;
            out.push(envelope);
        }
        Ok(out)
    }

    /// A stream of the events after `from` (every recorded one first), or
    /// only live events when `from` is `None`. The stream holds the log
    /// weakly: it ends when the log is dropped, and it stops following
    /// when the stream is dropped.
    pub(crate) fn subscribe(self: &Arc<Self>, from: Option<EventCursor>) -> EventStream {
        let live = self.live.subscribe();
        let start = from.unwrap_or_else(|| self.last());
        let (tx, rx) = mpsc::channel(LIVE_BUFFER);
        tokio::spawn(follow(Arc::downgrade(self), live, start, tx));
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

type Sink = mpsc::Sender<std::result::Result<proto::EventEnvelope, ApiError>>;

/// Send recorded events after `seen`, then live ones, to `tx`.
async fn follow(
    log: Weak<EventLog>,
    mut live: broadcast::Receiver<proto::EventEnvelope>,
    mut seen: EventCursor,
    tx: Sink,
) {
    // Replay the backlog. Live events that arrive meanwhile wait in the
    // broadcast buffer and are skipped below if already replayed.
    if !replay(&log, &mut seen, &tx).await {
        return;
    }
    loop {
        tokio::select! {
            () = tx.closed() => return,
            received = live.recv() => match received {
                Ok(envelope) => {
                    if envelope.cursor <= seen {
                        continue;
                    }
                    if envelope.cursor > seen + 1 {
                        // A gap: something was missed; fill it from the log.
                        if !replay(&log, &mut seen, &tx).await {
                            return;
                        }
                        if envelope.cursor <= seen {
                            continue;
                        }
                    }
                    seen = envelope.cursor;
                    if tx.send(Ok(envelope)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !replay(&log, &mut seen, &tx).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Send every recorded event after `seen`. False when the subscriber or the
/// log is gone, or reading failed (the error is sent first).
async fn replay(log: &Weak<EventLog>, seen: &mut EventCursor, tx: &Sink) -> bool {
    loop {
        let Some(strong) = log.upgrade() else {
            return false;
        };
        let after = *seen;
        let batch = tokio::task::spawn_blocking(move || strong.read_after(after, REPLAY_BATCH))
            .await
            .map_err(|err| err.to_string())
            .and_then(|read| read.map_err(|err| err.to_string()));
        let batch = match batch {
            Ok(batch) => batch,
            Err(err) => {
                let _ = tx.send(Err(ApiError::Internal(err))).await;
                return false;
            }
        };
        let done = batch.len() < REPLAY_BATCH;
        for envelope in batch {
            *seen = envelope.cursor;
            if tx.send(Ok(envelope)).await.is_err() {
                return false;
            }
        }
        if done {
            return true;
        }
    }
}

impl Inner {
    /// The event log, opened on first use.
    pub(crate) fn event_log(&self) -> Result<Arc<EventLog>> {
        let mut slot = lock(&self.events);
        if let Some(log) = &*slot {
            return Ok(Arc::clone(log));
        }
        let log = Arc::new(EventLog::open(self.store.hord_dir())?);
        *slot = Some(Arc::clone(&log));
        Ok(log)
    }

    /// Record and broadcast `events`.
    pub(crate) fn emit(&self, events: Vec<proto::event::Kind>) -> Result<()> {
        if !events.is_empty() {
            self.event_log()?.append(events)?;
        }
        Ok(())
    }

    /// Make recorded events durable, if the log is open.
    pub(crate) fn sync_events(&self) -> Result<()> {
        let open = lock(&self.events).clone();
        match open {
            Some(log) => log.sync(),
            None => Ok(()),
        }
    }
}

/// `Submitted` for a new queue entry.
pub(crate) fn submitted(seq: u64, change: ChangeId, actor: &Actor) -> Kind {
    Kind::Submitted(proto::Submitted {
        submission: seq,
        change: wire::id(change),
        actor: Some(wire::actor(actor)),
    })
}

/// `ConflictCheck` for the report the lander recorded.
pub(crate) fn conflict_check(report: &ConflictReport) -> Kind {
    let result = if report.has_hard() {
        proto::ConflictOutcome::Hard
    } else if report.conflicts.is_empty() && report.merge.is_empty() {
        proto::ConflictOutcome::Clean
    } else {
        proto::ConflictOutcome::Overlap
    };
    Kind::ConflictCheck(proto::ConflictCheck {
        change: wire::id(report.change),
        result: result.into(),
        set_conflicts: count(report.conflicts.len()),
        merge_conflicts: count(report.merge.len()),
    })
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Events for an entry the lander settled without landing it now:
/// `Parked` for a conflicted one, `Rejected` for a rejected one, nothing
/// for one found already landed.
pub(crate) fn settled(entry: &QueueEntry) -> Vec<Kind> {
    let change = wire::id(entry.change);
    match &entry.status {
        QueueStatus::Conflicted => {
            let report = entry.report.as_ref();
            let (reason, detail) = match report.and_then(|r| r.verification.as_ref()) {
                Some(why) => (proto::ParkReason::VerificationFailed, why.clone()),
                None => {
                    let hard: Vec<String> = report
                        .map(|r| {
                            r.merge
                                .iter()
                                .filter(|m| m.severity == MergeSeverity::Hard)
                                .map(|m| format!("{}: {}", m.path, m.reason))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        proto::ParkReason::MergeConflict,
                        format!("{} hard merge conflict(s): {}", hard.len(), hard.join("; ")),
                    )
                }
            };
            vec![Kind::Parked(proto::Parked {
                change,
                reason: reason.into(),
                detail,
            })]
        }
        QueueStatus::Rejected { reason } => vec![Kind::Rejected(proto::Rejected {
            change,
            reason: reason.clone(),
        })],
        QueueStatus::Parked { reason } => {
            let review = entry.report.as_ref().is_some_and(|r| {
                !r.policy.is_empty() && r.policy.iter().all(|v| v.requirement.kind() == "review")
            });
            vec![Kind::Parked(proto::Parked {
                change,
                reason: if review {
                    proto::ParkReason::NeedsReview
                } else {
                    proto::ParkReason::Policy
                }
                .into(),
                detail: reason.clone(),
            })]
        }
        QueueStatus::Queued
        | QueueStatus::Landed { .. }
        | QueueStatus::Replaying { .. }
        | QueueStatus::NeedsArbitration
        | QueueStatus::Replayed { .. }
        | QueueStatus::Arbitrated { .. } => Vec::new(),
    }
}

/// `Replaying` for attempt `attempt` on `change`.
pub(crate) fn replaying(change: ChangeId, attempt: u32, harness: &str) -> Kind {
    Kind::Replaying(proto::Replaying {
        change: wire::id(change),
        attempt,
        harness: harness.into(),
    })
}

/// `Parked` for a change that entered the arbitration queue.
pub(crate) fn needs_arbitration(change: ChangeId, detail: String) -> Kind {
    Kind::Parked(proto::Parked {
        change: wire::id(change),
        reason: proto::ParkReason::NeedsArbitration.into(),
        detail,
    })
}

/// `Arbitrated`: `change` was resolved by `arbiter`, landing as `result`.
pub(crate) fn arbitrated(change: ChangeId, arbiter: &Arbiter, result: ChangeId) -> Kind {
    Kind::Arbitrated(proto::Arbitrated {
        change: wire::id(change),
        by: Some(wire::actor(&arbiter.actor)),
        result: wire::id(result),
        key_id: arbiter.key_id.clone(),
        signature: arbiter.signature.as_ref().map(|s| s.as_slice().to_vec()),
    })
}

/// `Landed` then `HeadMoved` for a landing at `position`.
pub(crate) fn landed(
    change: ChangeId,
    position: u64,
    submitted: Option<ChangeId>,
    evidence: &[ObjectId],
    previous: Option<ChangeId>,
) -> Vec<Kind> {
    vec![
        Kind::Landed(proto::Landed {
            change: wire::id(change),
            position,
            submitted: submitted.map(wire::id),
            evidence: evidence.iter().copied().map(wire::id).collect(),
        }),
        Kind::HeadMoved(proto::HeadMoved {
            from: previous.map(wire::id),
            to: wire::id(change),
        }),
    ]
}

/// `EvidenceAttached` for evidence stored against `change`'s result.
pub(crate) fn evidence_attached(change: ChangeId, id: ObjectId, evidence: &Evidence) -> Kind {
    Kind::EvidenceAttached(proto::EvidenceAttached {
        change: wire::id(change),
        evidence: wire::id(id),
        kind: Some(wire::evidence_kind(&evidence.kind)),
        result: Some(wire::evidence_result(&evidence.result)),
        qualifier: evidence.qualifier.clone(),
    })
}

#[cfg(test)]
mod tests {
    use tokio_stream::StreamExt;

    use super::*;

    fn rejected(n: u32) -> proto::event::Kind {
        proto::event::Kind::Rejected(proto::Rejected {
            change: String::new(),
            reason: n.to_string(),
        })
    }

    fn reason(envelope: &proto::EventEnvelope) -> Result<String, Box<dyn std::error::Error>> {
        match &envelope
            .event
            .as_ref()
            .ok_or("an appended envelope carries its event")?
            .kind
        {
            Some(proto::event::Kind::Rejected(r)) => Ok(r.reason.clone()),
            other => Err(format!("expected a Rejected event, got {other:?}").into()),
        }
    }

    fn temp(tag: &str) -> std::io::Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("hord-events-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[tokio::test]
    async fn cursors_persist_and_a_subscriber_resumes_after_its_cursor()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp("persist")?;
        {
            let log = EventLog::open(&dir)?;
            let first = log.append(vec![rejected(1), rejected(2)])?;
            assert_eq!(
                first.iter().map(|e| e.cursor).collect::<Vec<_>>(),
                vec![1, 2]
            );
        }
        let log = Arc::new(EventLog::open(&dir)?);
        assert_eq!(log.last(), 2, "the cursor survives a reopen");
        log.append(vec![rejected(3)])?;
        let mut stream = log.subscribe(Some(1));
        let mut replayed = Vec::new();
        for _ in 0..2 {
            replayed.push(stream.next().await.ok_or("replayed event")??);
        }
        assert_eq!(
            replayed.iter().map(|e| e.cursor).collect::<Vec<_>>(),
            vec![2, 3]
        );
        log.append(vec![rejected(4)])?;
        let live = stream.next().await.ok_or("live event")??;
        assert_eq!((live.cursor, reason(&live)?), (4, "4".to_owned()));
        drop(stream);
        drop(log);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn a_live_subscriber_sees_only_new_events_and_one_that_lags_catches_up()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp("live")?;
        let log = Arc::new(EventLog::open(&dir)?);
        log.append(vec![rejected(0)])?;
        let mut stream = log.subscribe(None);
        // More than the broadcast buffer, so the follower lags and re-reads.
        let total = LIVE_BUFFER as u32 * 2 + 5;
        for n in 1..=total {
            log.append(vec![rejected(n)])?;
        }
        for n in 1..=total {
            let e = stream.next().await.ok_or("lagging event")??;
            assert_eq!(e.cursor, u64::from(n) + 1);
            assert_eq!(reason(&e)?, n.to_string());
        }
        drop(stream);
        drop(log);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
