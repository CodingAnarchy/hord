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
//! A failure that is about the change (its record, a merge, identity, or
//! reproduction error) parks it as [`QueueStatus::Rejected`] with its
//! report; only transient store and I/O failures stop the run, so one bad
//! change never wedges the queue. A change whose effect head already has
//! appends nothing to the log, and submitting a landed change again returns
//! its entry.
//!
//! Crash recovery: the queue status is written before the durable
//! `set_head`. On the first run, an entry marked landed whose change is not
//! in the log is queued again. The in-process head follows `set_head`
//! before anything else can fail; a failed history-index update is retried
//! on the next run and does not undo the landing.

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
    /// Not landed and not replayable as is: the record is missing, its ops
    /// do not reproduce its result, processing it failed deterministically,
    /// or head already contains its result ("already applied").
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
    /// Landed changes whose history-index update failed; retried each run.
    unindexed: Vec<ChangeId>,
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

/// Outcome of [`Inner::try_prepare`].
enum Prepared {
    /// Not landing now; park the entry with this status.
    Park(QueueStatus),
    /// Rebased and validated; verify, then land.
    Ready(Box<Ready>),
}

/// A rebased, validated change: what [`Candidate`] needs besides the entry.
struct Ready {
    landed_id: ChangeId,
    landed: ChangeRecord,
    report: ConflictReport,
    index: IdentityIndex,
}

/// Whether `err` is about the store or the machine rather than about the
/// change: I/O, the redb index, pack files, or a failed task. Those
/// propagate, and the entry stays queued for the next run. Everything else
/// (a missing or undecodable object the record names, invalid ops, a
/// merge, identity, or reproduction failure) is deterministic for the
/// record and parks it.
pub(crate) fn is_transient(err: &Error) -> bool {
    match err {
        Error::Io(_) | Error::Task(_) => true,
        Error::Store(store) => matches!(
            store,
            hord_store::Error::Io(_)
                | hord_store::Error::Index(_)
                | hord_store::Error::CorruptIndex(_)
                | hord_store::Error::InvalidPack(_)
        ),
        _ => false,
    }
}

pub(crate) async fn run(repo: &Repo) -> Result<Vec<QueueEntry>> {
    let mut state = repo.inner.lander.lock().await;
    let mut processed = Vec::new();
    if !state.unindexed.is_empty() {
        let pending = std::mem::take(&mut state.unindexed);
        state.unindexed = blocking(&repo.inner, move |inner| {
            Ok(pending
                .into_iter()
                .filter(|c| inner.store.index_change(*c).is_err())
                .collect())
        })
        .await?;
    }
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
                let (entry, indexed) =
                    blocking(&repo.inner, move |inner| inner.finish(*candidate, verdict)).await?;
                if !indexed && let QueueStatus::Landed { landed } = entry.status {
                    state.unindexed.push(landed);
                }
                entry
            }
        };
        processed.push(done);
    }
    Ok(processed)
}

impl Inner {
    pub(crate) fn submit(&self, change: ChangeId) -> Result<QueueEntry> {
        self.change_record(change)?;
        // Queued already, or landed already: submitting again is a no-op.
        if let Some(entry) = self.queue_entries()?.into_iter().rev().find(|e| {
            e.names(change) && matches!(e.status, QueueStatus::Queued | QueueStatus::Landed { .. })
        }) {
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

    /// Check, rebase, and validate `entry`. A failure that is about the
    /// change (a bad record, a merge or identity error) parks it as
    /// [`QueueStatus::Rejected`]; only transient store or I/O failures
    /// propagate, so one bad change cannot wedge the queue ([`is_transient`]).
    fn prepare(&self, entry: QueueEntry) -> Result<Step> {
        let mut report = None;
        match self.try_prepare(&entry, &mut report) {
            Ok(Prepared::Park(status)) => {
                Ok(Step::Done(Box::new(self.park(entry, status, report)?)))
            }
            Ok(Prepared::Ready(ready)) => {
                let Ready {
                    landed_id,
                    landed,
                    report,
                    index,
                } = *ready;
                Ok(Step::Candidate(Box::new(Candidate {
                    entry,
                    landed_id,
                    landed,
                    report,
                    index,
                })))
            }
            Err(err) if is_transient(&err) => Err(err),
            Err(err) => {
                let reason = err.to_string();
                Ok(Step::Done(Box::new(self.park(
                    entry,
                    QueueStatus::Rejected { reason },
                    report,
                )?)))
            }
        }
    }

    /// [`Self::prepare`] before error classification. `report` is filled
    /// in as soon as the set check has run, so a later failure keeps it.
    fn try_prepare(
        &self,
        entry: &QueueEntry,
        report: &mut Option<ConflictReport>,
    ) -> Result<Prepared> {
        let record = match self.change_record(entry.change) {
            Ok(record) => record,
            Err(Error::MissingChange(id)) => {
                let reason = format!("no change record {id}");
                return Ok(Prepared::Park(QueueStatus::Rejected { reason }));
            }
            Err(err) => return Err(err),
        };
        if !lock(&self.proposed).contains(&entry.change) {
            validate(self, entry.change, &record)?;
        }
        let head = self.head()?;
        let (set_report, landed_writes) = self.set_check(entry.change, &record, head)?;
        let report = report.insert(set_report);
        let rebased = match rebase(self, &record, head.snapshot, &landed_writes)? {
            Ok(rebased) => rebased,
            Err(merge) => {
                report.merge = merge;
                return Ok(Prepared::Park(QueueStatus::Conflicted));
            }
        };
        report.merge = rebased.soft;
        report.adapter_merged = rebased.adapter_merged;
        if rebased.result == head.snapshot {
            // Head already has everything this change does: nothing to append.
            let reason = match self.landed_as(entry.change)? {
                Some(landed) => return Ok(Prepared::Park(QueueStatus::Landed { landed })),
                None => format!(
                    "already applied: head {} already contains this change's result",
                    head.change.map_or_else(|| "(empty)".into(), |c| c.to_hex())
                ),
            };
            return Ok(Prepared::Park(QueueStatus::Rejected { reason }));
        }
        let checked_files = rebased.checked;
        // The rebased result's NodeIds are the rebase's. Validation reads
        // that snapshot, so stage them in this process; `finish` stores them
        // only if the change lands.
        if rebased.result != record.result {
            self.stage_identity_index(rebased.result, rebased.index.clone());
        }
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
        if !checked && let Err(err) = validate_except(self, landed_id, &landed, &checked_files) {
            if is_transient(&err) {
                return Err(err);
            }
            let reason = format!("rebased record does not reproduce its result: {err}");
            return Ok(Prepared::Park(QueueStatus::Rejected { reason }));
        }
        Ok(Prepared::Ready(Box::new(Ready {
            landed_id,
            landed,
            report: report.clone(),
            index: rebased.index,
        })))
    }

    /// The id `change` landed under, if it is in the log (as submitted, or
    /// as its rebased record per a landed queue entry).
    fn landed_as(&self, change: ChangeId) -> Result<Option<ChangeId>> {
        let log: std::collections::HashSet<ChangeId> = self.store.log()?.into_iter().collect();
        if log.contains(&change) {
            return Ok(Some(change));
        }
        Ok(self
            .queue_entries()?
            .into_iter()
            .find_map(|e| match e.status {
                QueueStatus::Landed { landed } if e.change == change && log.contains(&landed) => {
                    Some(landed)
                }
                _ => None,
            }))
    }

    /// Land `candidate` unless the verifier failed it. The second value is
    /// `false` when the landed change could not be added to the history
    /// index; the landing stands and the caller retries the index later.
    fn finish(&self, candidate: Candidate, verdict: Verdict) -> Result<(QueueEntry, bool)> {
        let Candidate {
            mut entry,
            landed_id,
            landed,
            mut report,
            index,
        } = candidate;
        if let Verdict::Fail { reason } = verdict {
            report.verification = Some(reason);
            return Ok((
                self.park(entry, QueueStatus::Conflicted, Some(report))?,
                true,
            ));
        }
        self.put_identity_index(landed.result, index)?;
        entry.status = QueueStatus::Landed { landed: landed_id };
        entry.report = Some(report);
        entry.updated_at = now();
        self.put_entry(&entry)?;
        self.store.append_log(landed_id)?;
        self.store.set_head(landed_id)?;
        // Head is durable: the cache must follow before anything else can
        // fail, or the next landing would rebase onto the old head.
        self.set_head_cache(Head {
            change: Some(landed_id),
            snapshot: landed.result,
        });
        // The footprint is a cache (`footprint` recomputes it on a miss),
        // and the history index is derived and rebuildable: neither undoes
        // the landing.
        if let Ok(footprint) = self.footprint_of(landed_id, &landed) {
            lock(&self.footprints).insert(landed_id, Arc::new(footprint));
        }
        let indexed = self.store.index_change(landed_id).is_ok();
        Ok((entry, indexed))
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
            adapter_merged: Vec::new(),
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

#[cfg(test)]
mod tests {
    use hord_core::{ObjectId, RepoPath};

    use super::*;

    #[test]
    fn only_store_and_io_failures_are_transient() {
        let io = || std::io::Error::other("disk");
        let transient = [
            Error::Io(io()),
            Error::Task("cancelled".into()),
            Error::Store(hord_store::Error::Io(io())),
            Error::Store(hord_store::Error::Index("redb".into())),
        ];
        for err in &transient {
            assert!(is_transient(err), "{err}");
        }
        let id = ObjectId::from_bytes([7; 32]);
        let path: RepoPath = "src/lib.rs".parse().unwrap();
        let deterministic = [
            Error::MissingChange(id),
            Error::Store(hord_store::Error::MissingObject(id)),
            Error::Corrupt {
                id,
                reason: "wrong snapshot".into(),
            },
            Error::OpsDoNotReproduce {
                path: path.clone(),
                reason: "stale replace".into(),
            },
            Error::NotParsed(path),
        ];
        for err in &deterministic {
            assert!(!is_transient(err), "{err}");
        }
    }
}
