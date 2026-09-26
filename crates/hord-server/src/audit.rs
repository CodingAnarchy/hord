//! `hord audit` (spec §12 M6): M6's measurable acceptance criteria over a
//! window of the log.
//!
//! [`gather`] reads what a window recorded from a repository: the lander's
//! `Landed` and `Arbitrated` events, the log, each landed change's records
//! and evidence, head's policy at its landing base, and the bridge's
//! divergence checks. [`judge`] turns those facts into an
//! [`proto::AuditReport`] and does no I/O, so it is tested on synthetic
//! facts. [`LocalAudit`] serves both as the `Audit` gRPC service's backend.
//!
//! The criteria:
//!
//! - every landed change has an intent, provenance (an actor, and a record
//!   that was signed by its author with a bound key, vouched for by the git
//!   bridge, replayed by the lander's harness, or resolved by an arbiter),
//!   passing evidence, no failing evidence for its result, and head's
//!   policy at its landing base allows it when judged again;
//! - every review on those changes is signed by a key bound to the actor
//!   who produced it, and every arbitration in the window carries the
//!   arbiter's signature by a key bound to them (a store edit has neither);
//! - every change in the log that was created in the window has a `Landed`
//!   event (a change appended to the log by editing the store has none);
//! - the git bridge's divergence checks (ADR 0036) all passed and were no
//!   further apart than an hour, with slack.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use hord_api::proto::audit_server::Audit;
use hord_api::proto::{AuditCriterion, AuditOrigin};
use hord_api::{ApiError, ApiResult, AuditBackend, proto, wire};
use hord_core::sign::{self, SignError};
use hord_core::{
    Actor, ChangeId, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, ObjectId, Signature,
};
use hord_policy::{Decision, EvidenceTag};
use hord_txn::{LocalRepo, Origin, QueueEntry, QueueStatus, Repo};
use tonic::{Request, Response, Status};

use crate::auth::{AuthStore, same_actor};
use crate::hosts::Hosts;
use crate::route::RepoName;

/// Bridge checks are hourly (ADR 0036); this allows five minutes of slack.
pub const DEFAULT_MAX_BRIDGE_GAP_MS: u64 = 65 * 60 * 1000;

/// Events read per batch from the event log.
const EVENT_BATCH: usize = 1024;

/// Whether a signature is by a key bound to the actor it claims.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyCheck {
    /// Signed, the signature verifies, and the key is bound to the actor.
    Bound,
    /// Signed and the signature verifies; the repository has no auth file,
    /// so the binding was not checked.
    Unchecked,
    /// No signature.
    Unsigned,
    /// A signature that does not verify, or a key bound to someone else or
    /// to no one: the reason.
    Bad(String),
}

/// Head's policy at a landed change's base, judged again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyJudgement {
    /// It allows the change; where the policy came from (`head`/`default`).
    Allow(String),
    /// It denies the change: where the policy came from, and each unmet
    /// requirement in words.
    Deny(String, Vec<String>),
    /// The policy file does not parse.
    Unreadable(String),
}

/// One piece of evidence counted for a landed change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceFacts {
    /// Its ObjectId.
    pub id: String,
    /// Its tag (`test`, `review:human`, …).
    pub tag: String,
    /// Whether it is a review.
    pub review: bool,
    /// Whether it is the lander's rebase attestation (not a check).
    pub attestation: bool,
    /// Its result: `None` for a pass, else the failure or skip reason.
    pub failure: Option<String>,
    /// Whether it is for the landed result (a failure elsewhere, such as
    /// the submitted snapshot before a rebase, does not count against it).
    pub for_result: bool,
    /// Its producer.
    pub produced_by: Actor,
    /// Its signature, checked against its producer.
    pub key: KeyCheck,
}

/// A change that landed in the window, as [`gather`] found it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LandedFacts {
    /// The landed ChangeId.
    pub change: String,
    /// Its position in the log.
    pub position: u64,
    /// When the lander recorded it landing.
    pub landed_at_ms: u64,
    /// Its intent's summary.
    pub summary: String,
    /// Its author.
    pub actor: Actor,
    /// How it reached the lander.
    pub origin: AuditOrigin,
    /// For [`AuditOrigin::Signed`] or an unsigned record: the submitted
    /// record's signature, checked against its author.
    pub key: KeyCheck,
    /// Its evidence.
    pub evidence: Vec<EvidenceFacts>,
    /// Head's policy, judged again.
    pub policy: PolicyJudgement,
}

/// An arbitration recorded in the window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbitrationFacts {
    /// The parked change.
    pub change: String,
    /// The arbiter.
    pub by: Option<Actor>,
    /// The decision's signature, checked against the arbiter. (The server
    /// verified the signature itself at ingest; this checks the binding.)
    pub key: KeyCheck,
}

/// One divergence check of the git bridge (ADR 0036).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeCheck {
    /// When it ran.
    pub at_ms: u64,
    /// Whether it found the mirror diverged.
    pub diverged: bool,
    /// Its detail, in words.
    pub detail: String,
}

/// Everything [`judge`] needs about a window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditFacts {
    /// Start of the window (inclusive).
    pub since_ms: u64,
    /// End of the window (exclusive).
    pub until_ms: u64,
    /// The longest allowed time without a bridge check.
    pub max_bridge_gap_ms: u64,
    /// Whether a window without bridge checks fails.
    pub require_bridge: bool,
    /// Whether keys were checked against an auth file.
    pub bindings_checked: bool,
    /// Changes that landed in the window, in log order.
    pub changes: Vec<LandedFacts>,
    /// Arbitrations in the window.
    pub arbitrations: Vec<ArbitrationFacts>,
    /// Changes created in the window that are in the log without a
    /// `Landed` event: (change, position).
    pub unrecorded: Vec<(String, u64)>,
    /// Bridge checks in the window, in time order.
    pub bridge_checks: Vec<BridgeCheck>,
}

fn finding(criterion: AuditCriterion, change: Option<&str>, detail: String) -> proto::AuditFinding {
    proto::AuditFinding {
        criterion: criterion as i32,
        change: change.map(str::to_owned),
        detail,
    }
}

/// Judge a window's facts: the report, with every violation.
#[must_use]
pub fn judge(facts: &AuditFacts) -> proto::AuditReport {
    let mut violations = Vec::new();
    let mut notes = Vec::new();
    let mut changes = Vec::new();
    let mut reviews = 0;
    for landed in &facts.changes {
        let before = violations.len();
        judge_change(landed, &mut violations);
        reviews += landed.evidence.iter().filter(|e| e.review).count();
        let policy_source = match &landed.policy {
            PolicyJudgement::Allow(source) | PolicyJudgement::Deny(source, _) => source.clone(),
            PolicyJudgement::Unreadable(_) => String::new(),
        };
        changes.push(proto::AuditedChange {
            change: landed.change.clone(),
            position: landed.position,
            landed_at_ms: landed.landed_at_ms,
            summary: landed.summary.clone(),
            actor: Some(wire::actor(&landed.actor)),
            origin: landed.origin as i32,
            passing_evidence: u32::try_from(
                landed
                    .evidence
                    .iter()
                    .filter(|e| e.failure.is_none() && !e.attestation)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            policy_source,
            ok: violations.len() == before,
        });
    }
    for arbitration in &facts.arbitrations {
        if let Some(problem) = key_problem(&arbitration.key, "arbitration") {
            violations.push(finding(
                AuditCriterion::Arbitration,
                Some(&arbitration.change),
                problem,
            ));
        }
    }
    for (change, position) in &facts.unrecorded {
        violations.push(finding(
            AuditCriterion::UnrecordedLanding,
            Some(change),
            format!(
                "in the log at position {position} with no Landed event: \
                 it did not come through the lander"
            ),
        ));
    }
    let bridge = judge_bridge(facts, &mut violations, &mut notes);
    if !facts.bindings_checked {
        notes.push(finding(
            AuditCriterion::Unspecified,
            None,
            "no auth file: signatures were verified, but not that their keys are bound \
             to their actors"
                .into(),
        ));
    }
    proto::AuditReport {
        since_ms: facts.since_ms,
        until_ms: facts.until_ms,
        changes,
        reviews: u32::try_from(reviews).unwrap_or(u32::MAX),
        arbitrations: u32::try_from(facts.arbitrations.len()).unwrap_or(u32::MAX),
        bridge: Some(bridge),
        bindings_checked: facts.bindings_checked,
        ok: violations.is_empty(),
        violations,
        notes,
    }
}

/// What is wrong with a signature, for `what`; `None` when nothing is.
fn key_problem(key: &KeyCheck, what: &str) -> Option<String> {
    match key {
        KeyCheck::Bound | KeyCheck::Unchecked => None,
        KeyCheck::Unsigned => Some(format!("unsigned {what}: it did not come through the API")),
        KeyCheck::Bad(reason) => Some(format!("{what}: {reason}")),
    }
}

fn judge_change(landed: &LandedFacts, violations: &mut Vec<proto::AuditFinding>) {
    let change = Some(landed.change.as_str());
    if landed.summary.trim().is_empty() {
        violations.push(finding(
            AuditCriterion::Intent,
            change,
            "no intent summary".into(),
        ));
    }
    let actor_ok = match &landed.actor {
        Actor::Human { id } => !id.is_empty(),
        Actor::Agent {
            id, model, harness, ..
        } => !id.is_empty() && !model.is_empty() && !harness.is_empty(),
    };
    if !actor_ok {
        violations.push(finding(
            AuditCriterion::Provenance,
            change,
            "the actor is incomplete (an agent needs an id, a model and a harness)".into(),
        ));
    }
    match landed.origin {
        AuditOrigin::Signed | AuditOrigin::SignedUnbound => {
            if let Some(problem) = key_problem(&landed.key, "submitted record") {
                violations.push(finding(AuditCriterion::Provenance, change, problem));
            }
        }
        AuditOrigin::Unsigned | AuditOrigin::Unspecified => violations.push(finding(
            AuditCriterion::Provenance,
            change,
            key_problem(&landed.key, "submitted record").unwrap_or_else(|| {
                "the submitted record is unsigned and no bridge vouched for it".into()
            }),
        )),
        AuditOrigin::BridgeVouched | AuditOrigin::Replay | AuditOrigin::Arbitration => {}
    }
    let checks: Vec<&EvidenceFacts> = landed.evidence.iter().filter(|e| !e.attestation).collect();
    if !checks.iter().any(|e| e.failure.is_none()) {
        violations.push(finding(
            AuditCriterion::Evidence,
            change,
            "no passing evidence".into(),
        ));
    }
    for failed in checks.iter().filter(|e| e.for_result) {
        if let Some(reason) = &failed.failure {
            violations.push(finding(
                AuditCriterion::Evidence,
                change,
                format!("{} {} did not pass: {reason}", failed.tag, failed.id),
            ));
        }
    }
    for review in checks.iter().filter(|e| e.review) {
        if let Some(problem) = key_problem(&review.key, &format!("review {}", review.id)) {
            violations.push(finding(AuditCriterion::Review, change, problem));
        }
    }
    match &landed.policy {
        PolicyJudgement::Allow(_) => {}
        PolicyJudgement::Deny(source, reasons) => violations.push(finding(
            AuditCriterion::Policy,
            change,
            format!(
                "the {source} policy at its landing base does not allow it: {}",
                reasons.join("; ")
            ),
        )),
        PolicyJudgement::Unreadable(reason) => violations.push(finding(
            AuditCriterion::Policy,
            change,
            format!("the policy at its landing base does not parse: {reason}"),
        )),
    }
}

fn judge_bridge(
    facts: &AuditFacts,
    violations: &mut Vec<proto::AuditFinding>,
    notes: &mut Vec<proto::AuditFinding>,
) -> proto::AuditBridge {
    let checks = &facts.bridge_checks;
    let max = if facts.max_bridge_gap_ms == 0 {
        DEFAULT_MAX_BRIDGE_GAP_MS
    } else {
        facts.max_bridge_gap_ms
    };
    let mut bridge = proto::AuditBridge {
        checks: u32::try_from(checks.len()).unwrap_or(u32::MAX),
        diverged: 0,
        first_at_ms: checks.first().map(|c| c.at_ms),
        last_at_ms: checks.last().map(|c| c.at_ms),
        longest_gap_ms: 0,
    };
    if checks.is_empty() {
        let note = finding(
            AuditCriterion::BridgeGap,
            None,
            "no bridge checks recorded".into(),
        );
        if facts.require_bridge {
            violations.push(note);
        } else {
            notes.push(note);
        }
        bridge.longest_gap_ms = facts.until_ms.saturating_sub(facts.since_ms);
        return bridge;
    }
    for check in checks.iter().filter(|c| c.diverged) {
        bridge.diverged += 1;
        violations.push(finding(
            AuditCriterion::BridgeDiverged,
            None,
            format!("at {} ms: {}", check.at_ms, check.detail),
        ));
    }
    let mut points = vec![facts.since_ms];
    points.extend(checks.iter().map(|c| c.at_ms));
    points.push(facts.until_ms);
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let gap = to.saturating_sub(from);
        bridge.longest_gap_ms = bridge.longest_gap_ms.max(gap);
        if gap > max {
            violations.push(finding(
                AuditCriterion::BridgeGap,
                None,
                format!(
                    "no bridge check from {from} ms to {to} ms ({} min, more than {} min)",
                    gap / 60_000,
                    max / 60_000
                ),
            ));
        }
    }
    bridge
}

/// The divergence check an event records, if it is one (ADR 0036). The
/// bridge's `BridgeChecked` event kind is defined by the bridge's slice of
/// `hord.proto`; until it is part of this schema, no event is one, and the
/// report says "no bridge checks recorded".
fn bridge_check(envelope: &proto::EventEnvelope) -> Option<BridgeCheck> {
    let _ = envelope;
    None
}

/// Who vouched for an unsigned submitted change, if the git bridge did
/// (ADR 0037): the key id on its `Submitted` event. Like
/// [`bridge_check`], it reads the bridge's field once the schema has it.
fn bridge_voucher(submitted: &proto::Submitted) -> Option<String> {
    let _ = submitted;
    None
}

/// Check `signature` over an object claimed by `actor`: `verify` checks it
/// cryptographically with the key it names; `auth`, when present, says
/// whom the key is bound to.
fn check_key(
    auth: Option<&AuthStore>,
    actor: &Actor,
    signature: Option<&Signature>,
    verify: impl FnOnce(&sign::PublicKey) -> Result<(), SignError>,
) -> KeyCheck {
    let Some(signature) = signature else {
        return KeyCheck::Unsigned;
    };
    let verified = sign::signer(signature).and_then(|key| verify(&key));
    if let Err(err) = verified {
        return KeyCheck::Bad(format!("bad signature by {}: {err}", signature.key_id));
    }
    bound(auth, actor, &signature.key_id)
}

/// Whether `key_id` is bound to `actor` in `auth`.
fn bound(auth: Option<&AuthStore>, actor: &Actor, key_id: &str) -> KeyCheck {
    let Some(auth) = auth else {
        return KeyCheck::Unchecked;
    };
    match auth.key_actor(key_id) {
        Ok(Some(owner)) if same_actor(actor, &owner) => KeyCheck::Bound,
        Ok(Some(owner)) => KeyCheck::Bad(format!(
            "signed by key {key_id}, which is bound to {}, not {}",
            owner.id(),
            actor.id()
        )),
        Ok(None) => KeyCheck::Bad(format!(
            "signed by key {key_id}, which is not bound to any actor"
        )),
        Err(err) => KeyCheck::Bad(format!("look up key {key_id}: {err}")),
    }
}

fn internal(err: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(err.to_string())
}

/// Every recorded event, in cursor order.
async fn all_events(repo: &Repo) -> ApiResult<Vec<proto::EventEnvelope>> {
    let mut out = Vec::new();
    let mut after = 0;
    loop {
        let batch = repo
            .recorded_events(after, EVENT_BATCH)
            .await
            .map_err(internal)?;
        let Some(last) = batch.last() else {
            return Ok(out);
        };
        after = last.cursor;
        out.extend(batch);
    }
}

fn change_id(text: &str) -> ApiResult<ChangeId> {
    wire::object_id("change", text)
}

/// Read a window's facts from `repo`, checking keys against `auth` when
/// given. `now_ms` ends a window without `until_ms`.
pub async fn gather(
    repo: &Repo,
    auth: Option<&AuthStore>,
    request: &proto::AuditRequest,
    now_ms: u64,
) -> ApiResult<AuditFacts> {
    let since_ms = request.since_ms;
    let until_ms = request.until_ms.unwrap_or(now_ms);
    if until_ms < since_ms {
        return Err(ApiError::InvalidArgument(format!(
            "the window ends ({until_ms} ms) before it starts ({since_ms} ms)"
        )));
    }
    let within = |at: u64| since_ms <= at && at < until_ms;
    let events = all_events(repo).await?;
    let mut landed_ever = BTreeSet::new();
    let mut landed = Vec::new();
    let mut submitted = BTreeMap::new();
    let mut arbitrations = Vec::new();
    let mut bridge_checks = Vec::new();
    for envelope in &events {
        if let Some(check) = bridge_check(envelope)
            && within(envelope.at_ms)
        {
            bridge_checks.push(check);
        }
        let Some(kind) = envelope.event.as_ref().and_then(|e| e.kind.as_ref()) else {
            continue;
        };
        match kind {
            proto::event::Kind::Landed(event) => {
                landed_ever.insert(event.change.clone());
                if within(envelope.at_ms) {
                    landed.push((envelope.at_ms, event.clone()));
                }
            }
            proto::event::Kind::Submitted(event) => {
                submitted.insert(event.change.clone(), event.clone());
            }
            proto::event::Kind::Arbitrated(event) if within(envelope.at_ms) => {
                let by = event
                    .by
                    .as_ref()
                    .map(|a| wire::actor_from("by", a))
                    .transpose()?;
                let key = match (&by, &event.key_id, &event.signature) {
                    (Some(by), Some(key_id), Some(_)) => bound(auth, by, key_id),
                    (None, _, _) => KeyCheck::Bad("no arbiter recorded".into()),
                    _ => KeyCheck::Unsigned,
                };
                arbitrations.push(ArbitrationFacts {
                    change: event.change.clone(),
                    by,
                    key,
                });
            }
            _ => {}
        }
    }
    bridge_checks.sort_by_key(|c| c.at_ms);
    landed.sort_by_key(|(_, event)| event.position);

    let queue: BTreeMap<ChangeId, QueueEntry> = repo
        .queue()
        .await
        .map_err(internal)?
        .into_iter()
        .filter_map(|entry| match entry.status {
            QueueStatus::Landed { landed } => Some((landed, entry)),
            _ => None,
        })
        .collect();

    let mut changes = Vec::new();
    for (at_ms, event) in landed {
        let id = change_id(&event.change)?;
        let entry = queue.get(&id);
        let record = repo.change(id).await.map_err(internal)?;
        let submitted_id = entry.map_or(record.rebased_from.unwrap_or(id), |e| e.change);
        let submitted_record = if submitted_id == id {
            record.clone()
        } else {
            repo.change(submitted_id).await.map_err(internal)?
        };
        let actor = record.provenance.actor.clone();
        let key = check_key(
            auth,
            &submitted_record.provenance.actor,
            submitted_record.signature.as_ref(),
            |key| sign::verify_change(&submitted_record, key),
        );
        let voucher = submitted
            .get(&wire::id(submitted_id))
            .and_then(bridge_voucher);
        let origin = match (entry.and_then(|e| e.origin.as_ref()), &key, voucher) {
            (Some(Origin::Replay { .. }), _, _) => AuditOrigin::Replay,
            (Some(Origin::Arbitration { .. }), _, _) => AuditOrigin::Arbitration,
            (None, KeyCheck::Bound, _) => AuditOrigin::Signed,
            (None, KeyCheck::Unchecked, _) => AuditOrigin::SignedUnbound,
            (None, KeyCheck::Unsigned, Some(_)) => AuditOrigin::BridgeVouched,
            (None, KeyCheck::Bad(_), _) => AuditOrigin::Signed,
            (None, KeyCheck::Unsigned, None) => AuditOrigin::Unsigned,
        };
        let evidence = landed_evidence(repo, auth, &record, &submitted_record).await?;
        let policy = match repo
            .judge_landed(id, entry.and_then(|e| e.report.clone()))
            .await
            .map_err(internal)?
        {
            Ok((Decision::Allow, source)) => PolicyJudgement::Allow(source.as_str().into()),
            Ok((Decision::Deny { reasons }, source)) => PolicyJudgement::Deny(
                source.as_str().into(),
                reasons
                    .iter()
                    .map(|r| match &r.rule {
                        Some(rule) => format!("{} ({rule})", r.requirement),
                        None => r.requirement.to_string(),
                    })
                    .collect(),
            ),
            Err(reason) => PolicyJudgement::Unreadable(reason),
        };
        changes.push(LandedFacts {
            change: event.change.clone(),
            position: event.position,
            landed_at_ms: at_ms,
            summary: record.intent.summary.clone(),
            actor,
            origin,
            key,
            evidence,
            policy,
        });
    }

    let unrecorded = unrecorded(repo, &landed_ever, since_ms, until_ms).await?;
    Ok(AuditFacts {
        since_ms,
        until_ms,
        max_bridge_gap_ms: request.max_bridge_gap_ms,
        require_bridge: request.require_bridge,
        bindings_checked: auth.is_some(),
        changes,
        arbitrations,
        unrecorded,
        bridge_checks,
    })
}

/// The evidence the lander could count for `record` (landed as rebased
/// from `submitted`, or submitted as is): the author's, what is indexed for
/// its result, and the reviews indexed for the submitted result
/// (ADR 0031).
async fn landed_evidence(
    repo: &Repo,
    auth: Option<&AuthStore>,
    record: &ChangeRecord,
    submitted: &ChangeRecord,
) -> ApiResult<Vec<EvidenceFacts>> {
    let store_repo = repo.clone();
    let (landed_result, submitted_result) = (record.result, submitted.result);
    let mut ids: Vec<(ObjectId, bool)> = Vec::new();
    for id in record.evidence.iter().chain(&submitted.evidence) {
        ids.push((*id, false));
    }
    let loaded = tokio::task::spawn_blocking(move || {
        let store = store_repo.store();
        let mut all = ids;
        all.extend(
            store
                .evidence_at(landed_result)
                .map_err(internal)?
                .into_iter()
                .map(|id| (id, false)),
        );
        if submitted_result != landed_result {
            // Only reviews carry across a rebase.
            all.extend(
                store
                    .evidence_at(submitted_result)
                    .map_err(internal)?
                    .into_iter()
                    .map(|id| (id, true)),
            );
        }
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for (id, reviews_only) in all {
            if !seen.insert(id) {
                continue;
            }
            let evidence: Evidence = store.get_object(id).map_err(internal)?;
            if reviews_only && evidence.kind != EvidenceKind::Review {
                continue;
            }
            out.push((id, evidence));
        }
        Ok::<_, ApiError>(out)
    })
    .await
    .map_err(internal)??;
    Ok(loaded
        .into_iter()
        .map(|(id, evidence)| {
            let review = evidence.kind == EvidenceKind::Review;
            let key = check_key(
                auth,
                &evidence.produced_by,
                evidence.signature.as_ref(),
                |key| sign::verify_evidence(&evidence, key),
            );
            EvidenceFacts {
                id: wire::id(id),
                tag: EvidenceTag::of(&evidence).map_or_else(
                    || format!("{:?}", evidence.kind).to_lowercase(),
                    |tag| tag.to_string(),
                ),
                review,
                attestation: matches!(evidence.kind, EvidenceKind::Rebase { .. }),
                failure: match &evidence.result {
                    EvidenceResult::Pass => None,
                    EvidenceResult::Fail { summary } => Some(format!("failed: {summary}")),
                    EvidenceResult::Skipped { reason } => Some(format!("skipped: {reason}")),
                },
                for_result: evidence.snapshot == landed_result,
                produced_by: evidence.produced_by,
                key,
            }
        })
        .collect())
}

/// Changes in the log created in the window that have no `Landed` event.
async fn unrecorded(
    repo: &Repo,
    landed_ever: &BTreeSet<String>,
    since_ms: u64,
    until_ms: u64,
) -> ApiResult<Vec<(String, u64)>> {
    let log_repo = repo.clone();
    let log = tokio::task::spawn_blocking(move || log_repo.store().log())
        .await
        .map_err(internal)?
        .map_err(internal)?;
    let mut out = Vec::new();
    for (position, id) in log.into_iter().enumerate() {
        let text = wire::id(id);
        if landed_ever.contains(&text) {
            continue;
        }
        let created = repo
            .change(id)
            .await
            .map_err(internal)?
            .provenance
            .created_at
            .as_millis();
        if since_ms <= created && created < until_ms {
            out.push((text, u64::try_from(position).unwrap_or(u64::MAX)));
        }
    }
    Ok(out)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// [`AuditBackend`] over a local repository, with the server's key
/// bindings when it has an auth file.
#[derive(Clone, Debug)]
pub struct LocalAudit {
    local: Arc<LocalRepo>,
    auth: Option<Arc<AuthStore>>,
}

impl LocalAudit {
    /// Audit `local`, checking keys against `auth` when given.
    #[must_use]
    pub fn new(local: Arc<LocalRepo>, auth: Option<Arc<AuthStore>>) -> Self {
        Self { local, auth }
    }
}

#[async_trait]
impl AuditBackend for LocalAudit {
    async fn audit_log(&self, request: proto::AuditRequest) -> ApiResult<proto::AuditReport> {
        let facts = gather(self.local.repo(), self.auth.as_deref(), &request, now_ms()).await?;
        Ok(judge(&facts))
    }
}

/// The `Audit` gRPC service over the hosted repositories.
#[derive(Clone, Debug)]
pub(crate) struct GrpcAudit {
    hosts: Arc<Hosts>,
    auth: Option<Arc<AuthStore>>,
}

impl GrpcAudit {
    pub(crate) fn new(hosts: Arc<Hosts>, auth: Option<Arc<AuthStore>>) -> Self {
        Self { hosts, auth }
    }
}

#[tonic::async_trait]
impl Audit for GrpcAudit {
    async fn audit_log(
        &self,
        request: Request<proto::AuditRequest>,
    ) -> Result<Response<proto::AuditReport>, Status> {
        let local = self
            .hosts
            .resolve_local(request.extensions().get::<RepoName>())?;
        let audit = LocalAudit::new(local, self.auth.clone());
        audit
            .audit_log(request.into_inner())
            .await
            .map(Response::new)
            .map_err(Status::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60 * 1000;

    fn ada() -> Actor {
        Actor::Human { id: "ada".into() }
    }

    fn passing(tag: &str) -> EvidenceFacts {
        EvidenceFacts {
            id: format!("ev-{tag}"),
            tag: tag.into(),
            review: tag.starts_with("review"),
            attestation: false,
            failure: None,
            for_result: true,
            produced_by: ada(),
            key: KeyCheck::Bound,
        }
    }

    fn landed(n: u64) -> LandedFacts {
        LandedFacts {
            change: format!("change-{n}"),
            position: n,
            landed_at_ms: n * HOUR,
            summary: format!("change {n}"),
            actor: ada(),
            origin: AuditOrigin::Signed,
            key: KeyCheck::Bound,
            evidence: vec![passing("test"), passing("review:human")],
            policy: PolicyJudgement::Allow("head".into()),
        }
    }

    /// Five hours, a change landed each hour, a passing bridge check every
    /// hour, one signed arbitration.
    fn clean() -> AuditFacts {
        AuditFacts {
            since_ms: 0,
            until_ms: 5 * HOUR,
            max_bridge_gap_ms: 0,
            require_bridge: false,
            bindings_checked: true,
            changes: (0..5).map(landed).collect(),
            arbitrations: vec![ArbitrationFacts {
                change: "parked".into(),
                by: Some(ada()),
                key: KeyCheck::Bound,
            }],
            unrecorded: Vec::new(),
            bridge_checks: (0..=5)
                .map(|h| BridgeCheck {
                    at_ms: h * HOUR,
                    diverged: false,
                    detail: String::new(),
                })
                .collect(),
        }
    }

    fn criteria(report: &proto::AuditReport) -> Vec<(AuditCriterion, Option<String>)> {
        report
            .violations
            .iter()
            .map(|v| (v.criterion(), v.change.clone()))
            .collect()
    }

    #[test]
    fn a_clean_window_passes() {
        let report = judge(&clean());
        assert!(report.ok, "{:#?}", report.violations);
        assert_eq!(report.changes.len(), 5);
        assert!(
            report
                .changes
                .iter()
                .all(|c| c.ok && c.passing_evidence == 2)
        );
        assert_eq!(report.reviews, 5);
        assert_eq!(report.arbitrations, 1);
        let bridge = report.bridge.unwrap_or_default();
        assert_eq!((bridge.checks, bridge.diverged), (6, 0));
        assert_eq!(bridge.longest_gap_ms, HOUR);
        assert!(report.notes.is_empty(), "{:#?}", report.notes);
    }

    #[test]
    fn a_change_missing_evidence_fails() {
        let mut facts = clean();
        facts.changes[2].evidence.clear();
        // The lander's rebase attestation is not evidence of a check.
        facts.changes[3].evidence = vec![EvidenceFacts {
            attestation: true,
            ..passing("rebase")
        }];
        let report = judge(&facts);
        assert!(!report.ok);
        assert_eq!(
            criteria(&report),
            [
                (AuditCriterion::Evidence, Some("change-2".into())),
                (AuditCriterion::Evidence, Some("change-3".into())),
            ]
        );
        assert!(!report.changes[2].ok && report.changes[1].ok);
    }

    #[test]
    fn failing_evidence_for_the_result_fails_but_not_for_another_snapshot() {
        let mut facts = clean();
        facts.changes[0].evidence.push(EvidenceFacts {
            failure: Some("failed: 1 test".into()),
            ..passing("test")
        });
        facts.changes[1].evidence.push(EvidenceFacts {
            failure: Some("failed: before the rebase".into()),
            for_result: false,
            ..passing("test")
        });
        let report = judge(&facts);
        assert_eq!(
            criteria(&report),
            [(AuditCriterion::Evidence, Some("change-0".into()))]
        );
    }

    #[test]
    fn an_unsigned_review_fails() {
        let mut facts = clean();
        facts.changes[1].evidence[1].key = KeyCheck::Unsigned;
        facts.changes[4].evidence[1].key =
            KeyCheck::Bad("signed by key k, which is bound to eve, not ada".into());
        let report = judge(&facts);
        assert_eq!(
            criteria(&report),
            [
                (AuditCriterion::Review, Some("change-1".into())),
                (AuditCriterion::Review, Some("change-4".into())),
            ]
        );
        assert!(report.violations[0].detail.contains("unsigned"));
    }

    #[test]
    fn a_gap_in_the_bridge_checks_fails() {
        let mut facts = clean();
        // No check at hours 2 and 3: three hours without one.
        facts
            .bridge_checks
            .retain(|c| c.at_ms != 2 * HOUR && c.at_ms != 3 * HOUR);
        let report = judge(&facts);
        assert_eq!(criteria(&report), [(AuditCriterion::BridgeGap, None)]);
        assert_eq!(report.bridge.unwrap_or_default().longest_gap_ms, 3 * HOUR);
        // The window's ends count: a bridge that starts late leaves a gap.
        let mut late = clean();
        late.bridge_checks.retain(|c| c.at_ms >= 2 * HOUR);
        assert_eq!(criteria(&judge(&late)), [(AuditCriterion::BridgeGap, None)]);
    }

    #[test]
    fn a_diverged_check_fails() {
        let mut facts = clean();
        facts.bridge_checks[3].diverged = true;
        facts.bridge_checks[3].detail = "main is abc, the export is def".into();
        let report = judge(&facts);
        assert_eq!(criteria(&report), [(AuditCriterion::BridgeDiverged, None)]);
        assert_eq!(report.bridge.unwrap_or_default().diverged, 1);
    }

    #[test]
    fn no_bridge_checks_is_a_note_unless_required() {
        let mut facts = clean();
        facts.bridge_checks.clear();
        let report = judge(&facts);
        assert!(report.ok);
        assert_eq!(report.notes.len(), 1);
        assert_eq!(report.notes[0].detail, "no bridge checks recorded");
        facts.require_bridge = true;
        let report = judge(&facts);
        assert_eq!(criteria(&report), [(AuditCriterion::BridgeGap, None)]);
    }

    #[test]
    fn store_edits_and_unsigned_work_fail() {
        let mut facts = clean();
        facts.unrecorded.push(("edited".into(), 9));
        facts.arbitrations[0].key = KeyCheck::Unsigned;
        facts.changes[0].origin = AuditOrigin::Unsigned;
        facts.changes[0].key = KeyCheck::Unsigned;
        facts.changes[1].summary = " ".into();
        facts.changes[2].policy = PolicyJudgement::Deny("head".into(), vec!["review:human".into()]);
        let report = judge(&facts);
        assert_eq!(
            criteria(&report),
            [
                (AuditCriterion::Provenance, Some("change-0".into())),
                (AuditCriterion::Intent, Some("change-1".into())),
                (AuditCriterion::Policy, Some("change-2".into())),
                (AuditCriterion::Arbitration, Some("parked".into())),
                (AuditCriterion::UnrecordedLanding, Some("edited".into())),
            ]
        );
    }

    #[test]
    fn bridge_vouched_replayed_and_arbitrated_changes_need_no_signature() {
        let mut facts = clean();
        for (n, origin) in [
            AuditOrigin::BridgeVouched,
            AuditOrigin::Replay,
            AuditOrigin::Arbitration,
        ]
        .into_iter()
        .enumerate()
        {
            facts.changes[n].origin = origin;
            facts.changes[n].key = KeyCheck::Unsigned;
        }
        let report = judge(&facts);
        assert!(report.ok, "{:#?}", report.violations);
        assert_eq!(
            report.changes[0].origin(),
            AuditOrigin::BridgeVouched,
            "reported apart from signed changes"
        );
    }

    #[test]
    fn without_an_auth_file_bindings_are_a_note() {
        let mut facts = clean();
        facts.bindings_checked = false;
        for change in &mut facts.changes {
            change.origin = AuditOrigin::SignedUnbound;
            change.key = KeyCheck::Unchecked;
        }
        let report = judge(&facts);
        assert!(report.ok, "{:#?}", report.violations);
        assert!(!report.bindings_checked);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.detail.contains("no auth file"))
        );
    }
}
