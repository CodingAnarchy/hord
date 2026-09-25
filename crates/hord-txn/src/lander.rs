//! The lander (spec §6.7): a persistent queue processed in order.
//!
//! For each queued change: validate the ops (spec §3.5) unless this process
//! proposed it, run the set check (§6.3), rebase structurally onto `head`
//! (§6.4 rung 1), verify and apply head's policy ([`crate::gate`], §6.5,
//! §7), then append to the log, move `head`, and index the landed change.
//! A hard merge conflict or a failed verification parks the change as
//! [`QueueStatus::Conflicted`] (it needs a replay, M5); a policy denial
//! parks it as [`QueueStatus::Parked`] (missing evidence, such as a review)
//! or rejects it (required evidence failed on its snapshot). Set overlaps
//! that rebase cleanly land with the report attached, flagged for the
//! verifier (§6.4).
//!
//! Verification is speculative (spec §6.7, ADR 0025): up to
//! [`SPECULATIVE_WINDOW`] changes are prepared ahead, each stacked on the
//! ones before it that are expected to land, and verified concurrently; the
//! front lands first, and a front whose outcome differs from what was
//! expected sends the rest back to be prepared again (see [`run`]).
//!
//! A change that lands on a head other than its base lands as a new record
//! (ADR 0018): `base`, `result`, `parents`, `ops`, `write_set`, and
//! `identity_deltas` are recomputed for that head (the sets exactly as
//! `propose` computes them), `read_set`, `intent`, `provenance`, and
//! `evidence` are kept, `signature` is cleared, `rebased_from` names the
//! submitted record, and the lander's `Rebase { submitted }` attestation is
//! appended to `evidence`. The rebased record and its attestation are
//! stored only when it lands. The
//! queue entry keeps both ids, and the store's `rebased` index maps the
//! submitted id to the landed one.
//!
//! A failure that is about the change (its record, a merge, identity, or
//! reproduction error) parks it as [`QueueStatus::Rejected`] with its
//! report; only transient store and I/O failures stop the run, so one bad
//! change never wedges the queue. A change whose effect head already has
//! appends nothing to the log, and submitting a landed change again returns
//! its entry.
//!
//! A landing is one durable commit ([`hord_store::Store::land`]): the queue
//! entry, the log append, `head`, and the history index move together or not
//! at all. The result snapshot, its identity tree, and the record are
//! content-addressed objects stored before it. On the first run, an entry
//! marked landed whose change is not in the log (a store written before
//! landings were atomic, or forged) is queued again. The in-process head
//! follows the commit before anything else can fail.
//!
//! Validation depends only on the record, so its outcome is stored by
//! change id ([`hord_store::Store::mark_checked`]): `submit` records the
//! changes this process proposed, and the lander checks queued changes from
//! elsewhere a few entries ahead of itself, in parallel, off its own path.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use hord_api::EventStream;
use hord_core::{
    Actor, Bytes, ChangeId, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, IdentityDelta,
    NodeId, ObjectId, SnapshotId, Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::conflict::{ConflictReport, Footprint, check};
use crate::escalation::{Escalation, Origin, TAMPERED};
use crate::events;
use crate::files::{validate, validate_except};
use crate::gate::{CandidateContext, HeadPolicy, Verdict, VerifyContext, VerifyRequest};
use crate::propose::declared;
use crate::rebase::rebase;
use crate::repo::{Head, Inner, Repo, blocking, lock, now};
use crate::sets::sets_between;
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
    /// head already contains its result ("already applied"), or head's
    /// policy denies it on evidence that failed for its snapshot, which
    /// cannot change (ADR 0025).
    Rejected {
        /// Why.
        reason: String,
    },
    /// Not landed yet: head's policy (ADR 0026) requires evidence the
    /// snapshot does not have, such as a review. The report's `policy`
    /// lists the violations. Attach the evidence and submit again; nothing
    /// that already ran is re-run.
    Parked {
        /// Why, in words.
        reason: String,
    },
    /// Conflicted, and on rung 2 of the escalation ladder (spec §6.4): the
    /// replay harness is running this attempt, or the replay it proposed is
    /// in the queue. See the entry's [`Escalation`].
    Replaying {
        /// Attempt number, from 1.
        attempt: u32,
    },
    /// Replays ran out: parked in the arbitration queue (spec §6.4 rung 3)
    /// with a conflict summary and the replay candidates.
    NeedsArbitration,
    /// Resolved by a replay: a change with `parent_intent` naming this one
    /// landed.
    Replayed {
        /// The id the replay landed under.
        landed: ChangeId,
    },
    /// Resolved by an arbiter: a change whose parents include this one
    /// landed.
    Arbitrated {
        /// The id the resolution landed under.
        landed: ChangeId,
    },
}

impl QueueStatus {
    /// Whether an arbiter may resolve an entry in this status: a conflict
    /// with no harness to replay it, or one whose replays ran out.
    #[must_use]
    pub fn is_arbitrable(&self) -> bool {
        matches!(self, Self::Conflicted | Self::NeedsArbitration)
    }
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
    /// Why this change was submitted, when it is part of another change's
    /// escalation: a replay or an arbiter's resolution.
    pub origin: Option<Origin>,
    /// Its way up the escalation ladder (spec §6.4), once it entered it.
    pub escalation: Option<Escalation>,
}

/// Stored form of a [`QueueEntry`]; the sequence number is the table key.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredEntry {
    change: ChangeId,
    status: QueueStatus,
    submitted_at: Timestamp,
    updated_at: Timestamp,
    report: Option<ConflictReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    escalation: Option<Escalation>,
}

/// Ids that name `entry` ([`QueueEntry::names`]), for the store's index.
fn entry_names(entry: &QueueEntry) -> Vec<ChangeId> {
    let mut names = vec![entry.change];
    if let QueueStatus::Landed { landed } = entry.status
        && landed != entry.change
    {
        names.push(landed);
    }
    names
}

impl QueueEntry {
    /// The entry at `seq` from its stored bytes.
    fn decode(seq: u64, bytes: &[u8]) -> Result<Self> {
        Ok(Self::from_stored(seq, hord_encoding::decode(bytes)?))
    }

    fn from_stored(seq: u64, stored: StoredEntry) -> Self {
        Self {
            seq,
            change: stored.change,
            status: stored.status,
            submitted_at: stored.submitted_at,
            updated_at: stored.updated_at,
            report: stored.report,
            origin: stored.origin,
            escalation: stored.escalation,
        }
    }

    fn to_stored(&self) -> StoredEntry {
        StoredEntry {
            change: self.change,
            status: self.status.clone(),
            submitted_at: self.submitted_at,
            updated_at: self.updated_at,
            report: self.report.clone(),
            origin: self.origin,
            escalation: self.escalation.clone(),
        }
    }

    /// Whether `id` is this entry's submitted or landed change.
    #[must_use]
    pub fn names(&self, id: ChangeId) -> bool {
        self.change == id || matches!(self.status, QueueStatus::Landed { landed } if landed == id)
    }
}

/// The lander as a long-running task (spec §6.7, ADR 0024).
///
/// [`Lander::spawn`] starts a tokio task that drains the queue, then sleeps
/// until [`Repo::submit`] wakes it or the cancellation token fires. It is
/// the only writer of the log and head: the repository's lander lock
/// serializes it with [`Repo::land_local`], which runs the same drain
/// inline ([`Lander::drain`]). A transient store or I/O failure is retried
/// after a pause; the entry stays queued.
#[derive(Debug)]
pub struct Lander;

/// Pause before retrying after a transient failure.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

impl Lander {
    /// Start the lander for `repo` on the current tokio runtime. Returns its
    /// task, which ends when `cancel` fires, and a live [`EventStream`] of
    /// the events it emits from now on (spec §10.5.3). Must be called from
    /// within a runtime.
    pub fn spawn(
        repo: Repo,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(tokio::task::JoinHandle<()>, EventStream)> {
        let log = repo.inner.event_log()?;
        let stream = log.subscribe(None);
        let task = tokio::spawn(async move {
            loop {
                let pause = match Self::drain(&repo).await {
                    Ok(_) => None,
                    Err(_) => Some(RETRY_AFTER),
                };
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = repo.inner.wake.notified(), if pause.is_none() => {}
                    () = tokio::time::sleep(pause.unwrap_or_default()), if pause.is_some() => {}
                }
            }
        });
        Ok((task, stream))
    }

    /// Process queued changes until none is left (one lander pass).
    /// Returns the entries processed, in order.
    pub async fn drain(repo: &Repo) -> Result<Vec<QueueEntry>> {
        run(repo).await
    }
}

#[derive(Debug, Default)]
pub(crate) struct LanderState {
    /// Lowest sequence number that may still be queued; `None` before the
    /// first run's recovery.
    cursor: Option<u64>,
    /// Validation of queued changes ahead of the cursor, on blocking
    /// threads. The lander awaits a change's task before preparing it.
    checking: HashMap<ChangeId, tokio::task::JoinHandle<()>>,
}

/// How many queued changes ahead of the cursor the lander validates at once.
fn check_ahead() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 16))
}

/// Candidates the lander prepares and verifies ahead of landing, each
/// stacked on the ones before it (ADR 0025, spec §6.7: K = 4).
pub const SPECULATIVE_WINDOW: usize = 4;

/// A change ready to verify and land.
struct Candidate {
    entry: QueueEntry,
    landed_id: ChangeId,
    landed: Arc<ChangeRecord>,
    report: ConflictReport,
    /// The rebase attestation, stored with a rebased record.
    attestation: Option<Evidence>,
    /// Head's policy at the landing base (ADR 0026).
    policy: HeadPolicy,
    /// Policy facts without evidence; `None` when the policy requires
    /// nothing.
    facts: Option<hord_policy::Facts>,
    /// What verification must produce.
    verify: hord_verify::VerifyPolicy,
    /// Where the candidate lands (for the next candidate to stack on).
    footprint: Arc<Footprint>,
    /// Whether it is expected to land ([`Inner::predict`]): later
    /// candidates stack only on those that are.
    predicted: bool,
}

impl Candidate {
    fn head(&self) -> Head {
        Head {
            change: Some(self.landed_id),
            snapshot: self.landed.result,
        }
    }
}

enum Step {
    /// Settled without verification: its status and report, not yet
    /// written.
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
    attestation: Option<Evidence>,
    policy: HeadPolicy,
}

/// One place in the speculative window.
enum Slot {
    /// Settled at prepare; written when it reaches the front.
    Done(Box<QueueEntry>),
    /// Being verified.
    Verifying {
        candidate: Box<Candidate>,
        verdict: tokio::task::JoinHandle<Verdict>,
    },
}

impl Slot {
    fn seq(&self) -> u64 {
        match self {
            Self::Done(entry) => entry.seq,
            Self::Verifying { candidate, .. } => candidate.entry.seq,
        }
    }
}

/// Whether `err` is about the store or the machine rather than about the
/// change: I/O, the redb index, or a failed task. Those
/// propagate, and the entry stays queued for the next run. Everything else
/// (a missing or undecodable object the record names, invalid ops, a
/// merge, identity, or reproduction failure) is deterministic for the
/// record and parks it.
pub(crate) fn is_transient(err: &Error) -> bool {
    match err {
        Error::Io(_) | Error::Task(_) | Error::EventLog(_) => true,
        Error::Store(store) => matches!(
            store,
            hord_store::Error::Io(_)
                | hord_store::Error::Index(_)
                | hord_store::Error::CorruptIndex(_)
        ),
        _ => false,
    }
}

/// Drain the queue with a speculative window (ADR 0025).
///
/// Up to [`SPECULATIVE_WINDOW`] queued changes are prepared ahead, each
/// rebased onto the one before it as if it had landed, and verified
/// concurrently. The front of the window lands (or parks) first. When a
/// candidate does not land, everything behind it was prepared on a head
/// that will not exist: its verification is abandoned (evidence it already
/// indexed stays, a true fact about that snapshot) and it is prepared
/// again. Outcomes decided at prepare (parks, rejections) are written only
/// when they reach the front, for the same reason.
pub(crate) async fn run(repo: &Repo) -> Result<Vec<QueueEntry>> {
    let mut state = repo.inner.lander.lock().await;
    if state.cursor.is_none() {
        // First run in this process: replays a stopped lander left behind.
        for job in blocking(&repo.inner, Inner::resume_ladders).await? {
            crate::replay::spawn_replay(repo, job);
        }
    }
    let mut processed = Vec::new();
    let ahead = check_ahead();
    let mut window: VecDeque<Slot> = VecDeque::new();
    loop {
        // Closing the repository: stop here, recording nothing more.
        if repo.inner.closing.is_cancelled() {
            abandon(&mut window, &mut state);
            break;
        }
        // Fill the window.
        while window.len() < SPECULATIVE_WINDOW {
            let from = match window.back() {
                Some(slot) => Some(slot.seq() + 1),
                None => state.cursor,
            };
            let next = blocking(&repo.inner, move |inner| inner.next_queued(from)).await?;
            let Some(entry) = next else {
                break;
            };
            if window.is_empty() {
                state.cursor = Some(entry.seq);
            }
            let seq = entry.seq;
            let unchecked =
                blocking(&repo.inner, move |inner| inner.unchecked_after(seq, ahead)).await?;
            for change in unchecked {
                if state.checking.len() >= ahead {
                    break;
                }
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    state.checking.entry(change)
                {
                    let inner = Arc::clone(&repo.inner);
                    slot.insert(repo.inner.tasks.spawn_blocking(move || {
                        // A failure is found again, and classified, in `prepare`.
                        let _ = inner.check_ops(change);
                    }));
                }
            }
            if let Some(task) = state.checking.remove(&entry.change) {
                let _ = task.await;
            }
            // Stack on the candidates ahead in the window.
            let stacked: Vec<(Head, Arc<Footprint>)> = window
                .iter()
                .filter_map(|slot| match slot {
                    Slot::Verifying { candidate, .. } if candidate.predicted => {
                        Some((candidate.head(), Arc::clone(&candidate.footprint)))
                    }
                    Slot::Verifying { .. } => None,
                    Slot::Done(_) => None,
                })
                .collect();
            let step = blocking(&repo.inner, move |inner| inner.prepare(entry, &stacked)).await?;
            window.push_back(match step {
                Step::Done(entry) => Slot::Done(entry),
                Step::Candidate(candidate) => {
                    let pending: Vec<_> = window
                        .iter()
                        .filter_map(|slot| match slot {
                            Slot::Verifying { candidate, .. } if candidate.predicted => {
                                Some((candidate.landed.result, candidate.landed.write_set.clone()))
                            }
                            _ => None,
                        })
                        .collect();
                    let verdict = spawn_verify(repo, &candidate, pending);
                    Slot::Verifying { candidate, verdict }
                }
            });
        }
        let Some(front) = window.pop_front() else {
            if !repo.inner.replays.any() {
                break;
            }
            // Replays are running (spec §6.4 rung 2): what they propose is
            // submitted, which wakes this loop, as does a replay finishing.
            tokio::select! {
                () = repo.inner.wake.notified() => {}
                () = repo.inner.closing.cancelled() => {}
            }
            continue;
        };
        state.cursor = Some(front.seq());
        let (done, mispredicted) = match front {
            Slot::Done(entry) => {
                let entry = blocking(&repo.inner, move |inner| inner.settle(*entry)).await?;
                (entry, false)
            }
            Slot::Verifying {
                candidate,
                mut verdict,
            } => {
                let predicted = candidate.predicted;
                // Shutdown must not wait out a verification run: cancel it
                // (its commands are killed) and record nothing. The change
                // stays queued, and the next start verifies it again.
                let verdict = tokio::select! {
                    verdict = &mut verdict => verdict,
                    () = repo.inner.closing.cancelled() => {
                        verdict.abort();
                        let seq = candidate.entry.seq;
                        abandon(&mut window, &mut state);
                        state.cursor = Some(seq);
                        break;
                    }
                };
                let verdict = verdict.unwrap_or_else(|err| Verdict::Fail {
                    evidence: Vec::new(),
                    reason: format!("verification task failed: {err}"),
                });
                let entry =
                    blocking(&repo.inner, move |inner| inner.finish(*candidate, verdict)).await?;
                let landed = matches!(entry.status, QueueStatus::Landed { .. });
                (entry, landed != predicted)
            }
        };
        if mispredicted {
            // Everything behind was stacked on a head that does not exist
            // (a landing that did not happen, or one that did but was not
            // expected): abandon it and prepare it again.
            for slot in window.drain(..) {
                if let Slot::Verifying { verdict, .. } = slot {
                    verdict.abort();
                }
            }
        }
        // Rung 2 and 3 (spec §6.4): start the replays of a change that
        // entered the ladder when it settled, and move the escalation of
        // the change this one replays or resolves.
        let (done, mut jobs) = blocking(&repo.inner, move |inner| inner.escalate(done)).await?;
        jobs.extend(std::mem::take(&mut *crate::repo::lock(
            &repo.inner.pending_replays,
        )));
        for job in jobs {
            crate::replay::spawn_replay(repo, job);
        }
        processed.push(done);
    }
    // Idle: make the events of this run durable.
    if !processed.is_empty() {
        blocking(&repo.inner, |inner| inner.sync_events()).await?;
    }
    Ok(processed)
}

/// Drop the window without recording anything: abort its verification
/// and point the cursor at its first entry, which is still queued.
fn abandon(window: &mut VecDeque<Slot>, state: &mut LanderState) {
    if let Some(front) = window.front() {
        state.cursor = Some(front.seq());
    }
    for slot in window.drain(..) {
        if let Slot::Verifying { verdict, .. } = slot {
            verdict.abort();
        }
    }
}

/// Start verifying `candidate` on the runtime.
///
/// `pending` are the candidates ahead in the window it is stacked on.
fn spawn_verify(
    repo: &Repo,
    candidate: &Candidate,
    pending: Vec<(SnapshotId, BTreeSet<NodeId>)>,
) -> tokio::task::JoinHandle<Verdict> {
    let context: Arc<dyn VerifyContext> = Arc::new(CandidateContext::new(
        Arc::clone(&repo.inner),
        candidate.landed_id,
        Arc::clone(&candidate.landed),
        pending,
        true,
    ));
    let request = VerifyRequest {
        change_id: candidate.landed_id,
        change: Arc::clone(&candidate.landed),
        report: candidate.report.clone(),
        policy: candidate.verify.clone(),
        context,
        plan_only: false,
    };
    let verifier = Arc::clone(&repo.inner.verifier);
    repo.inner
        .tasks
        .spawn(async move { verifier.verify(request).await })
}

impl Inner {
    pub(crate) fn submit(&self, change: ChangeId) -> Result<QueueEntry> {
        self.submit_as(change, None)
    }

    /// [`Self::submit`], naming why: an arbiter's resolution passes its
    /// [`Origin`]. A record whose `provenance.parent_intent` is set is a
    /// replay of that change ([`Origin::Replay`]) however it was submitted.
    pub(crate) fn submit_as(&self, change: ChangeId, origin: Option<Origin>) -> Result<QueueEntry> {
        let record = self.change_record(change)?;
        let origin = origin.or_else(|| {
            record
                .provenance
                .parent_intent
                .map(|of| Origin::Replay { of })
        });
        // Queued already, or landed already: submitting again is a no-op.
        if let Some(entry) = self
            .named_entries(change)?
            .into_iter()
            .rev()
            .find(|e| matches!(e.status, QueueStatus::Queued | QueueStatus::Landed { .. }))
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
            origin,
            escalation: None,
        };
        // `propose` checked the ops; record that for a lander in any process.
        let checked: &[ChangeId] = if self.was_proposed(change) {
            &[change]
        } else {
            &[]
        };
        let seq = self
            .store
            .queue_push(&hord_encoding::encode(&stored)?, &[change], checked)?;
        // Emit before waking the lander, so `submitted` precedes its events.
        self.emit(vec![events::submitted(
            seq,
            change,
            &record.provenance.actor,
        )])?;
        self.wake.notify_one();
        Ok(QueueEntry::from_stored(seq, stored))
    }

    pub(crate) fn queue_entries(&self) -> Result<Vec<QueueEntry>> {
        self.store
            .queue_entries()?
            .into_iter()
            .map(|(seq, bytes)| QueueEntry::decode(seq, &bytes))
            .collect()
    }

    pub(crate) fn queue_status(&self, change: ChangeId) -> Result<QueueEntry> {
        self.named_entries(change)?
            .pop()
            .ok_or(Error::NotQueued(change))
    }

    /// Queue entries that name `id` ([`QueueEntry::names`]), in sequence
    /// order: point lookups through the store's name index.
    pub(crate) fn named_entries(&self, id: ChangeId) -> Result<Vec<QueueEntry>> {
        if !self.store.queue_names_indexed()? {
            // A queue from before the name index: index it once.
            let rows: Vec<_> = self
                .queue_entries()?
                .iter()
                .map(|e| (e.seq, entry_names(e)))
                .collect();
            self.store.index_queue_names(&rows)?;
        }
        let mut out = Vec::new();
        for seq in self.store.queue_named(id)? {
            if let Some(bytes) = self.store.queue_entry(seq)? {
                let entry = QueueEntry::decode(seq, &bytes)?;
                if entry.names(id) {
                    out.push(entry);
                }
            }
        }
        Ok(out)
    }

    /// Whether `change`'s ops are known to reproduce its result: this
    /// process proposed it, or a check was recorded.
    fn ops_checked(&self, change: ChangeId) -> Result<bool> {
        Ok(self.was_proposed(change) || self.store.is_checked(change)?)
    }

    /// Validate `change` and record the outcome if it passes.
    fn check_ops(&self, change: ChangeId) -> Result<()> {
        if self.ops_checked(change)? {
            return Ok(());
        }
        let record = self.change_record(change)?;
        validate(self, change, &record)?;
        self.store.mark_checked(change)?;
        Ok(())
    }

    /// Up to `limit` queued changes after `seq` whose ops are not known to
    /// be checked, in queue order.
    fn unchecked_after(&self, seq: u64, limit: usize) -> Result<Vec<ChangeId>> {
        let mut out = Vec::new();
        let mut next = seq + 1;
        // Look a bounded distance ahead: landed and parked entries are
        // skipped, not counted, but the walk must stay cheap per landing.
        let end = next + 4 * limit as u64;
        while out.len() < limit && next < end {
            let Some(bytes) = self.store.queue_entry(next)? else {
                break;
            };
            let entry = QueueEntry::decode(next, &bytes)?;
            if entry.status == QueueStatus::Queued && !self.ops_checked(entry.change)? {
                out.push(entry.change);
            }
            next += 1;
        }
        Ok(out)
    }

    pub(crate) fn put_entry(&self, entry: &QueueEntry) -> Result<()> {
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
            let entry = QueueEntry::decode(seq, &bytes)?;
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
        let mut first = None;
        let mut next = 0;
        for mut entry in self.queue_entries()? {
            next = entry.seq + 1;
            if let QueueStatus::Landed { landed } = entry.status
                && !self.store.log_contains(landed)?
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

    /// Check, rebase, and validate `entry` on head, or on the last of
    /// `stacked` (candidates ahead in the speculative window, with their
    /// footprints, as if they had landed). A failure that is about the
    /// change (a bad record, a merge or identity error) settles it as
    /// [`QueueStatus::Rejected`]; only transient store or I/O failures
    /// propagate, so one bad change cannot wedge the queue
    /// ([`is_transient`]). A settled entry is not written yet
    /// ([`Self::settle`]).
    fn prepare(&self, entry: QueueEntry, stacked: &[(Head, Arc<Footprint>)]) -> Result<Step> {
        let mut report = None;
        let base = match stacked.last() {
            Some((head, _)) => *head,
            None => self.head()?,
        };
        let staged: Vec<Arc<Footprint>> = stacked.iter().map(|(_, f)| Arc::clone(f)).collect();
        let prepared = self.try_prepare(&entry, base, &staged, &mut report);
        if let Some(report) = &report
            && !matches!(&prepared, Err(err) if is_transient(err))
        {
            self.emit(vec![events::conflict_check(report)])?;
        }
        let settled = |mut entry: QueueEntry, status, report| {
            entry.status = status;
            entry.report = report;
            Ok(Step::Done(Box::new(entry)))
        };
        match prepared {
            Ok(Prepared::Park(status)) => settled(entry, status, report),
            Ok(Prepared::Ready(ready)) => {
                let Ready {
                    landed_id,
                    landed,
                    report,
                    attestation,
                    policy,
                } = *ready;
                // What verification must produce: the requirements head's
                // policy applies to this change (ADR 0026).
                let (facts, require, max_impact) = match &policy {
                    Ok((policy, _)) => {
                        let (facts, require) = self.requirements(policy, &landed)?;
                        (facts, require, policy.land().max_impact)
                    }
                    Err(_) => (None, Default::default(), None),
                };
                let footprint = Arc::new(self.footprint_of(landed_id, &landed)?);
                let predicted = match (&policy, &facts) {
                    (Ok((policy, _)), Some(facts)) => {
                        self.predict(policy, facts, &require, &landed, &report)?
                    }
                    (Ok(_), None) => true,
                    (Err(_), _) => false,
                };
                Ok(Step::Candidate(Box::new(Candidate {
                    entry,
                    landed_id,
                    landed: Arc::new(landed),
                    report,
                    attestation,
                    policy,
                    facts,
                    verify: hord_verify::VerifyPolicy {
                        require,
                        max_impact,
                        ..hord_verify::VerifyPolicy::default()
                    },
                    footprint,
                    predicted,
                })))
            }
            Err(err) if is_transient(&err) => Err(err),
            Err(err) => {
                let reason = err.to_string();
                settled(entry, QueueStatus::Rejected { reason }, report)
            }
        }
    }

    /// Whether head's policy is expected to let `landed` through: it would
    /// with the evidence indexed for its snapshot now plus a pass for
    /// every requirement in `require` (what the policy applies to `facts`)
    /// a verifier can produce. Review evidence is signed
    /// by a reviewer, not produced by verification, so it must be there
    /// already. A guess for stacking only (ADR 0025): a wrong one costs a
    /// re-preparation, never a wrong landing.
    fn predict(
        &self,
        policy: &hord_policy::CompiledPolicy,
        facts: &hord_policy::Facts,
        require: &BTreeSet<String>,
        landed: &ChangeRecord,
        report: &ConflictReport,
    ) -> Result<bool> {
        let mut evidence = self.candidate_evidence_facts(landed, report)?;
        for requirement in require {
            let Ok(tag) = requirement.parse::<hord_policy::EvidenceTag>() else {
                continue;
            };
            if tag.kind() != "review" {
                evidence.push(hord_policy::EvidenceFact {
                    tag,
                    result: hord_core::EvidenceResult::Pass,
                });
            }
        }
        let assumed = hord_policy::Facts {
            evidence,
            ..facts.clone()
        };
        Ok(policy.evaluate(&assumed).is_allow())
    }

    /// Write a settled entry, and announce it. A conflicted change that
    /// goes on to rung 2 is written once, already replaying (or parked for
    /// arbitration when no replay is allowed): it is never visible, or
    /// announced, as conflicted in between ([`Inner::enter_ladder`]).
    fn settle(&self, mut entry: QueueEntry) -> Result<QueueEntry> {
        entry.updated_at = now();
        if let Some(jobs) = self.enter_ladder(&mut entry)? {
            crate::repo::lock(&self.pending_replays).extend(jobs);
            return Ok(entry);
        }
        self.put_entry(&entry)?;
        self.emit(events::settled(&entry))?;
        Ok(entry)
    }

    /// [`Self::prepare`] before error classification. `report` is filled
    /// in as soon as the set check has run, so a later failure keeps it.
    fn try_prepare(
        &self,
        entry: &QueueEntry,
        head: Head,
        staged: &[Arc<Footprint>],
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
        if !self.ops_checked(entry.change)? {
            validate(self, entry.change, &record)?;
            self.store.mark_checked(entry.change)?;
        }
        // ADR 0034: a replay, however it was submitted, may not change the
        // acceptance tests it must satisfy.
        if let Some(Origin::Replay { of }) = entry.origin {
            let touched = self.tampered_tests(of, &record)?;
            if !touched.is_empty() {
                let names: Vec<&str> = touched.iter().map(|(_, name)| name.as_str()).collect();
                return Ok(Prepared::Park(QueueStatus::Rejected {
                    reason: format!("{TAMPERED}: {}", names.join(", ")),
                }));
            }
        }
        // ADR 0026: the landing base's policy judges the change.
        let policy = self.policy_at(head.snapshot)?;
        let (set_report, landed_writes) =
            self.set_check(entry.change, &record, head, staged, &policy)?;
        let report = report.insert(set_report);
        if let Err(reason) = &policy {
            // Nothing weaker than a readable policy judges a change.
            return Ok(Prepared::Park(QueueStatus::Parked {
                reason: format!("head's policy does not parse: {reason}"),
            }));
        }
        let rebased = match rebase(self, &record, head.snapshot, &landed_writes)? {
            Ok(rebased) => rebased,
            Err(merge) => {
                report.merge = merge;
                return Ok(Prepared::Park(QueueStatus::Conflicted));
            }
        };
        report.merge = rebased.soft;
        report.adapter_merged = rebased.adapter_merged;
        // An arbiter's resolution may change nothing ("keep ours"): it
        // lands to record the decision and its parents (spec §6.4 rung 3).
        let resolution = matches!(entry.origin, Some(Origin::Arbitration { .. }));
        if rebased.result == head.snapshot && !resolution {
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
        // ADR 0026 amendment: a policy file that does not parse never lands.
        if let Err(reason) = self.check_policy_file(head.snapshot, rebased.result)? {
            let reason = format!(
                "the change's {} does not parse: {reason}",
                hord_policy::POLICY_PATH
            );
            return Ok(Prepared::Park(QueueStatus::Rejected { reason }));
        }
        let checked_files = rebased.checked;
        let (landed_id, landed, attestation) =
            if rebased.result == record.result && record.base == head.snapshot {
                (entry.change, record, None)
            } else {
                // ADR 0018: recompute what the landed record says it did from
                // head → result; keep what the author depended on and why.
                let declared: Vec<IdentityDelta> = declared(&record.identity_deltas).collect();
                let (write_set, identity_deltas) =
                    sets_between(self, head.snapshot, rebased.result, &rebased.ops, &declared)?;
                let attestation = self.rebase_attestation(entry.change, &record, rebased.result);
                let mut evidence = record.evidence.clone();
                evidence.push(ObjectId::of(&attestation)?);
                // The first parent is the head it lands on; further parents
                // (an arbiter's resolution names the colliding changes) stay.
                let mut parents: Vec<ChangeId> = head.change.into_iter().collect();
                for parent in record.parents.iter().skip(1) {
                    if !parents.contains(parent) {
                        parents.push(*parent);
                    }
                }
                let landed = ChangeRecord {
                    base: head.snapshot,
                    result: rebased.result,
                    parents,
                    ops: rebased.ops,
                    write_set,
                    identity_deltas,
                    evidence,
                    signature: None,
                    rebased_from: Some(entry.change),
                    ..record
                };
                // Stored only when it lands (`finish`).
                (ObjectId::of(&landed)?, landed, Some(attestation))
            };
        // Spec §3.5: the record that lands must reproduce its result. The
        // submitted record was checked above; a rebased record has rewritten
        // ops, so it is checked again.
        let checked = landed_id == entry.change;
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
            attestation,
            policy,
        })))
    }

    /// The lander's `Rebase` attestation for `submitted` landing as
    /// `result` (ADR 0018 amendment). A pure function of the two records,
    /// so the same landing gives the same landed id anywhere: its time is
    /// the submitted record's `created_at`. Unsigned until M5.
    fn rebase_attestation(
        &self,
        submitted: ChangeId,
        record: &ChangeRecord,
        result: SnapshotId,
    ) -> Evidence {
        Evidence {
            kind: EvidenceKind::Rebase { submitted },
            qualifier: None,
            snapshot: result,
            toolchain: self.toolchain,
            command: "hord lander: structural rebase (spec §6.4 rung 1)".into(),
            scope: None,
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 0,
            produced_by: Actor::Agent {
                id: "hord-lander".into(),
                model: String::new(),
                model_hash: Bytes::default(),
                harness: "hord-txn".into(),
            },
            produced_at: record.provenance.created_at,
            signature: None,
        }
    }

    /// The id `change` landed under, if it is in the log (as submitted, or
    /// as its rebased record per a landed queue entry).
    pub(crate) fn landed_as(&self, change: ChangeId) -> Result<Option<ChangeId>> {
        if self.store.log_contains(change)? {
            return Ok(Some(change));
        }
        for e in self.named_entries(change)? {
            if let QueueStatus::Landed { landed } = e.status
                && e.change == change
                && self.store.log_contains(landed)?
            {
                return Ok(Some(landed));
            }
        }
        Ok(None)
    }

    /// Land `candidate` unless the verifier failed it or head's policy
    /// denies it, in one durable commit ([`hord_store::Store::land`]).
    ///
    /// The verdict's evidence is announced (`EvidenceAttached`) and listed
    /// in `Landed`; it is indexed by snapshot, never written into the
    /// record (ADR 0025). The policy is evaluated against every piece of
    /// evidence indexed for the candidate snapshot. A denial parks the
    /// change as [`QueueStatus::Parked`] with the violations in its report,
    /// or rejects it when a required check failed on this very snapshot.
    fn finish(&self, candidate: Candidate, verdict: Verdict) -> Result<QueueEntry> {
        let Candidate {
            mut entry,
            landed_id,
            landed,
            mut report,
            attestation,
            policy,
            facts,
            ..
        } = candidate;
        let mut announced = Vec::new();
        for id in verdict.evidence() {
            if let Ok(evidence) = self.get_object::<Evidence>(*id) {
                announced.push(events::evidence_attached(landed_id, *id, &evidence));
            }
        }
        self.emit(announced)?;
        let evidence = match verdict {
            Verdict::Fail { reason, .. } => {
                report.verification = Some(reason);
                entry.status = QueueStatus::Conflicted;
                entry.report = Some(report);
                return self.settle(entry);
            }
            Verdict::Pass { evidence } => evidence,
        };
        if let (Ok((policy, _)), Some(mut facts)) = (&policy, facts) {
            facts.evidence = self.candidate_evidence_facts(&landed, &report)?;
            if let hord_policy::Decision::Deny { reasons } = policy.evaluate(&facts) {
                let summary = reasons
                    .iter()
                    .map(|r| match &r.rule {
                        Some(rule) => format!("{} ({rule})", r.requirement),
                        None => r.requirement.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let failed = reasons
                    .iter()
                    .any(|r| r.evidence == hord_policy::EvidenceState::Failed);
                report.policy = reasons;
                entry.status = if failed {
                    QueueStatus::Rejected {
                        reason: format!("policy: required evidence failed: {summary}"),
                    }
                } else {
                    QueueStatus::Parked {
                        reason: format!("policy: missing {summary}"),
                    }
                };
                entry.report = Some(report);
                return self.settle(entry);
            }
        }
        let previous = self.head()?.change;
        if let Some(attestation) = &attestation {
            self.store.put_object(attestation)?;
        }
        if landed_id != entry.change {
            self.store.put_object(landed.as_ref())?;
        }
        entry.status = QueueStatus::Landed { landed: landed_id };
        entry.report = Some(report);
        entry.updated_at = now();
        let bytes = hord_encoding::encode(&entry.to_stored())?;
        let names: &[ChangeId] = if landed_id == entry.change {
            &[]
        } else {
            &[landed_id]
        };
        self.store.land(&hord_store::Landing {
            change: landed_id,
            record: landed.as_ref(),
            entry: (entry.seq, &bytes),
            names,
        })?;
        // Head is durable: the cache must follow before anything else can
        // fail, or the next landing would rebase onto the old head.
        self.set_head_cache(Head {
            change: Some(landed_id),
            snapshot: landed.result,
        });
        let position = self.store.log_len()?.saturating_sub(1) as u64;
        let submitted = (landed_id != entry.change).then_some(entry.change);
        self.emit(events::landed(
            landed_id, position, submitted, &evidence, previous,
        ))?;
        // The footprint is a cache (`footprint` recomputes it on a miss).
        if let Ok(footprint) = self.footprint_of(landed_id, &landed) {
            lock(&self.footprints).insert(landed_id, Arc::new(footprint), 1);
        }
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
        let policy = self.policy_at(head.snapshot)?;
        Ok(self.set_check(change, record, head, &[], &policy)?.0)
    }

    /// [`Self::set_report`] plus everything `L` wrote (write sets and coarse
    /// path ids), for the rebase's per-file fast path. Write-read conflicts
    /// count when the repository or head's `policy` asks for strict reads.
    fn set_check(
        &self,
        change: ChangeId,
        record: &ChangeRecord,
        head: Head,
        staged: &[Arc<Footprint>],
        policy: &HeadPolicy,
    ) -> Result<(ConflictReport, BTreeSet<hord_core::NodeId>)> {
        let strict_reads = self.config.strict_reads
            || policy
                .as_ref()
                .is_ok_and(|(policy, _)| policy.land().strict_reads);
        let mut landed = self.landed_since(record)?;
        landed.extend(staged.iter().cloned());
        let own = self.footprint_of(change, record)?;
        let conflicts = check(&own, &landed, strict_reads);
        let written = landed
            .iter()
            .flat_map(|f| f.writes.iter().copied())
            .collect();
        let report = ConflictReport {
            head: head.change,
            checked_against: landed.iter().map(|f| f.change).collect(),
            strict_reads,
            conflicts,
            ..ConflictReport::empty(change, record.base)
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
            return Ok(found);
        }
        let record = self.change_record(change)?;
        let footprint = Arc::new(self.footprint_of(change, &record)?);
        lock(&self.footprints).insert(change, Arc::clone(&footprint), 1);
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
        let parent = match record.parents.first() {
            Some(parent) => self.store.log_position(*parent)?,
            None => None,
        };
        let start = match parent {
            Some(i) => i + 1,
            None => {
                let log = self.store.log()?;
                self.position_of_snapshot(&log, record.base)?
                    .map_or(0, |i| i + 1)
            }
        };
        self.store
            .log_since(start)?
            .iter()
            .map(|c| self.footprint(*c))
            .collect()
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
    fn only_store_and_io_failures_are_transient() -> Result<(), Box<dyn std::error::Error>> {
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
        let path: RepoPath = "src/lib.rs".parse()?;
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
        Ok(())
    }
}
