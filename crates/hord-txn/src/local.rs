//! [`LocalRepo`]: [`hord_api::RepoBackend`] over a local `.hord/` store
//! (spec §10.5.2, ADR 0024), with its lander running as a task.

use std::sync::Arc;

use async_trait::async_trait;
use hord_api::{
    ApiError, ApiResult, DEFAULT_LOG_LIMIT, EventStream, MAX_BATCH_BYTES, MAX_BATCH_IDS,
    RepoBackend, proto, wire,
};
use hord_core::{Actor, Bytes, ChangeId, ChangeRecord, Evidence, NodeId, ObjectId, Signature};
use hord_store::EdgeKind;
use tokio_util::sync::CancellationToken;

use crate::conflict::{ConflictKind, ConflictReport, MergeSeverity};
use crate::escalation::{Arbiter, Arbitration, Escalation, ReplayOutcome};
use crate::lander::{Lander, QueueEntry, QueueStatus};
use crate::replay::summary_message;
use crate::repo::{Inner, Repo};
use crate::source::ObjectSource;
use crate::{Error, Result};

/// A [`Repo`] served through [`RepoBackend`], with its lander running as a
/// task ([`Lander::spawn`]) so submitted changes land without a
/// `land_local` call. This is what `hord serve` wraps (ADR 0024) and what
/// the conformance suite runs against locally.
///
/// Dropping it stops the lander.
pub struct LocalRepo {
    repo: Repo,
    cancel: CancellationToken,
    lander: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for LocalRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRepo")
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

impl LocalRepo {
    /// Serve `repo` and start its lander on the current tokio runtime.
    pub fn new(repo: Repo) -> Result<Self> {
        let cancel = CancellationToken::new();
        let (task, _events) = Lander::spawn(repo.clone(), cancel.clone())?;
        Ok(Self {
            repo,
            cancel,
            lander: std::sync::Mutex::new(Some(task)),
        })
    }

    /// Serve `repo` without a lander task: submitted changes wait for
    /// [`Repo::land_local`] or a lander elsewhere. For one-shot clients such
    /// as `hord --no-daemon`, which must not land as a side effect.
    #[must_use]
    pub fn without_lander(repo: Repo) -> Self {
        Self {
            repo,
            cancel: CancellationToken::new(),
            lander: std::sync::Mutex::new(None),
        }
    }

    /// The repository this serves.
    #[must_use]
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Stop the lander and wait for it to finish its current step, then
    /// close the repository ([`Repo::close`]): afterwards no task it spawned
    /// holds it.
    pub async fn shutdown(&self) {
        // Stop replays and verification first: the lander may be waiting
        // for either, and must not wait them out.
        self.repo.inner.stop_background();
        self.cancel.cancel();
        let task = crate::repo::lock(&self.lander).take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.repo.close().await;
    }

    /// End every event stream this serves (see [`Repo::close_events`]),
    /// so a server's connections can drain before [`Self::shutdown`].
    pub async fn close_events(&self) {
        self.repo.close_events().await;
    }
}

impl Drop for LocalRepo {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl ObjectSource for LocalRepo {
    fn get_objects(&self, ids: &[ObjectId]) -> Result<Vec<Vec<u8>>> {
        self.repo.get_objects(ids)
    }

    fn has(&self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        ObjectSource::has(&self.repo, ids)
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        ObjectSource::get(&self.repo, id)
    }
}

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        let message = err.to_string();
        match err {
            Error::MissingChange(_)
            | Error::NotQueued(_)
            | Error::UnknownWorkspace(_)
            | Error::UnknownName(_)
            | Error::MissingFile(_)
            | Error::MissingIdentity(_)
            | Error::Store(hord_store::Error::MissingObject(_)) => Self::NotFound(message),
            Error::InvalidPath(_) | Error::Encoding(_) | Error::Corrupt { .. } => {
                Self::InvalidArgument(message)
            }
            Error::AmbiguousName { .. }
            | Error::NotEmpty(_)
            | Error::NothingToPropose
            | Error::NotArbitrable { .. }
            | Error::NoHarness => Self::FailedPrecondition(message),
            Error::BadSignature(_) => Self::InvalidArgument(message),
            _ => Self::Internal(message),
        }
    }
}

/// Run `f` on the blocking pool with API errors.
async fn api<T, F>(repo: &Repo, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> ApiResult<T> + Send + 'static,
{
    let inner = Arc::clone(&repo.inner);
    repo.inner
        .tasks
        .spawn_blocking(move || f(&inner))
        .await
        .map_err(|err| ApiError::Internal(format!("background task failed: {err}")))?
}

fn check_batch(field: &str, n: usize) -> ApiResult<()> {
    if n > MAX_BATCH_IDS {
        return Err(ApiError::ResourceExhausted(format!(
            "{field}: {n} entries; at most {MAX_BATCH_IDS} per call"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------- views

/// Wire form of a landed change at `position`.
fn summary(
    position: Option<usize>,
    change: ChangeId,
    record: &ChangeRecord,
) -> proto::ChangeSummary {
    proto::ChangeSummary {
        change: wire::id(change),
        position: position.map_or(0, |p| p as u64),
        base: wire::id(record.base),
        result: wire::id(record.result),
        parents: record.parents.iter().copied().map(wire::id).collect(),
        summary: record.intent.summary.clone(),
        actor: Some(wire::actor(&record.provenance.actor)),
        created_at_ms: record.provenance.created_at.as_millis(),
        rebased_from: record.rebased_from.map(wire::id),
    }
}

/// Wire form of a [`ConflictReport`] (node names are not resolved).
#[must_use]
pub fn conflict_report_message(report: &ConflictReport) -> proto::ConflictReport {
    proto::ConflictReport {
        change: wire::id(report.change),
        base: wire::id(report.base),
        head: report.head.map(wire::id),
        checked_against: report
            .checked_against
            .iter()
            .copied()
            .map(wire::id)
            .collect(),
        strict_reads: report.strict_reads,
        clean: report.is_clean(),
        conflicts: report
            .conflicts
            .iter()
            .map(|c| proto::SetConflict {
                kind: match c.kind {
                    ConflictKind::WriteWrite => proto::ConflictKind::WriteWrite,
                    ConflictKind::ReadWrite => proto::ConflictKind::ReadWrite,
                    ConflictKind::WriteRead => proto::ConflictKind::WriteRead,
                }
                .into(),
                landed: wire::id(c.landed),
                nodes: c.nodes.iter().copied().map(wire::node_ref).collect(),
                paths: c.paths.iter().map(ToString::to_string).collect(),
                landed_summary: String::new(),
            })
            .collect(),
        merge: report
            .merge
            .iter()
            .map(|m| proto::MergeConflict {
                path: m.path.to_string(),
                severity: match m.severity {
                    MergeSeverity::Hard => proto::MergeSeverity::Hard,
                    MergeSeverity::Soft => proto::MergeSeverity::Soft,
                }
                .into(),
                nodes: m.nodes.iter().copied().map(wire::node_ref).collect(),
                reason: m.reason.clone(),
            })
            .collect(),
        verification: report.verification.clone(),
        adapter_merged: report
            .adapter_merged
            .iter()
            .map(|m| proto::AdapterMerge {
                path: m.path.to_string(),
                nodes: m.nodes.iter().map(ToString::to_string).collect(),
            })
            .collect(),
        policy: report.policy.iter().map(violation_message).collect(),
    }
}

/// Wire form of a policy violation (ADR 0026).
fn violation_message(v: &hord_policy::Violation) -> proto::PolicyViolation {
    use hord_policy::{EvidenceState, Trigger, ViolationSource};
    proto::PolicyViolation {
        source: match v.source {
            ViolationSource::Land => "land",
            ViolationSource::MaxWriteSet => "max_write_set",
            ViolationSource::Rule => "rule",
        }
        .into(),
        rule: v.rule.clone(),
        requirement: v.requirement.to_string(),
        evidence: match v.evidence {
            EvidenceState::Absent => "absent",
            EvidenceState::Failed => "failed",
            EvidenceState::Skipped => "skipped",
        }
        .into(),
        triggers: v
            .triggers
            .iter()
            .map(|t| match t {
                Trigger::Definition { node, path } => format!("definition {node} {path}"),
                Trigger::Path { path } => format!("path {path}"),
                Trigger::Actor { actor } => format!("actor {}", actor.as_str()),
                Trigger::WriteSet { size, limit } => format!("write_set {size} > {limit}"),
            })
            .collect(),
    }
}

/// Wire form of a [`QueueEntry`]; `record` supplies the summary and actor.
#[must_use]
pub fn queue_entry_message(entry: &QueueEntry, record: Option<&ChangeRecord>) -> proto::QueueEntry {
    let (status, landed, reason) = match &entry.status {
        QueueStatus::Queued => (proto::QueueStatus::Queued, None, None),
        QueueStatus::Landed { landed } => {
            (proto::QueueStatus::Landed, Some(wire::id(*landed)), None)
        }
        QueueStatus::Conflicted => (proto::QueueStatus::Conflicted, None, None),
        QueueStatus::Rejected { reason } => {
            (proto::QueueStatus::Rejected, None, Some(reason.clone()))
        }
        QueueStatus::Parked { reason } => (proto::QueueStatus::Parked, None, Some(reason.clone())),
        QueueStatus::Replaying { .. } => (proto::QueueStatus::Replaying, None, None),
        QueueStatus::NeedsArbitration => (
            proto::QueueStatus::NeedsArbitration,
            None,
            entry.escalation.as_ref().and_then(|e| e.note.clone()),
        ),
        QueueStatus::Replayed { landed } => {
            (proto::QueueStatus::Replayed, Some(wire::id(*landed)), None)
        }
        QueueStatus::Arbitrated { landed } => (
            proto::QueueStatus::Arbitrated,
            Some(wire::id(*landed)),
            None,
        ),
    };
    let conflicts = entry
        .report
        .as_ref()
        .map_or(0, |r| r.conflicts.len() + r.merge.len());
    proto::QueueEntry {
        seq: entry.seq,
        change: wire::id(entry.change),
        status: status.into(),
        landed,
        reason,
        summary: record.map(|r| r.intent.summary.clone()).unwrap_or_default(),
        actor: record.map(|r| wire::actor(&r.provenance.actor)),
        submitted_at_ms: entry.submitted_at.as_millis(),
        updated_at_ms: entry.updated_at.as_millis(),
        conflicts: u32::try_from(conflicts).unwrap_or(u32::MAX),
        hard: entry.report.as_ref().is_some_and(ConflictReport::has_hard),
        report: entry.report.as_ref().map(conflict_report_message),
        escalation: entry.escalation.as_ref().map(escalation_message),
    }
}

/// The parked change, the decision, and the arbiter an Arbitrate request
/// names. An unset arbiter is `anonymous`; `key_id` and `signature` come
/// together or not at all.
pub fn arbitrate_request(
    request: &proto::ArbitrateRequest,
) -> ApiResult<(ChangeId, Arbitration, Arbiter)> {
    use proto::arbitration::Action;
    let change = wire::object_id("change", &request.change)?;
    let action = match request.action.as_ref().and_then(|a| a.action.as_ref()) {
        Some(Action::PickOurs(true)) => Arbitration::PickOurs,
        Some(Action::PickTheirs(true)) => Arbitration::PickTheirs,
        Some(Action::Replay(true)) => Arbitration::Replay {
            note: request.note.clone(),
        },
        Some(Action::Resolved(id)) => {
            Arbitration::Resolved(wire::object_id("action.resolved", id)?)
        }
        Some(Action::PickOurs(false) | Action::PickTheirs(false) | Action::Replay(false))
        | None => {
            return Err(ApiError::InvalidArgument(
                "action: pick_ours, pick_theirs, replay, or resolved".into(),
            ));
        }
    };
    let actor = match &request.arbiter {
        Some(actor) => wire::actor_from("arbiter", actor)?,
        None => Actor::Human {
            id: "anonymous".into(),
        },
    };
    let signature = match (&request.key_id, &request.signature) {
        (Some(key_id), Some(bytes)) => Some(Signature {
            key_id: key_id.clone(),
            bytes: Bytes::new(bytes.clone()),
        }),
        (None, None) => None,
        _ => {
            return Err(ApiError::InvalidArgument(
                "key_id and signature come together".into(),
            ));
        }
    };
    Ok((change, action, Arbiter { actor, signature }))
}

/// Wire form of an [`Escalation`].
#[must_use]
pub fn escalation_message(escalation: &Escalation) -> proto::Escalation {
    proto::Escalation {
        attempts: escalation
            .attempts
            .iter()
            .map(|a| proto::ReplayAttempt {
                attempt: a.attempt,
                harness: a.harness.clone(),
                outcome: match a.outcome {
                    ReplayOutcome::Running => proto::ReplayOutcome::Running,
                    ReplayOutcome::Proposed => proto::ReplayOutcome::Proposed,
                    ReplayOutcome::GaveUp => proto::ReplayOutcome::GaveUp,
                    ReplayOutcome::Killed => proto::ReplayOutcome::Killed,
                    ReplayOutcome::OverBudget => proto::ReplayOutcome::OverBudget,
                    ReplayOutcome::Failed => proto::ReplayOutcome::Failed,
                }
                .into(),
                change: a.change.map(wire::id),
                detail: a.detail.clone(),
                elapsed_ms: a.elapsed_ms,
                tokens: a.tokens,
                cost_micros: a.cost_micros,
                model: a.model.clone(),
                note: a.note.clone(),
            })
            .collect(),
        candidates: escalation
            .candidates
            .iter()
            .map(|c| proto::ArbitrationCandidate {
                change: wire::id(c.change),
                attempts: c.attempts.clone(),
                base: wire::id(c.base),
                result: wire::id(c.result),
                ops: c.ops,
                settled: c.settled.clone(),
            })
            .collect(),
        summary: escalation.summary.as_ref().map(summary_message),
        resolution: escalation.resolution.as_ref().map(|r| wire::id(r.change)),
        note: escalation.note.clone(),
    }
}

fn edge_kind(kind: i32) -> ApiResult<EdgeKind> {
    match proto::EdgeKind::try_from(kind) {
        Ok(proto::EdgeKind::Contains) => Ok(EdgeKind::Contains),
        Ok(proto::EdgeKind::References) => Ok(EdgeKind::References),
        Ok(proto::EdgeKind::Depends) => Ok(EdgeKind::Depends),
        Ok(proto::EdgeKind::Tests) => Ok(EdgeKind::Tests),
        Ok(proto::EdgeKind::DerivedFrom) => Ok(EdgeKind::DerivedFrom),
        Ok(proto::EdgeKind::Unspecified) | Err(_) => Err(ApiError::InvalidArgument(format!(
            "kind: not an edge kind: {kind}"
        ))),
    }
}

impl Inner {
    fn entry_with_record(&self, entry: &QueueEntry) -> proto::QueueEntry {
        let record = self.change_record(entry.change).ok();
        queue_entry_message(entry, record.as_ref())
    }

    /// Landed summaries of `changes`, in the given order.
    fn summaries(&self, changes: &[ChangeId]) -> Result<Vec<proto::ChangeSummary>> {
        changes
            .iter()
            .map(|c| {
                let record = self.change_record(*c)?;
                Ok(summary(self.store.log_position(*c)?, *c, &record))
            })
            .collect()
    }

    fn log_page(&self, q: &proto::LogQuery) -> ApiResult<proto::LogPage> {
        let after = wire::optional_object_id("after", q.after.as_deref())?;
        let node = match q.node.as_deref() {
            None | Some("") => None,
            Some(text) => Some(wire::node_id("node", text)?),
        };
        let path = match q.path.as_deref() {
            None | Some("") => None,
            Some(text) => Some(wire::repo_path("path", text)?),
        };
        let actor = q.actor.as_deref().filter(|a| !a.is_empty());
        let limit = match q.limit {
            0 => DEFAULT_LOG_LIMIT,
            n => n as usize,
        };
        let log = self.store.log().map_err(Error::from)?;
        let end = match after {
            None => log.len(),
            Some(after) => self
                .store
                .log_position(after)
                .map_err(Error::from)?
                .ok_or_else(|| ApiError::NotFound(format!("after: {after} is not landed")))?,
        };
        let mut items = Vec::new();
        let mut next = None;
        for position in (0..end).rev() {
            if items.len() == limit {
                next = items
                    .last()
                    .map(|s: &proto::ChangeSummary| s.change.clone());
                break;
            }
            let change = log[position];
            let filtered =
                actor.is_some() || q.since_ms.is_some() || node.is_some() || path.is_some();
            let record = match self.change_record(change) {
                Ok(record) => record,
                // A log entry that is not a readable record (a store written
                // by hand, or damaged) is listed bare, and never matches a
                // filter.
                Err(Error::MissingChange(_)) => {
                    if !filtered {
                        items.push(proto::ChangeSummary {
                            change: wire::id(change),
                            position: position as u64,
                            ..Default::default()
                        });
                    }
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            if let Some(actor) = actor
                && record.provenance.actor.id() != actor
            {
                continue;
            }
            if let Some(since) = q.since_ms
                && record.provenance.created_at.as_millis() < since
            {
                continue;
            }
            if let Some(node) = node
                && !crate::query::touches_node(&record, node)
            {
                continue;
            }
            if let Some(path) = &path
                && !self.touches_path(&record, path)?
            {
                continue;
            }
            items.push(summary(Some(position), change, &record));
        }
        Ok(proto::LogPage { items, next })
    }

    fn queue_view(&self, q: &proto::QueueQuery) -> ApiResult<Vec<proto::QueueEntry>> {
        let change = wire::optional_object_id("change", q.change.as_deref())?;
        let actor = q.actor.as_deref().filter(|a| !a.is_empty());
        let entries = match change {
            Some(change) => self.named_entries(change)?,
            None => self.queue_entries()?,
        };
        let mut out = Vec::new();
        for entry in entries {
            if q.pending_only && entry.status != QueueStatus::Queued {
                continue;
            }
            let record = self.change_record(entry.change).ok();
            if let Some(actor) = actor
                && record.as_ref().map(|r| r.provenance.actor.id()) != Some(actor)
            {
                continue;
            }
            let mut view = queue_entry_message(&entry, record.as_ref());
            // Asked about one conflicted change that has no summary yet (no
            // harness replayed it): summarize it for the arbiter.
            if change.is_some()
                && entry.status == QueueStatus::Conflicted
                && entry
                    .escalation
                    .as_ref()
                    .is_none_or(|e| e.summary.is_none())
                && let Some(report) = &entry.report
                && let Ok(summary) = self.conflict_summary(entry.change, report)
            {
                view.escalation.get_or_insert_with(Default::default).summary =
                    Some(summary_message(&summary));
            }
            // Asked about one change: name the report's nodes (`hord
            // conflicts`).
            if change.is_some()
                && let (Some(report), Some(wire_report)) = (&entry.report, view.report.as_mut())
            {
                self.name_report(report, wire_report)?;
            }
            out.push(view);
        }
        Ok(out)
    }

    /// Fill in `wire`'s node names and paths and the landed changes'
    /// summaries: definitions of the files the change and each landed
    /// change touched, at their base and result.
    fn name_report(&self, report: &ConflictReport, wire: &mut proto::ConflictReport) -> Result<()> {
        let mut records = vec![self.change_record(report.change)?];
        let mut summaries = std::collections::BTreeMap::new();
        for conflict in &report.conflicts {
            if let std::collections::btree_map::Entry::Vacant(slot) =
                summaries.entry(conflict.landed)
            {
                let record = self.change_record(conflict.landed)?;
                slot.insert(record.intent.summary.clone());
                records.push(record);
            }
        }
        let known = self.node_names(&records.iter().collect::<Vec<_>>())?;
        let name = |node: &mut proto::NodeRef| {
            if let Ok(id) = node.id.parse::<NodeId>()
                && let Some((name, path)) = known.get(&id)
            {
                node.name.clone_from(name);
                node.path = Some(path.clone());
            }
        };
        for conflict in &mut wire.conflicts {
            conflict.nodes.iter_mut().for_each(name);
            if let Ok(landed) = conflict.landed.parse::<ObjectId>()
                && let Some(summary) = summaries.get(&landed)
            {
                conflict.landed_summary.clone_from(summary);
            }
        }
        for merge in &mut wire.merge {
            merge.nodes.iter_mut().for_each(name);
        }
        Ok(())
    }

    /// ADR 0025: store `evidence` and attach it to the snapshot `change`
    /// resulted in (the landed record's result, if it landed rebased).
    fn attach_evidence(&self, change: ChangeId, bytes: &[u8]) -> ApiResult<ObjectId> {
        let evidence: Evidence = hord_encoding::decode(bytes).map_err(|err| {
            ApiError::InvalidArgument(format!("evidence: not an Evidence object: {err}"))
        })?;
        if hord_encoding::encode(&evidence).map_err(Error::from)? != bytes {
            return Err(ApiError::InvalidArgument(
                "evidence: not canonical CBOR (spec §3.9)".into(),
            ));
        }
        let landed = self.landed_as(change)?.unwrap_or(change);
        let result = self.change_record(landed)?.result;
        if evidence.snapshot != result {
            return Err(ApiError::InvalidArgument(format!(
                "evidence is for snapshot {}, but change {change} resulted in {result}",
                evidence.snapshot
            )));
        }
        // Stored and indexed under its snapshot (ADR 0025), where reuse
        // and policy find it.
        let id = self.store.put_evidence(&evidence).map_err(Error::from)?;
        self.emit(vec![crate::events::evidence_attached(
            change, id, &evidence,
        )])?;
        Ok(id)
    }
}

#[async_trait]
impl RepoBackend for LocalRepo {
    async fn get_objects(
        &self,
        request: proto::GetObjectsRequest,
    ) -> ApiResult<proto::GetObjectsResponse> {
        check_batch("ids", request.ids.len())?;
        let ids = wire::object_ids("ids", &request.ids)?;
        api(&self.repo, move |inner| {
            let mut objects = Vec::with_capacity(ids.len());
            let mut bytes = 0;
            for id in ids {
                let cbor = inner.get_bytes(id)?;
                bytes += cbor.len();
                if bytes > MAX_BATCH_BYTES && !objects.is_empty() {
                    return Err(ApiError::ResourceExhausted(format!(
                        "response over {MAX_BATCH_BYTES} bytes; request fewer ids"
                    )));
                }
                objects.push(proto::Object {
                    id: wire::id(id),
                    cbor,
                });
            }
            Ok(proto::GetObjectsResponse { objects })
        })
        .await
    }

    async fn put_objects(
        &self,
        request: proto::PutObjectsRequest,
    ) -> ApiResult<proto::PutObjectsResponse> {
        check_batch("objects", request.objects.len())?;
        let bytes: usize = request.objects.iter().map(|o| o.cbor.len()).sum();
        if bytes > MAX_BATCH_BYTES {
            return Err(ApiError::ResourceExhausted(format!(
                "objects: {bytes} bytes; at most {MAX_BATCH_BYTES} per call"
            )));
        }
        for object in &request.objects {
            wire::verified_object(object)?;
        }
        api(&self.repo, move |inner| {
            for object in &request.objects {
                inner.store.put(&object.cbor).map_err(Error::from)?;
            }
            Ok(proto::PutObjectsResponse {})
        })
        .await
    }

    async fn has(&self, request: proto::HasRequest) -> ApiResult<proto::HasResponse> {
        check_batch("ids", request.ids.len())?;
        let ids = wire::object_ids("ids", &request.ids)?;
        let repo = self.repo.clone();
        let tasks = self.repo.inner.tasks.clone();
        let present = tasks
            .spawn_blocking(move || ObjectSource::has(&repo, &ids))
            .await
            .map_err(|err| ApiError::Internal(err.to_string()))??;
        Ok(proto::HasResponse { present })
    }

    async fn head(&self, _request: proto::HeadRequest) -> ApiResult<proto::HeadResponse> {
        let head = self.repo.head().await?;
        Ok(proto::HeadResponse {
            change: head.change.map(wire::id),
        })
    }

    async fn log(&self, request: proto::LogQuery) -> ApiResult<proto::LogPage> {
        api(&self.repo, move |inner| inner.log_page(&request)).await
    }

    async fn refs(&self, request: proto::RefsRequest) -> ApiResult<proto::RefsResponse> {
        api(&self.repo, move |inner| {
            let refs = inner.store.refs(&request.prefix).map_err(Error::from)?;
            Ok(proto::RefsResponse {
                refs: refs.into_iter().map(|(k, v)| (k, wire::id(v))).collect(),
            })
        })
        .await
    }

    async fn submit(&self, request: proto::SubmitRequest) -> ApiResult<proto::SubmitResponse> {
        let change = wire::object_id("change", &request.change)?;
        let entry = self.repo.submit(change).await?;
        let submission = entry.seq;
        let entry = api(&self.repo, move |inner| Ok(inner.entry_with_record(&entry))).await?;
        Ok(proto::SubmitResponse {
            submission,
            entry: Some(entry),
        })
    }

    async fn queue(&self, request: proto::QueueQuery) -> ApiResult<proto::QueueResponse> {
        let entries = api(&self.repo, move |inner| inner.queue_view(&request)).await?;
        Ok(proto::QueueResponse { entries })
    }

    async fn arbitrate(
        &self,
        request: proto::ArbitrateRequest,
    ) -> ApiResult<proto::ArbitrateResponse> {
        let (change, action, arbiter) = arbitrate_request(&request)?;
        let (resolution, entry) = self.repo.arbitrate(change, action, arbiter).await?;
        let entry = api(&self.repo, move |inner| Ok(inner.entry_with_record(&entry))).await?;
        Ok(proto::ArbitrateResponse {
            change: wire::id(resolution),
            entry: Some(entry),
        })
    }

    async fn node_history(
        &self,
        request: proto::NodeHistoryRequest,
    ) -> ApiResult<proto::NodeHistoryResponse> {
        let node: NodeId = wire::node_id("node", &request.node)?;
        api(&self.repo, move |inner| {
            let changes = inner.store.node_history(node).map_err(Error::from)?;
            Ok(proto::NodeHistoryResponse {
                changes: inner.summaries(&changes)?,
            })
        })
        .await
    }

    async fn edges(&self, request: proto::EdgesRequest) -> ApiResult<proto::EdgesResponse> {
        let snapshot = wire::object_id("snapshot", &request.snapshot)?;
        let node = wire::node_id("node", &request.node)?;
        let kind = edge_kind(request.kind)?;
        api(&self.repo, move |inner| {
            let nodes = inner.edges(snapshot, node, kind)?;
            Ok(proto::EdgesResponse {
                nodes: nodes.iter().map(ToString::to_string).collect(),
            })
        })
        .await
    }

    async fn resolve_name(
        &self,
        request: proto::ResolveNameRequest,
    ) -> ApiResult<proto::ResolveNameResponse> {
        let snapshot = wire::object_id("snapshot", &request.snapshot)?;
        api(&self.repo, move |inner| {
            let nodes = inner.resolve_in(snapshot, &request.name)?;
            Ok(proto::ResolveNameResponse {
                nodes: nodes.iter().map(ToString::to_string).collect(),
            })
        })
        .await
    }

    async fn attach_evidence(
        &self,
        request: proto::AttachEvidenceRequest,
    ) -> ApiResult<proto::AttachEvidenceResponse> {
        let change = wire::object_id("change", &request.change)?;
        api(&self.repo, move |inner| {
            let id = inner.attach_evidence(change, &request.evidence)?;
            Ok(proto::AttachEvidenceResponse {
                evidence: wire::id(id),
            })
        })
        .await
    }

    async fn events(&self, request: proto::EventsRequest) -> ApiResult<EventStream> {
        Ok(self.repo.events(request.from).await?)
    }
}
