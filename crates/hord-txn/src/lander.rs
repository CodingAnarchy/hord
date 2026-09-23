//! The lander (spec §6.7): a persistent queue processed in order.
//!
//! For each queued change: validate the ops (spec §3.5) unless this process
//! proposed it, run the set check (§6.3), rebase structurally onto `head`
//! (§6.4 rung 1), call the [`Verifier`], then append to the log, move
//! `head`, and index the landed change. A hard merge conflict or a failed
//! verification parks the change as [`QueueStatus::Conflicted`] (it needs a
//! replay, M5). Set overlaps that rebase cleanly land with the report
//! attached, flagged for the verifier (§6.4).
//!
//! A change that lands on a head other than its base is stored again with
//! `base`, `result`, `parents`, and `ops` rewritten for that head; it lands
//! under that record's id. The queue entry keeps both ids.
//!
//! Crash recovery: the queue status is written before the durable
//! `set_head`. On the first run, an entry marked landed whose change is not
//! in the log is queued again.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use hord_core::{ChangeId, ChangeRecord, SnapshotId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::conflict::{ConflictReport, Footprint, check};
use crate::files::{validate, validate_except};
use crate::rebase::rebase;
use crate::repo::{Head, Inner, Repo, blocking, lock, now};
use crate::semantic::IdentityIndex;
use crate::{Error, Result};

/// Where a submitted change is in the lander (spec §6.2, §6.7).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum QueueStatus {
    /// Waiting for the lander.
    Queued,
    /// Appended to the log.
    Landed {
        /// Id it landed under: the submitted id, or its rebased record.
        landed: ChangeId,
    },
    /// Parked, not landed: a hard merge conflict or a failed verification.
    /// Needs a replay (spec §6.4 rung 2, M5). See the entry's report.
    Conflicted,
    /// Not a valid change: missing, or its ops do not reproduce its result.
    Rejected {
        /// Why.
        reason: String,
    },
}

/// One submission in the lander queue (`hord queue`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// Position in the queue; submission order.
    pub seq: u64,
    /// The submitted change.
    pub change: ChangeId,
    /// Current status.
    pub status: QueueStatus,
    /// When it was submitted.
    pub submitted_at: Timestamp,
    /// When the status last changed.
    pub updated_at: Timestamp,
    /// The lander's findings once processed; `None` while queued.
    pub report: Option<ConflictReport>,
}

/// Stored form of a [`QueueEntry`]; the sequence number is the table key.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredEntry {
    change: ChangeId,
    status: QueueStatus,
    submitted_at: Timestamp,
    updated_at: Timestamp,
    report: Option<ConflictReport>,
}

impl QueueEntry {
    fn from_stored(seq: u64, stored: StoredEntry) -> Self {
        Self {
            seq,
            change: stored.change,
            status: stored.status,
            submitted_at: stored.submitted_at,
            updated_at: stored.updated_at,
            report: stored.report,
        }
    }

    fn to_stored(&self) -> StoredEntry {
        StoredEntry {
            change: self.change,
            status: self.status.clone(),
            submitted_at: self.submitted_at,
            updated_at: self.updated_at,
            report: self.report.clone(),
        }
    }

    /// Whether `id` is this entry's submitted or landed change.
    #[must_use]
    pub fn names(&self, id: ChangeId) -> bool {
        self.change == id || matches!(self.status, QueueStatus::Landed { landed } if landed == id)
    }
}

/// Outcome of a [`Verifier`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Land it.
    Pass,
    /// Do not land it; a semantic conflict (spec §6.5).
    Fail {
        /// Why, for the conflict report.
        reason: String,
    },
}

/// What the lander asks a [`Verifier`] to check: the rebased change.
#[derive(Clone, Copy, Debug)]
pub struct VerifyRequest<'a> {
    /// Id the change will land under.
    pub change_id: ChangeId,
    /// The record as it will land (base = current head snapshot).
    pub change: &'a ChangeRecord,
    /// Set overlaps and soft merge conflicts to re-check.
    pub report: &'a ConflictReport,
}

/// Future returned by [`Verifier::verify`].
pub type VerifyFuture<'a> = Pin<Box<dyn Future<Output = Verdict> + Send + 'a>>;

/// Verification at landing (spec §6.5). M4 supplies the real engine.
///
/// The lander awaits this outside any lock on the store, so an
/// implementation may run checks concurrently or speculatively.
pub trait Verifier: Send + Sync {
    /// Check `request.change` against its result snapshot.
    fn verify<'a>(&'a self, request: VerifyRequest<'a>) -> VerifyFuture<'a>;
}

/// M3's verifier: always passes.
#[derive(Clone, Copy, Debug, Default)]
pub struct StubVerifier;

impl Verifier for StubVerifier {
    fn verify<'a>(&'a self, _request: VerifyRequest<'a>) -> VerifyFuture<'a> {
        Box::pin(async { Verdict::Pass })
    }
}

#[derive(Debug, Default)]
pub(crate) struct LanderState {
    /// Lowest sequence number that may still be queued; `None` before the
    /// first run's recovery.
    cursor: Option<u64>,
}

/// A change ready to verify and land.
struct Candidate {
    entry: QueueEntry,
    landed_id: ChangeId,
    landed: ChangeRecord,
    report: ConflictReport,
    index: IdentityIndex,
}

enum Step {
    Done(Box<QueueEntry>),
    Candidate(Box<Candidate>),
}

pub(crate) async fn run(repo: &Repo) -> Result<Vec<QueueEntry>> {
    let mut state = repo.inner.lander.lock().await;
    let mut processed = Vec::new();
    loop {
        let cursor = state.cursor;
        let next = blocking(&repo.inner, move |inner| inner.next_queued(cursor)).await?;
        let Some(entry) = next else {
            break;
        };
        state.cursor = Some(entry.seq);
        let step = blocking(&repo.inner, move |inner| inner.prepare(entry)).await?;
        let done = match step {
            Step::Done(entry) => *entry,
            Step::Candidate(candidate) => {
                let verdict = repo
                    .inner
                    .verifier
                    .verify(VerifyRequest {
                        change_id: candidate.landed_id,
                        change: &candidate.landed,
                        report: &candidate.report,
                    })
                    .await;
                blocking(&repo.inner, move |inner| inner.finish(*candidate, verdict)).await?
            }
        };
        processed.push(done);
    }
    Ok(processed)
}

impl Inner {
    pub(crate) fn submit(&self, change: ChangeId) -> Result<QueueEntry> {
        self.change_record(change)?;
        if let Some(entry) = self
            .queue_entries()?
            .into_iter()
            .rev()
            .find(|e| e.change == change && e.status == QueueStatus::Queued)
        {
            return Ok(entry);
        }
        let at = now();
        let stored = StoredEntry {
            change,
            status: QueueStatus::Queued,
            submitted_at: at,
            updated_at: at,
            report: None,
        };
        let seq = self.store.queue_push(&hord_encoding::encode(&stored)?)?;
        Ok(QueueEntry::from_stored(seq, stored))
    }

    pub(crate) fn queue_entries(&self) -> Result<Vec<QueueEntry>> {
        self.store
            .queue_entries()?
            .into_iter()
            .map(|(seq, bytes)| Ok(QueueEntry::from_stored(seq, hord_encoding::decode(&bytes)?)))
            .collect()
    }

    pub(crate) fn queue_status(&self, change: ChangeId) -> Result<QueueEntry> {
        self.queue_entries()?
            .into_iter()
            .rev()
            .find(|e| e.names(change))
            .ok_or(Error::NotQueued(change))
    }

    fn put_entry(&self, entry: &QueueEntry) -> Result<()> {
        self.store
            .queue_set(entry.seq, &hord_encoding::encode(&entry.to_stored())?)?;
        Ok(())
    }

    fn next_queued(&self, cursor: Option<u64>) -> Result<Option<QueueEntry>> {
        let start = match cursor {
            Some(seq) => seq,
            None => self.recover()?,
        };
        let mut seq = start;
        while let Some(bytes) = self.store.queue_entry(seq)? {
            let entry = QueueEntry::from_stored(seq, hord_encoding::decode(&bytes)?);
            if entry.status == QueueStatus::Queued {
                return Ok(Some(entry));
            }
            seq += 1;
        }
        Ok(None)
    }

    /// Re-queue entries marked landed whose change never reached the log.
    /// Returns the first queued sequence number, or the next one to be
    /// assigned.
    fn recover(&self) -> Result<u64> {
        let log: std::collections::HashSet<ChangeId> = self.store.log()?.into_iter().collect();
        let mut first = None;
        let mut next = 0;
        for mut entry in self.queue_entries()? {
            next = entry.seq + 1;
            if let QueueStatus::Landed { landed } = entry.status
                && !log.contains(&landed)
            {
                entry.status = QueueStatus::Queued;
                entry.report = None;
                self.put_entry(&entry)?;
            }
            if entry.status == QueueStatus::Queued && first.is_none() {
                first = Some(entry.seq);
            }
        }
        Ok(first.unwrap_or(next))
    }

    fn park(
        &self,
        mut entry: QueueEntry,
        status: QueueStatus,
        report: Option<ConflictReport>,
    ) -> Result<QueueEntry> {
        entry.status = status;
        entry.report = report;
        entry.updated_at = now();
        self.put_entry(&entry)?;
        Ok(entry)
    }

    fn prepare(&self, entry: QueueEntry) -> Result<Step> {
        let record = match self.change_record(entry.change) {
            Ok(record) => record,
            Err(Error::MissingChange(id)) => {
                let reason = format!("no change record {id}");
                return Ok(Step::Done(Box::new(self.park(
                    entry,
                    QueueStatus::Rejected { reason },
                    None,
                )?)));
            }
            Err(err) => return Err(err),
        };
        if !lock(&self.proposed).contains(&entry.change)
            && let Err(err) = validate(self, entry.change, &record)
        {
            let reason = err.to_string();
            return Ok(Step::Done(Box::new(self.park(
                entry,
                QueueStatus::Rejected { reason },
                None,
            )?)));
        }
        let head = self.head()?;
        let (mut report, landed_writes) = self.set_check(entry.change, &record, head)?;
        let rebased = match rebase(self, &record, head.snapshot, &landed_writes)? {
            Ok(rebased) => rebased,
            Err(merge) => {
                report.merge = merge;
                return Ok(Step::Done(Box::new(self.park(
                    entry,
                    QueueStatus::Conflicted,
                    Some(report),
                )?)));
            }
        };
        report.merge = rebased.soft;
        let checked_files = rebased.checked;
        let (landed_id, landed) = if rebased.result == record.result && record.base == head.snapshot
        {
            (entry.change, record)
        } else {
            let landed = ChangeRecord {
                base: head.snapshot,
                result: rebased.result,
                parents: head.change.into_iter().collect(),
                ops: rebased.ops,
                ..record
            };
            (self.store.put_object(&landed)?, landed)
        };
        // Spec §3.5: the record that lands must reproduce its result. A
        // rebased record has rewritten ops, so it is checked even when the
        // submitted one was checked at propose.
        let checked = landed_id == entry.change && lock(&self.proposed).contains(&entry.change);
        let v = if checked {
            Ok(())
        } else {
            validate_except(self, landed_id, &landed, &checked_files)
        };
        if let Err(err) = v {
            let reason = format!("rebased record does not reproduce its result: {err}");
            return Ok(Step::Done(Box::new(self.park(
                entry,
                QueueStatus::Rejected { reason },
                Some(report),
            )?)));
        }
        Ok(Step::Candidate(Box::new(Candidate {
            entry,
            landed_id,
            landed,
            report,
            index: rebased.index,
        })))
    }

    fn finish(&self, candidate: Candidate, verdict: Verdict) -> Result<QueueEntry> {
        let Candidate {
            mut entry,
            landed_id,
            landed,
            mut report,
            index,
        } = candidate;
        if let Verdict::Fail { reason } = verdict {
            report.verification = Some(reason);
            return self.park(entry, QueueStatus::Conflicted, Some(report));
        }
        self.put_identity_index(landed.result, index)?;
        entry.status = QueueStatus::Landed { landed: landed_id };
        entry.report = Some(report);
        entry.updated_at = now();
        self.put_entry(&entry)?;
        self.store.append_log(landed_id)?;
        self.store.set_head(landed_id)?;
        self.store.index_change(landed_id)?;
        let footprint = self.footprint_of(landed_id, &landed)?;
        lock(&self.footprints).insert(landed_id, Arc::new(footprint));
        self.set_head_cache(Head {
            change: Some(landed_id),
            snapshot: landed.result,
        });
        Ok(entry)
    }

    /// The §6.3 set check of `record` against everything landed after its
    /// base, as a report with no merge outcomes yet.
    fn set_report(
        &self,
        change: ChangeId,
        record: &ChangeRecord,
        head: Head,
    ) -> Result<ConflictReport> {
        Ok(self.set_check(change, record, head)?.0)
    }

    /// [`Self::set_report`] plus everything `L` wrote (write sets and coarse
    /// path ids), for the rebase's per-file fast path.
    fn set_check(
        &self,
        change: ChangeId,
        record: &ChangeRecord,
        head: Head,
    ) -> Result<(
        ConflictReport,
        std::collections::BTreeSet<hord_core::NodeId>,
    )> {
        let landed = self.landed_since(record)?;
        let own = self.footprint_of(change, record)?;
        let conflicts = check(&own, &landed, self.config.strict_reads);
        let written = landed
            .iter()
            .flat_map(|f| f.writes.iter().copied())
            .collect();
        let report = ConflictReport {
            change,
            base: record.base,
            head: head.change,
            checked_against: landed.iter().map(|f| f.change).collect(),
            strict_reads: self.config.strict_reads,
            conflicts,
            merge: Vec::new(),
            verification: None,
        };
        Ok((report, written))
    }

    pub(crate) fn conflicts(&self, change: ChangeId) -> Result<ConflictReport> {
        if let Ok(entry) = self.queue_status(change)
            && let Some(report) = entry.report
        {
            return Ok(report);
        }
        let record = self.change_record(change)?;
        let head = self.head()?;
        self.set_report(change, &record, head)
    }

    fn footprint(&self, change: ChangeId) -> Result<Arc<Footprint>> {
        if let Some(found) = lock(&self.footprints).get(&change) {
            return Ok(Arc::clone(found));
        }
        let record = self.change_record(change)?;
        let footprint = Arc::new(self.footprint_of(change, &record)?);
        lock(&self.footprints).insert(change, Arc::clone(&footprint));
        Ok(footprint)
    }

    fn footprint_of(&self, change: ChangeId, record: &ChangeRecord) -> Result<Footprint> {
        let touched: Vec<_> = self
            .changed_paths(record.base, record.result)?
            .into_iter()
            .map(|d| d.path)
            .collect();
        Ok(Footprint::of(change, record, &touched))
    }

    /// Changes landed after `record`'s base, in landing order (spec §6.3 `L`).
    ///
    /// The base is located by the record's first parent, else by the latest
    /// landed change whose result is the base snapshot. A base found nowhere
    /// in the log is checked against the whole log.
    fn landed_since(&self, record: &ChangeRecord) -> Result<Vec<Arc<Footprint>>> {
        let log = self.store.log()?;
        let start = match record
            .parents
            .first()
            .and_then(|parent| log.iter().rposition(|c| c == parent))
        {
            Some(i) => i + 1,
            None => self
                .position_of_snapshot(&log, record.base)?
                .map_or(0, |i| i + 1),
        };
        log[start..].iter().map(|c| self.footprint(*c)).collect()
    }

    fn position_of_snapshot(
        &self,
        log: &[ChangeId],
        snapshot: SnapshotId,
    ) -> Result<Option<usize>> {
        for (i, change) in log.iter().enumerate().rev() {
            if self.footprint(*change)?.result == snapshot {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// Latest landed change whose result is `snapshot`.
    pub(crate) fn change_for_snapshot(&self, snapshot: SnapshotId) -> Result<Option<ChangeId>> {
        let log = self.store.log()?;
        Ok(self.position_of_snapshot(&log, snapshot)?.map(|i| log[i]))
    }
}
