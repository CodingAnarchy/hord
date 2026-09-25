//! The escalation ladder above the structural rebase (spec §6.4): rung 2
//! replays a conflicted change through a harness ([`crate::replay`]), and
//! rung 3 parks it for an arbiter.
//!
//! The ladder's state lives on the conflicted change's queue entry
//! ([`Escalation`]). A change that is conflicted (a hard merge conflict, or
//! a verification failure after a clean rebase, spec §6.5) goes to
//! [`QueueStatus::Replaying`] when the repository has a harness and head's
//! policy allows replays (`[land] max_replay_attempts`, ADR 0026). Each
//! attempt (ADR 0029) runs the harness once under head's `[replay] budget`
//! (ADR 0028). A proposed change is recorded with `parent_intent` naming
//! the original and submitted like any other ([`Origin::Replay`]); when it
//! lands, the original is [`QueueStatus::Replayed`]. When it does not, or
//! the harness gave up, was killed, went over budget, or failed, the next
//! attempt runs, up to `max_replay_attempts`. Then the change is
//! [`QueueStatus::NeedsArbitration`], with a [`ConflictSummary`] and every
//! attempt's proposed change as a candidate, deduplicated by semantic ops.
//!
//! An arbiter ([`Repo::arbitrate`]) keeps what landed, takes the parked
//! change, asks for one more replay with a note, or names a change that
//! resolves it. The resolution is submitted with [`Origin::Arbitration`]
//! and lands as a change whose parents include the parked change and the
//! changes it collided with; when it lands, the parked change is
//! [`QueueStatus::Arbitrated`] and an `Arbitrated` event records the
//! arbiter and their signature. A conflicted change with no harness to
//! replay it can be arbitrated directly.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

use hord_core::sign::{self, PublicKey, SigningKey};
use hord_core::{
    Acceptance, Actor, Bytes, ChangeId, ChangeRecord, Intent, IntentRef, NodeId, ObjectId,
    Provenance, RepoPath, Signature, SnapshotId,
};
use serde::{Deserialize, Serialize};

use crate::events;
use crate::lander::{QueueEntry, QueueStatus};
use crate::repo::{Base, BeginOptions, Inner, Repo, blocking, lock, now};
use crate::summary::ConflictSummary;
use crate::{Error, Result};

/// Why an entry was submitted, when it is part of another change's
/// escalation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Origin {
    /// A replay of `of` (its record's `provenance.parent_intent`).
    Replay {
        /// The change it replays.
        of: ChangeId,
    },
    /// An arbiter's resolution of the parked change `of`.
    Arbitration {
        /// The parked change.
        of: ChangeId,
    },
}

/// How a replay attempt ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReplayOutcome {
    /// The harness is still running.
    Running,
    /// It proposed a change, which was submitted to the lander.
    Proposed,
    /// It gave up.
    GaveUp,
    /// It ran past its wall-clock budget and was killed.
    Killed,
    /// It reported tokens or cost over its budget; its result was rejected.
    OverBudget,
    /// It failed, or broke the protocol.
    Failed,
    /// It changed an acceptance test it must satisfy (ADR 0034); its result
    /// was rejected before verification.
    Tampered,
}

/// One replay attempt (ADR 0029: one replay per attempt).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayAttempt {
    /// Attempt number, from 1.
    pub attempt: u32,
    /// The harness that ran it.
    pub harness: String,
    /// How it ended.
    pub outcome: ReplayOutcome,
    /// The replay change submitted to the lander, with `parent_intent` set.
    pub change: Option<ChangeId>,
    /// What happened, in words: the harness's reason, or how the replay
    /// change settled.
    pub detail: Option<String>,
    /// Wall-clock time the harness ran, in milliseconds.
    pub elapsed_ms: u64,
    /// Tokens the harness reported.
    pub tokens: Option<u64>,
    /// Cost the harness reported, in micro-dollars.
    pub cost_micros: Option<u64>,
    /// Model the harness reported.
    pub model: Option<String>,
    /// The arbiter's note it ran with, if an arbiter asked for it.
    pub note: Option<String>,
    /// For a tampered attempt: the protected tests it changed (ADR 0034).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tampered: Vec<NodeId>,
}

/// A replay result offered to the arbiter. Attempts with equal semantic
/// ops are one candidate (ADR 0029).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArbitrationCandidate {
    /// The replay change (`Arbitration::Resolved` lands it).
    pub change: ChangeId,
    /// The attempts that produced it.
    pub attempts: Vec<u32>,
    /// Its base snapshot.
    pub base: SnapshotId,
    /// Its result snapshot.
    pub result: SnapshotId,
    /// Number of ops.
    pub ops: u32,
    /// How it settled in the lander.
    pub settled: String,
}

/// An arbiter's resolution that was submitted and has not landed yet.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingResolution {
    /// The resolution as submitted.
    pub change: ChangeId,
    /// Who decided.
    pub by: Actor,
    /// The arbiter's signature over the decision
    /// ([`arbitration_message`]), stored on the `Arbitrated` event.
    pub signature: Option<Signature>,
}

/// A conflicted change's way up the ladder.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Escalation {
    /// Replay attempts, oldest first.
    pub attempts: Vec<ReplayAttempt>,
    /// Candidates for the arbiter, once parked.
    pub candidates: Vec<ArbitrationCandidate>,
    /// The conflict summary the harness and the arbiter get.
    pub summary: Option<ConflictSummary>,
    /// A resolution submitted by an arbiter, not landed yet.
    pub resolution: Option<PendingResolution>,
    /// Why the last resolution or replay did not help, if it did not.
    pub note: Option<String>,
    /// Files the last "take theirs" resolution took whole from the parked
    /// change, because hord cannot merge them by definition (blob-tier or
    /// unparseable files, or a conflict on the file itself).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub whole_file: Vec<RepoPath>,
}

/// What an arbiter decided (spec §10.5.2 `Arbitration`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Arbitration {
    /// Keep what landed; the parked change's effect is dropped.
    PickOurs,
    /// Take the parked change over what landed: every file it changed gets
    /// its version.
    PickTheirs,
    /// Hand it to the replay harness once more, with a note.
    Replay {
        /// Added to the replay request.
        note: Option<String>,
    },
    /// This change resolves it: a replay candidate, or one the arbiter made.
    Resolved(ChangeId),
}

/// Who arbitrates, and their signature over the decision.
///
/// [`Repo::arbitrate`] checks that a signature verifies with the key it
/// names ([`verify_arbitration`]); that the key belongs to `actor` is the
/// server's auth layer's check (spec §10.5.4), which knows the keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Arbiter {
    /// The arbiter; the resolution's author.
    pub actor: Actor,
    /// Their signature over [`arbitration_message`] under
    /// [`ARBITRATION_DOMAIN`].
    pub signature: Option<Signature>,
}

/// How the lander words a replay rejected for changing a protected test
/// (ADR 0034); a rejection reason starts with it.
pub(crate) const TAMPERED: &str = "changes acceptance tests it must satisfy";

/// Signature domain of an arbiter's decision (`hord_core::sign`).
pub const ARBITRATION_DOMAIN: &str = "hord.arbitration";

/// What an arbiter signs: the decision, canonically encoded.
#[derive(Serialize)]
struct Decision<'a> {
    change: ChangeId,
    action: &'a str,
    resolved: Option<ChangeId>,
    note: Option<&'a str>,
}

/// The message an arbiter signs for `action` on the parked `change`: the
/// [`ObjectId`] bytes of the decision's canonical encoding (the parked
/// change, the action, the resolving change, and the note).
pub fn arbitration_message(change: ChangeId, action: &Arbitration) -> Result<ObjectId> {
    let (name, resolved, note) = match action {
        Arbitration::PickOurs => ("pick_ours", None, None),
        Arbitration::PickTheirs => ("pick_theirs", None, None),
        Arbitration::Replay { note } => ("replay", None, note.as_deref()),
        Arbitration::Resolved(id) => ("resolved", Some(*id), None),
    };
    Ok(ObjectId::of(&Decision {
        change,
        action: name,
        resolved,
        note,
    })?)
}

/// Sign `action` on `change` with `key`.
pub fn sign_arbitration(
    change: ChangeId,
    action: &Arbitration,
    key: &SigningKey,
) -> Result<Signature> {
    let message = arbitration_message(change, action)?;
    Ok(key.sign(ARBITRATION_DOMAIN, message.as_bytes()))
}

/// Check `signature` over `action` on `change` with `key`.
pub fn verify_arbitration(
    change: ChangeId,
    action: &Arbitration,
    signature: &Signature,
    key: &PublicKey,
) -> Result<()> {
    let message = arbitration_message(change, action)?;
    key.verify(ARBITRATION_DOMAIN, message.as_bytes(), signature)
        .map_err(|err| Error::BadSignature(err.to_string()))
}

/// One replay attempt to run.
#[derive(Clone, Debug)]
pub(crate) struct ReplayJob {
    /// The conflicted change.
    pub change: ChangeId,
    /// Attempt number.
    pub attempt: u32,
    /// The arbiter's note, if any.
    pub note: Option<String>,
}

/// Replays running in this process: one task per conflicted change, with
/// the attempts queued behind the one it is running.
#[derive(Debug, Default)]
pub(crate) struct Replays {
    running: Mutex<HashMap<ChangeId, VecDeque<ReplayJob>>>,
}

impl Replays {
    /// Queue `job`. Returns `true` when no task runs its change yet: the
    /// caller starts one, which takes the job with [`Self::next`].
    pub fn start(&self, job: ReplayJob) -> bool {
        let mut running = lock(&self.running);
        match running.get_mut(&job.change) {
            Some(queued) => {
                queued.push_back(job);
                false
            }
            None => {
                running.insert(job.change, VecDeque::from([job]));
                true
            }
        }
    }

    /// The next attempt queued for `change`; `None` ends its task, which is
    /// then no longer running.
    pub fn next(&self, change: ChangeId) -> Option<ReplayJob> {
        let mut running = lock(&self.running);
        let next = running.get_mut(&change).and_then(VecDeque::pop_front);
        if next.is_none() {
            running.remove(&change);
        }
        next
    }

    pub fn is_running(&self, change: ChangeId) -> bool {
        lock(&self.running).contains_key(&change)
    }

    pub fn any(&self) -> bool {
        !lock(&self.running).is_empty()
    }
}

/// How the lander describes a settled replay or resolution.
fn settled_text(status: &QueueStatus) -> String {
    match status {
        QueueStatus::Queued => "queued".into(),
        QueueStatus::Landed { landed } => format!("landed as {landed}"),
        QueueStatus::Conflicted => "conflicted".into(),
        QueueStatus::Rejected { reason } => format!("rejected: {reason}"),
        QueueStatus::Parked { reason } => format!("parked: {reason}"),
        QueueStatus::Replaying { attempt } => format!("replaying (attempt {attempt})"),
        QueueStatus::NeedsArbitration => "needs arbitration".into(),
        QueueStatus::Replayed { landed } => format!("replayed as {landed}"),
        QueueStatus::Arbitrated { landed } => format!("arbitrated as {landed}"),
    }
}

/// The lander's own actor, for workspaces it opens for a harness.
pub(crate) fn lander_actor() -> Actor {
    Actor::Agent {
        id: "hord-lander".into(),
        model: String::new(),
        model_hash: Bytes::default(),
        harness: "hord-txn".into(),
    }
}

impl Inner {
    /// Head's `max_replay_attempts` and `[replay] budget` (ADR 0026, ADR
    /// 0028). A head policy that does not parse gives the defaults.
    pub(crate) fn replay_limits(&self) -> Result<(u64, hord_core::ReplayBudget)> {
        let head = self.head()?;
        Ok(match self.policy_at(head.snapshot)? {
            Ok((policy, _)) => (
                policy.land().max_replay_attempts,
                policy.replay().budget.clone(),
            ),
            Err(_) => (
                hord_policy::DEFAULT_MAX_REPLAY_ATTEMPTS,
                hord_core::ReplayBudget::default(),
            ),
        })
    }

    /// The latest queue entry submitted as `change` (not one that landed
    /// under that id).
    pub(crate) fn submitted_entry(&self, change: ChangeId) -> Result<QueueEntry> {
        self.named_entries(change)?
            .into_iter()
            .rev()
            .find(|e| e.change == change)
            .ok_or(Error::NotQueued(change))
    }

    /// Write `entry` with a fresh `updated_at`.
    fn rewrite(&self, entry: &mut QueueEntry) -> Result<()> {
        entry.updated_at = now();
        self.put_entry(entry)
    }

    /// Move `entry`, just settled by the lander, along the ladder. Returns
    /// it as it now stands and the replays to start.
    pub(crate) fn escalate(&self, entry: QueueEntry) -> Result<(QueueEntry, Vec<ReplayJob>)> {
        let _ladder = lock(&self.ladder);
        match entry.origin {
            Some(Origin::Replay { of }) => {
                let jobs = self.replay_settled(of, &entry)?;
                Ok((entry, jobs))
            }
            Some(Origin::Arbitration { of }) => {
                self.resolution_settled(of, &entry)?;
                Ok((entry, Vec::new()))
            }
            None => Ok((entry, Vec::new())),
        }
    }

    /// Put `entry`, a change settling as conflicted, on rung 2 (or, with no
    /// replay allowed, rung 3) in the same write that settles it, so no
    /// reader sees it conflicted first. `None` when it does not enter the
    /// ladder: a replay or a resolution (their original's escalation moves
    /// instead, [`Self::escalate`]), or no harness.
    pub(crate) fn enter_ladder(&self, entry: &mut QueueEntry) -> Result<Option<Vec<ReplayJob>>> {
        if entry.status != QueueStatus::Conflicted
            || entry.origin.is_some()
            || self.harness.is_none()
        {
            return Ok(None);
        }
        let _ladder = lock(&self.ladder);
        let summary = entry
            .report
            .as_ref()
            .and_then(|report| self.conflict_summary(entry.change, report).ok());
        entry.escalation = Some(Escalation {
            summary,
            ..Escalation::default()
        });
        self.next_attempt(entry, None).map(Some)
    }

    /// Start the next attempt on `entry` (conflicted, with an escalation),
    /// or park it for arbitration when attempts ran out. Writes the entry.
    /// `note` is an arbiter's: that attempt runs whatever the limit.
    fn next_attempt(&self, entry: &mut QueueEntry, note: Option<String>) -> Result<Vec<ReplayJob>> {
        let (max, _) = self.replay_limits()?;
        let escalation = entry.escalation.get_or_insert_with(Escalation::default);
        let automatic = escalation
            .attempts
            .iter()
            .filter(|a| a.note.is_none())
            .count() as u64;
        let after_arbiter = escalation.attempts.last().is_some_and(|a| a.note.is_some());
        let Some(harness) = &self.harness else {
            return self.park_for_arbitration(entry).map(|()| Vec::new());
        };
        if note.is_none() && (after_arbiter || automatic >= max) {
            self.park_for_arbitration(entry)?;
            return Ok(Vec::new());
        }
        let attempt = u32::try_from(escalation.attempts.len() + 1).unwrap_or(u32::MAX);
        let name = harness.name();
        escalation.attempts.push(ReplayAttempt {
            attempt,
            harness: name.clone(),
            outcome: ReplayOutcome::Running,
            change: None,
            detail: None,
            elapsed_ms: 0,
            tokens: None,
            cost_micros: None,
            model: None,
            note: note.clone(),
            tampered: Vec::new(),
        });
        entry.status = QueueStatus::Replaying { attempt };
        self.rewrite(entry)?;
        self.emit(vec![events::replaying(entry.change, attempt, &name)])?;
        Ok(vec![ReplayJob {
            change: entry.change,
            attempt,
            note,
        }])
    }

    /// Park `entry` for an arbiter, with its candidates, and announce it.
    fn park_for_arbitration(&self, entry: &mut QueueEntry) -> Result<()> {
        let report = entry.report.clone();
        let escalation = entry.escalation.get_or_insert_with(Escalation::default);
        if escalation.summary.is_none()
            && let Some(report) = &report
        {
            escalation.summary = self.conflict_summary(entry.change, report).ok();
        }
        escalation.candidates = self.candidates(&escalation.attempts)?;
        entry.status = QueueStatus::NeedsArbitration;
        let tried = escalation.attempts.len();
        let detail = match &escalation.note {
            Some(note) => note.clone(),
            None => format!(
                "{tried} replay attempt{} did not land; {} candidate{}",
                if tried == 1 { "" } else { "s" },
                escalation.candidates.len(),
                if escalation.candidates.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ),
        };
        self.rewrite(entry)?;
        self.emit(vec![events::needs_arbitration(entry.change, detail)])?;
        Ok(())
    }

    /// Every attempt's proposed change, one candidate per distinct set of
    /// ops (ADR 0029), in attempt order.
    fn candidates(&self, attempts: &[ReplayAttempt]) -> Result<Vec<ArbitrationCandidate>> {
        let mut by_ops: BTreeMap<ObjectId, usize> = BTreeMap::new();
        let mut out: Vec<ArbitrationCandidate> = Vec::new();
        for attempt in attempts {
            let Some(change) = attempt.change else {
                continue;
            };
            let record = match self.change_record(change) {
                Ok(record) => record,
                Err(Error::MissingChange(_)) => continue,
                Err(err) => return Err(err),
            };
            let key = ObjectId::of(&record.ops)?;
            if let Some(&i) = by_ops.get(&key) {
                out[i].attempts.push(attempt.attempt);
                continue;
            }
            by_ops.insert(key, out.len());
            out.push(ArbitrationCandidate {
                change,
                attempts: vec![attempt.attempt],
                base: record.base,
                result: record.result,
                ops: u32::try_from(record.ops.len()).unwrap_or(u32::MAX),
                settled: attempt.detail.clone().unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// A replay of `of` settled as `replay`: resolve `of` if it landed,
    /// else record why and go on to the next attempt.
    fn replay_settled(&self, of: ChangeId, replay: &QueueEntry) -> Result<Vec<ReplayJob>> {
        let Ok(mut entry) = self.submitted_entry(of) else {
            // A replay of a change this queue never saw: nothing to update.
            return Ok(Vec::new());
        };
        if matches!(
            entry.status,
            QueueStatus::Replayed { .. }
                | QueueStatus::Arbitrated { .. }
                | QueueStatus::Landed { .. }
        ) {
            return Ok(Vec::new());
        }
        let settled = settled_text(&replay.status);
        let escalation = entry.escalation.get_or_insert_with(Escalation::default);
        match escalation
            .attempts
            .iter_mut()
            .find(|a| a.change == Some(replay.change))
        {
            Some(attempt) => attempt.detail = Some(settled.clone()),
            None if matches!(&replay.status, QueueStatus::Rejected { reason } if reason.starts_with(TAMPERED)) =>
            {
                // A replay proposed outside the lander that changed a
                // protected test (ADR 0034): rejected at prepare.
                let attempt = u32::try_from(escalation.attempts.len() + 1).unwrap_or(u32::MAX);
                let record = self.change_record(replay.change)?;
                let tampered = self
                    .protected_tests(of, record.base)?
                    .into_iter()
                    .map(|(node, _)| node)
                    .filter(|node| record.write_set.contains(node))
                    .collect();
                escalation.attempts.push(ReplayAttempt {
                    attempt,
                    harness: "manual".into(),
                    outcome: ReplayOutcome::Tampered,
                    change: None,
                    detail: Some(settled.clone()),
                    elapsed_ms: 0,
                    tokens: None,
                    cost_micros: None,
                    model: None,
                    note: None,
                    tampered,
                });
            }
            None => {
                // Proposed outside the lander (`hord replay`): record it.
                let attempt = u32::try_from(escalation.attempts.len() + 1).unwrap_or(u32::MAX);
                escalation.attempts.push(ReplayAttempt {
                    attempt,
                    harness: "manual".into(),
                    outcome: ReplayOutcome::Proposed,
                    change: Some(replay.change),
                    detail: Some(settled.clone()),
                    elapsed_ms: 0,
                    tokens: None,
                    cost_micros: None,
                    model: None,
                    note: None,
                    tampered: Vec::new(),
                });
            }
        }
        if let QueueStatus::Landed { landed } = replay.status {
            entry.status = QueueStatus::Replayed { landed };
            escalation.note = None;
            self.rewrite(&mut entry)?;
            return Ok(Vec::new());
        }
        match entry.status {
            // Waiting on this replay: it failed, so try again.
            QueueStatus::Replaying { .. }
                if escalation.attempts.last().map(|a| a.change) == Some(Some(replay.change)) =>
            {
                self.next_attempt(&mut entry, None)
            }
            // Parked or still replaying something else: keep the record.
            _ => {
                if entry.status == QueueStatus::NeedsArbitration {
                    escalation.candidates = self.candidates(&escalation.attempts)?;
                }
                self.rewrite(&mut entry)?;
                Ok(Vec::new())
            }
        }
    }

    /// An arbiter's resolution of `of` settled as `resolution`: on landing,
    /// `of` is arbitrated and the `Arbitrated` event records who decided;
    /// otherwise `of` goes back to the arbitration queue with the reason.
    fn resolution_settled(&self, of: ChangeId, resolution: &QueueEntry) -> Result<()> {
        let Ok(mut entry) = self.submitted_entry(of) else {
            return Ok(());
        };
        let escalation = entry.escalation.get_or_insert_with(Escalation::default);
        let pending = escalation
            .resolution
            .take_if(|p| p.change == resolution.change);
        if let QueueStatus::Landed { landed } = resolution.status {
            entry.status = QueueStatus::Arbitrated { landed };
            escalation.note = None;
            self.rewrite(&mut entry)?;
            let arbiter = pending.map_or_else(
                || Arbiter {
                    actor: lander_actor(),
                    signature: None,
                },
                |p| Arbiter {
                    actor: p.by,
                    signature: p.signature,
                },
            );
            self.emit(vec![events::arbitrated(of, &arbiter, landed)])?;
            return Ok(());
        }
        escalation.note = Some(format!(
            "resolution {} did not land: {}",
            resolution.change,
            settled_text(&resolution.status)
        ));
        self.park_for_arbitration(&mut entry)
    }

    /// Entries left replaying by a lander that stopped mid-attempt: record
    /// the attempt as interrupted and go on. Returns the replays to start.
    pub(crate) fn resume_ladders(&self) -> Result<Vec<ReplayJob>> {
        let _ladder = lock(&self.ladder);
        let mut jobs = Vec::new();
        for mut entry in self.queue_entries()? {
            if !matches!(entry.status, QueueStatus::Replaying { .. })
                || self.replays.is_running(entry.change)
            {
                continue;
            }
            let Some(escalation) = entry.escalation.as_mut() else {
                continue;
            };
            let Some(last) = escalation.attempts.last_mut() else {
                continue;
            };
            if last.outcome != ReplayOutcome::Running {
                // Waiting on a replay change in the queue.
                continue;
            }
            last.outcome = ReplayOutcome::Failed;
            last.detail = Some("interrupted: the lander stopped during the attempt".into());
            jobs.extend(self.next_attempt(&mut entry, None)?);
        }
        Ok(jobs)
    }

    /// Record how attempt `job` ended, and submit what it proposed. Returns
    /// the next attempt to run, if any.
    pub(crate) fn finish_attempt(
        &self,
        job: &ReplayJob,
        base: SnapshotId,
        budget: &hord_core::ReplayBudget,
        ended: crate::replay::Ended,
    ) -> Result<Vec<ReplayJob>> {
        use crate::replay::Ended;
        let _ladder = lock(&self.ladder);
        let mut entry = self.submitted_entry(job.change)?;
        let Some(escalation) = entry.escalation.as_mut() else {
            return Ok(Vec::new());
        };
        let Some(attempt) = escalation
            .attempts
            .iter_mut()
            .find(|a| a.attempt == job.attempt)
        else {
            return Ok(Vec::new());
        };
        attempt.elapsed_ms = ended.elapsed_ms();
        let (outcome, detail) = match ended {
            Ended::Killed { .. } => (
                ReplayOutcome::Killed,
                format!(
                    "ran past its wall-clock budget of {} ms and was killed",
                    budget.wall_time_ms
                ),
            ),
            Ended::Failed { reason, .. } => (ReplayOutcome::Failed, reason),
            Ended::Returned { result, .. } => {
                attempt.tokens = result.tokens;
                attempt.cost_micros = result.cost_micros;
                attempt.model = result.model.clone();
                match crate::replay::over_budget(&result, budget) {
                    Some(why) => (ReplayOutcome::OverBudget, why),
                    None => {
                        match self.accept_replay(job.change, base, &result, &mut attempt.tampered) {
                            Ok(Ok(change)) => {
                                attempt.change = Some(change);
                                (ReplayOutcome::Proposed, "submitted".into())
                            }
                            Ok(Err((outcome, why))) => (outcome, why),
                            Err(err) if crate::lander::is_transient(&err) => return Err(err),
                            Err(err) => (ReplayOutcome::Failed, err.to_string()),
                        }
                    }
                }
            }
        };
        attempt.outcome = outcome;
        attempt.detail = Some(detail);
        if outcome == ReplayOutcome::Proposed {
            // Recorded before the replay is submitted, so the lander finds
            // the attempt when it settles.
            let replay = attempt.change;
            self.rewrite(&mut entry)?;
            if let Some(replay) = replay {
                self.submit_as(replay, Some(Origin::Replay { of: job.change }))?;
            }
            return Ok(Vec::new());
        }
        self.next_attempt(&mut entry, None)
    }

    /// Check what a harness proposed for `of` on `base`, and record it as a
    /// replay: `provenance.parent_intent = of` (spec §6.6, "Hord records
    /// parent_intent"). Returns the replay change to submit, or why the
    /// result is not one.
    fn accept_replay(
        &self,
        of: ChangeId,
        base: SnapshotId,
        result: &hord_api::proto::ReplayResult,
        tampered: &mut Vec<NodeId>,
    ) -> Result<std::result::Result<ChangeId, (ReplayOutcome, String)>> {
        use hord_api::proto::replay_result::Status;
        let proposed = match &result.status {
            Some(Status::Proposed(p)) => &p.change,
            Some(Status::GaveUp(g)) => return Ok(Err((ReplayOutcome::GaveUp, g.reason.clone()))),
            None => {
                return Ok(Err((
                    ReplayOutcome::Failed,
                    "the result has no status".into(),
                )));
            }
        };
        let Ok(id) = proposed.parse::<ChangeId>() else {
            return Ok(Err((
                ReplayOutcome::Failed,
                format!("proposed {proposed:?}, which is not a change id"),
            )));
        };
        let record = match self.change_record(id) {
            Ok(record) => record,
            Err(Error::MissingChange(_)) => {
                return Ok(Err((
                    ReplayOutcome::Failed,
                    format!("proposed change {id} is not stored in the repository"),
                )));
            }
            Err(err) => return Err(err),
        };
        if record.base != base {
            return Ok(Err((
                ReplayOutcome::Failed,
                format!(
                    "proposed change {id} is based on {}, not on the replay's base {base}",
                    record.base
                ),
            )));
        }
        // ADR 0034: a replay may not change the acceptance tests it must
        // satisfy. Rejected before verification, like an over-budget result.
        let protected = self.protected_tests(of, base)?;
        let touched: Vec<(NodeId, String)> = protected
            .into_iter()
            .filter(|(node, _)| record.write_set.contains(node))
            .collect();
        if !touched.is_empty() {
            let names: Vec<&str> = touched.iter().map(|(_, name)| name.as_str()).collect();
            let why = format!("{TAMPERED}: {} (proposed change {id})", names.join(", "));
            self.emit(vec![events::rejected(id, &why)])?;
            *tampered = touched.into_iter().map(|(node, _)| node).collect();
            return Ok(Err((ReplayOutcome::Tampered, why)));
        }
        match record.provenance.parent_intent {
            Some(parent) if parent == of => return Ok(Ok(id)),
            Some(other) => {
                return Ok(Err((
                    ReplayOutcome::Failed,
                    format!("proposed change {id} is a replay of {other}, not of {of}"),
                )));
            }
            None => {}
        }
        let checked = self.was_proposed(id) || self.store.is_checked(id)?;
        let replay = as_replay(record, of);
        let replay_id = self.store.put_object(&replay)?;
        if checked {
            // Same base, result, and ops as the checked record.
            self.store.mark_checked(replay_id)?;
        }
        Ok(Ok(replay_id))
    }
}

impl Inner {
    /// The acceptance tests a replay of `of` must not change (ADR 0034):
    /// the `test` acceptance criteria of `of`'s intent and of the intents
    /// of the changes it collided with, by name.
    pub(crate) fn protected_test_names(&self, of: ChangeId) -> Result<Vec<String>> {
        let mut changes = vec![of];
        if let Ok(entry) = self.submitted_entry(of)
            && let Some(report) = &entry.report
        {
            changes.extend(Self::colliders(report));
        }
        let mut names = Vec::new();
        for change in changes {
            let record = match self.change_record(change) {
                Ok(record) => record,
                Err(Error::MissingChange(_)) => continue,
                Err(err) => return Err(err),
            };
            for acceptance in &record.intent.acceptance {
                if let Acceptance::Test { name } = acceptance
                    && !names.contains(name)
                {
                    names.push(name.clone());
                }
            }
        }
        Ok(names)
    }

    /// Names of the protected tests (ADR 0034) that `record`, a replay of
    /// `of`, changes: in its write set, resolved at its base.
    pub(crate) fn tampered_tests(
        &self,
        of: ChangeId,
        record: &ChangeRecord,
    ) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .protected_tests(of, record.base)?
            .into_iter()
            .filter(|(node, _)| record.write_set.contains(node))
            .map(|(_, name)| name)
            .collect();
        out.dedup();
        Ok(out)
    }

    /// [`Self::protected_test_names`] resolved to definitions in `snapshot`
    /// (a replay's base), with their names. A name that resolves to no
    /// definition there protects nothing (ADR 0034).
    fn protected_tests(&self, of: ChangeId, snapshot: SnapshotId) -> Result<Vec<(NodeId, String)>> {
        let mut out = Vec::new();
        for name in self.protected_test_names(of)? {
            for node in self.resolve_in(snapshot, &name)? {
                out.push((node, name.clone()));
            }
        }
        Ok(out)
    }
}

/// `record` as a replay of `of`: `provenance.parent_intent = of`, with the
/// author's signature dropped (it covered the record without it). Base,
/// result, and ops are unchanged, so a check of `record`'s ops holds for
/// the replay (spec §6.6: Hord records `parent_intent`).
#[must_use]
pub fn as_replay(record: ChangeRecord, of: ChangeId) -> ChangeRecord {
    ChangeRecord {
        provenance: Provenance {
            parent_intent: Some(of),
            ..record.provenance
        },
        signature: None,
        ..record
    }
}

/// The empty record "keep ours" lands: head to head, with the parents given.
fn keep_ours(
    head: crate::Head,
    toolchain: ObjectId,
    parents: Vec<ChangeId>,
    intent: Intent,
    by: &Actor,
) -> ChangeRecord {
    ChangeRecord {
        base: head.snapshot,
        result: head.snapshot,
        parents,
        ops: Vec::new(),
        intent,
        provenance: Provenance {
            actor: by.clone(),
            toolchain,
            created_at: now(),
            session: None,
            parent_intent: None,
        },
        read_set: Default::default(),
        write_set: Default::default(),
        identity_deltas: Vec::new(),
        evidence: Vec::new(),
        signature: None,
        rebased_from: None,
    }
}

/// `first`, then the parked change, then the changes it collided with,
/// without repeats (spec §6.4: the resolution's parents include both
/// colliding changes).
fn resolution_parents(
    first: &[ChangeId],
    parked: ChangeId,
    colliders: &[ChangeId],
) -> Vec<ChangeId> {
    let mut parents: Vec<ChangeId> = Vec::new();
    for id in first.iter().chain([&parked]).chain(colliders) {
        if !parents.contains(id) {
            parents.push(*id);
        }
    }
    parents
}

impl Repo {
    /// Resolve the parked `change` (spec §6.4 rung 3, §10.5.2).
    ///
    /// `change` must be [`QueueStatus::NeedsArbitration`], or
    /// [`QueueStatus::Conflicted`] with no harness to replay it. With
    /// [`Arbitration::Replay`], the harness runs once more with the note and
    /// the returned id is `change`. Otherwise the resolution is submitted to
    /// the lander as a change whose parents are head, `change`, and the
    /// changes it collided with, and its id is returned; when it lands,
    /// `change` becomes [`QueueStatus::Arbitrated`] and an `Arbitrated`
    /// event names `arbiter`. Returns the id and `change`'s entry.
    pub async fn arbitrate(
        &self,
        change: ChangeId,
        action: Arbitration,
        arbiter: Arbiter,
    ) -> Result<(ChangeId, QueueEntry)> {
        if let Some(signature) = &arbiter.signature {
            let key =
                sign::signer(signature).map_err(|err| Error::BadSignature(err.to_string()))?;
            verify_arbitration(change, &action, signature, &key)?;
        }
        let entry = blocking(&self.inner, move |inner| {
            let entry = inner.submitted_entry(change)?;
            if !entry.status.is_arbitrable() {
                return Err(Error::NotArbitrable {
                    change,
                    status: settled_text(&entry.status),
                });
            }
            if let Some(pending) = entry
                .escalation
                .as_ref()
                .and_then(|e| e.resolution.as_ref())
            {
                return Err(Error::NotArbitrable {
                    change,
                    status: format!("resolution {} is already in the queue", pending.change),
                });
            }
            Ok(entry)
        })
        .await?;
        let record = self.change(change).await?;
        let colliders = entry
            .report
            .as_ref()
            .map(Inner::colliders)
            .unwrap_or_default();
        let head = self.head().await?;
        let refs: Vec<IntentRef> = std::iter::once(change)
            .chain(colliders.iter().copied())
            .map(|change| IntentRef::Change { change })
            .collect();
        let parked_summary = record.intent.summary.clone();
        let parked_acceptance = record.intent.acceptance.clone();
        let mut whole_file: Vec<RepoPath> = Vec::new();
        let resolution = match action {
            Arbitration::Replay { note } => {
                if self.inner.harness.is_none() {
                    return Err(Error::NoHarness);
                }
                let (entry, jobs) = blocking(&self.inner, move |inner| {
                    let _ladder = lock(&inner.ladder);
                    let mut entry = inner.submitted_entry(change)?;
                    let jobs = inner.next_attempt(&mut entry, Some(note.unwrap_or_default()))?;
                    Ok((entry, jobs))
                })
                .await?;
                for job in jobs {
                    crate::replay::spawn_replay(self, job);
                }
                return Ok((change, entry));
            }
            Arbitration::PickOurs => {
                let intent = Intent {
                    summary: format!(
                        "Arbitrate: keep what landed over \"{}\"",
                        record.intent.summary
                    ),
                    body: summary_body(&entry),
                    refs,
                    acceptance: Vec::new(),
                };
                keep_ours(
                    head,
                    self.inner.toolchain,
                    resolution_parents(
                        &head.change.into_iter().collect::<Vec<_>>(),
                        change,
                        &colliders,
                    ),
                    intent,
                    &arbiter.actor,
                )
            }
            Arbitration::PickTheirs => {
                let mut ws = self
                    .begin(BeginOptions {
                        base: Base::Head,
                        actor: arbiter.actor.clone(),
                        session: None,
                    })
                    .await?;
                let on = ws.base();
                let theirs =
                    blocking(&self.inner, move |inner| inner.merge_theirs(&record, on)).await?;
                let mut body = summary_body(&entry);
                if !theirs.whole_file.is_empty() {
                    let list: Vec<String> =
                        theirs.whole_file.iter().map(ToString::to_string).collect();
                    body.push_str(&format!(
                        "\nTaken whole from the parked change (hord cannot merge them by definition): {}\n",
                        list.join(", ")
                    ));
                }
                whole_file = theirs.whole_file;
                let intent = Intent {
                    summary: format!("Arbitrate: take \"{}\"", parked_summary),
                    body,
                    refs,
                    acceptance: parked_acceptance,
                };
                for (path, bytes) in theirs.files {
                    match bytes {
                        Some(bytes) => ws.write_file(&path, bytes.as_slice().to_vec()).await?,
                        None => {
                            if ws.read_file(&path).await?.is_some() {
                                ws.delete_file(&path).await?;
                            }
                        }
                    }
                }
                let base_parents: Vec<ChangeId> = ws.base_change().into_iter().collect();
                let wsbase = ws.base();
                match ws.propose(intent.clone()).await {
                    Ok(proposal) => ChangeRecord {
                        parents: resolution_parents(&proposal.record.parents, change, &colliders),
                        ..proposal.record
                    },
                    // Head already has their version of every file.
                    Err(Error::NothingToPropose) => keep_ours(
                        crate::Head {
                            change: base_parents.first().copied(),
                            snapshot: wsbase,
                        },
                        self.inner.toolchain,
                        resolution_parents(&base_parents, change, &colliders),
                        intent,
                        &arbiter.actor,
                    ),
                    Err(err) => return Err(err),
                }
            }
            Arbitration::Resolved(given) => {
                let given_record = self.change(given).await?;
                ChangeRecord {
                    parents: resolution_parents(&given_record.parents, change, &colliders),
                    // The author signed the record with its own parents.
                    signature: None,
                    ..given_record
                }
            }
        };
        blocking(&self.inner, move |inner| {
            let _ladder = lock(&inner.ladder);
            let id = inner.store.put_object(&resolution)?;
            let mut entry = inner.submitted_entry(change)?;
            let report = entry.report.clone();
            let escalation = entry.escalation.get_or_insert_with(Escalation::default);
            if escalation.summary.is_none()
                && let Some(report) = &report
            {
                escalation.summary = inner.conflict_summary(change, report).ok();
            }
            escalation.resolution = Some(PendingResolution {
                change: id,
                by: arbiter.actor,
                signature: arbiter.signature,
            });
            escalation.note = None;
            escalation.whole_file = whole_file;
            inner.rewrite(&mut entry)?;
            inner.submit_as(id, Some(Origin::Arbitration { of: change }))?;
            Ok((id, entry))
        })
        .await
    }
}

/// The conflict summary's text, for a resolution's intent body.
fn summary_body(entry: &QueueEntry) -> String {
    entry
        .escalation
        .as_ref()
        .and_then(|e| e.summary.as_ref())
        .map(|s| s.text.clone())
        .unwrap_or_default()
}
