//! [`LocalChanges`]: the read-only `Changes` service (ADR 0030) over a
//! local repository: a change decoded with names, its text diff, the
//! flight recordings registered under `.hord/recordings/`, and views 4–6
//! ([`crate::browse`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use hord_api::proto::changes_server::Changes as GrpcTrait;
use hord_api::{ApiError, ApiResult, ChangesBackend, RepoBackend, proto, recording, wire};
use hord_core::{
    Acceptance, Blob, ChangeId, ChangeRecord, Evidence, IntentRef, NodeId, ObjectId, Op, RepoPath,
    SnapshotId,
};
use hord_txn::{LocalRepo, Repo};
use tokio::task::spawn_blocking;
use tonic::{Request, Response, Status};

use crate::hosts::Hosts;
use crate::route::RepoName;

/// Directory under `.hord/` whose entries name recordings: one empty file
/// per recording, named by the hex ObjectId of its `Blob` (spec §10.4:
/// "stored as Blobs in the repository under `.hord/recordings/`").
pub const RECORDINGS_DIR: &str = "recordings";

/// Events read from the log per batch when collecting a change's history.
const HISTORY_BATCH: usize = 4_096;

/// Store a flight recording (JSON Lines, [`recording::FORMAT`]) as a `Blob`
/// and register it under `.hord/recordings/`. Returns the Blob's id. The
/// recording is parsed first, so only a readable one is registered.
pub fn save_recording(repo: &Repo, bytes: Vec<u8>) -> ApiResult<ObjectId> {
    recording::parse(&bytes)?;
    let store = repo.store();
    let id = store
        .put_object(&Blob::new(bytes))
        .map_err(|e| ApiError::Internal(format!("store recording: {e}")))?;
    let dir = store.hord_dir().join(RECORDINGS_DIR);
    fs::create_dir_all(&dir)
        .and_then(|()| fs::write(dir.join(id.to_hex()), b""))
        .map_err(|e| ApiError::Internal(format!("register recording: {e}")))?;
    Ok(id)
}

/// The `Changes` service over one [`LocalRepo`].
#[derive(Clone, Debug)]
pub struct LocalChanges {
    pub(crate) local: Arc<LocalRepo>,
}

impl LocalChanges {
    /// Serve `local`.
    #[must_use]
    pub fn new(local: Arc<LocalRepo>) -> Self {
        Self { local }
    }

    pub(crate) fn repo(&self) -> &Repo {
        self.local.repo()
    }

    /// Run blocking store work off the async threads.
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(Repo) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let repo = self.repo().clone();
        spawn_blocking(move || f(repo))
            .await
            .map_err(|e| ApiError::Internal(format!("task: {e}")))?
    }
}

#[async_trait]
impl ChangesBackend for LocalChanges {
    async fn get_change(&self, request: proto::GetChangeRequest) -> ApiResult<proto::ChangeView> {
        let id = wire::object_id("change", &request.change)?;
        let record = self.change(id).await?;
        let names = Names::for_records(self.repo(), &[&record]).await?;
        let queue = self
            .local
            .queue(proto::QueueQuery {
                change: Some(request.change.clone()),
                ..Default::default()
            })
            .await?
            .entries
            .pop();
        let mut ids = BTreeSet::from([request.change.clone()]);
        if let Some(entry) = &queue {
            ids.insert(entry.change.clone());
            ids.extend(entry.landed.clone());
        }
        if let Some(from) = record.rebased_from {
            ids.insert(wire::id(from));
        }
        let landed = match queue.as_ref().and_then(|q| q.landed.as_deref()) {
            Some(landed) if landed != request.change => Some(
                self.change(wire::object_id("landed", landed)?)
                    .await?
                    .result,
            ),
            _ => None,
        };
        let evidence = self.evidence(&record, landed).await?;
        let history = self.history(ids).await?;
        Ok(proto::ChangeView {
            change: request.change,
            base: wire::id(record.base),
            result: wire::id(record.result),
            parents: record.parents.iter().copied().map(wire::id).collect(),
            intent: Some(intent_view(&record)),
            provenance: Some(proto::ProvenanceView {
                actor: Some(wire::actor(&record.provenance.actor)),
                toolchain: wire::id(record.provenance.toolchain),
                created_at_ms: record.provenance.created_at.as_millis(),
                session: record.provenance.session.clone(),
                parent_intent: record.provenance.parent_intent.map(wire::id),
                signature_key: record.signature.as_ref().map(|s| s.key_id.clone()),
            }),
            ops: record.ops.iter().map(|op| op_view(op, &names)).collect(),
            read_set: record.read_set.iter().map(|n| names.view(*n)).collect(),
            write_set: record.write_set.iter().map(|n| names.view(*n)).collect(),
            evidence,
            rebased_from: record.rebased_from.map(wire::id),
            queue,
            history,
        })
    }

    async fn change_diff(
        &self,
        request: proto::ChangeDiffRequest,
    ) -> ApiResult<proto::ChangeDiffResponse> {
        let id = wire::object_id("change", &request.change)?;
        let record = self.change(id).await?;
        let repo = self.repo();
        let paths = repo.changed_paths(record.base, record.result).await?;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let before = repo.file_bytes(record.base, path.clone()).await?;
            let after = repo.file_bytes(record.result, path.clone()).await?;
            files.push(file_diff(
                &path,
                before.as_ref().map(|b| b.as_slice()),
                after.as_ref().map(|b| b.as_slice()),
            ));
        }
        Ok(proto::ChangeDiffResponse { files })
    }

    async fn list_recordings(
        &self,
        _request: proto::ListRecordingsRequest,
    ) -> ApiResult<proto::ListRecordingsResponse> {
        self.blocking(|repo| {
            let dir = repo.store().hord_dir().join(RECORDINGS_DIR);
            let mut ids = Vec::new();
            match fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry.map_err(|e| io_err(&dir, &e))?;
                        // Anything not named by an object id is not ours.
                        if let Some(id) = entry
                            .file_name()
                            .to_str()
                            .and_then(|n| n.parse::<ObjectId>().ok())
                        {
                            ids.push(id);
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&dir, &e)),
            }
            ids.sort();
            let mut recordings = Vec::with_capacity(ids.len());
            for id in ids {
                let (header, events) = load_recording(&repo, id)?;
                recordings.push(proto::RecordingInfo {
                    id: wire::id(id),
                    header: Some(header),
                    events: events.len() as u64,
                });
            }
            Ok(proto::ListRecordingsResponse { recordings })
        })
        .await
    }

    async fn get_recording(
        &self,
        request: proto::GetRecordingRequest,
    ) -> ApiResult<proto::GetRecordingResponse> {
        let id = wire::object_id("id", &request.id)?;
        self.blocking(move |repo| {
            let (header, events) = load_recording(&repo, id)?;
            Ok(proto::GetRecordingResponse {
                header: Some(header),
                events,
            })
        })
        .await
    }

    async fn node_lineage(
        &self,
        request: proto::NodeLineageRequest,
    ) -> ApiResult<proto::NodeLineageResponse> {
        self.lineage(request).await
    }

    async fn change_trace(
        &self,
        request: proto::ChangeTraceRequest,
    ) -> ApiResult<proto::ChangeTraceResponse> {
        self.trace(request).await
    }

    async fn list_tree(
        &self,
        request: proto::ListTreeRequest,
    ) -> ApiResult<proto::ListTreeResponse> {
        self.tree(request).await
    }

    async fn get_file(&self, request: proto::GetFileRequest) -> ApiResult<proto::GetFileResponse> {
        self.file(request).await
    }

    async fn node_edges(
        &self,
        request: proto::NodeEdgesRequest,
    ) -> ApiResult<proto::NodeEdgesResponse> {
        self.neighbourhood(request).await
    }
}

impl LocalChanges {
    pub(crate) async fn change(&self, id: ChangeId) -> ApiResult<ChangeRecord> {
        self.blocking(move |repo| {
            if !repo.store().contains(id).map_err(internal)? {
                return Err(ApiError::NotFound(format!("change {id}")));
            }
            repo.store()
                .get_object(id)
                .map_err(|e| ApiError::InvalidArgument(format!("{id} is not a change: {e}")))
        })
        .await
    }

    /// The author's evidence, then what is indexed for the record's result
    /// and, when it landed as another record, for that record's result.
    pub(crate) async fn evidence(
        &self,
        record: &ChangeRecord,
        landed: Option<SnapshotId>,
    ) -> ApiResult<Vec<proto::ChangeEvidence>> {
        let author = record.evidence.clone();
        let result = record.result;
        self.blocking(move |repo| {
            let store = repo.store();
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            let mut sources = vec![("author", author)];
            sources.push(("snapshot", store.evidence_at(result).map_err(internal)?));
            if let Some(landed) = landed {
                sources.push(("landed", store.evidence_at(landed).map_err(internal)?));
            }
            for (source, ids) in sources {
                for id in ids {
                    if !seen.insert(id) {
                        continue;
                    }
                    let ev: Evidence = store.get_object(id).map_err(internal)?;
                    out.push(proto::ChangeEvidence {
                        id: wire::id(id),
                        kind: Some(wire::evidence_kind(&ev.kind)),
                        result: Some(wire::evidence_result(&ev.result)),
                        qualifier: ev.qualifier.clone(),
                        command: ev.command.clone(),
                        produced_by: Some(wire::actor(&ev.produced_by)),
                        produced_at_ms: ev.produced_at.as_millis(),
                        cost_ms: ev.cost_ms,
                        source: source.into(),
                    });
                }
            }
            Ok(out)
        })
        .await
    }

    /// Recorded events that name any of `ids`, in cursor order.
    async fn history(&self, ids: BTreeSet<String>) -> ApiResult<Vec<proto::EventEnvelope>> {
        let mut out = Vec::new();
        let mut after = 0;
        loop {
            let batch = self.repo().recorded_events(after, HISTORY_BATCH).await?;
            let Some(last) = batch.last() else {
                return Ok(out);
            };
            after = last.cursor;
            out.extend(batch.into_iter().filter(|e| names_any(e, &ids)));
        }
    }
}

/// Whether an event is about one of `ids`.
pub fn names_any(envelope: &proto::EventEnvelope, ids: &BTreeSet<String>) -> bool {
    use proto::event::Kind;
    let Some(kind) = envelope.event.as_ref().and_then(|e| e.kind.as_ref()) else {
        return false;
    };
    let named: Vec<&str> = match kind {
        Kind::Submitted(e) => vec![&e.change],
        Kind::ConflictCheck(e) => vec![&e.change],
        Kind::Verifying(e) => vec![&e.change],
        Kind::EvidenceAttached(e) => vec![&e.change],
        Kind::Replaying(e) => vec![&e.change],
        Kind::Parked(e) => vec![&e.change],
        Kind::Arbitrated(e) => vec![&e.change, &e.result],
        Kind::Landed(e) => {
            let mut v = vec![e.change.as_str()];
            v.extend(e.submitted.as_deref());
            v
        }
        Kind::Rejected(e) => vec![&e.change],
        Kind::HeadMoved(_) | Kind::BridgeChecked(_) => vec![],
    };
    named.into_iter().any(|id| ids.contains(id))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(e.to_string())
}

fn io_err(dir: &Path, e: &io::Error) -> ApiError {
    ApiError::Internal(format!("{}: {e}", dir.display()))
}

fn load_recording(
    repo: &Repo,
    id: ObjectId,
) -> ApiResult<(proto::RecordingHeader, Vec<proto::EventEnvelope>)> {
    if !repo.store().contains(id).map_err(internal)? {
        return Err(ApiError::NotFound(format!("recording {id}")));
    }
    let blob: Blob = repo
        .store()
        .get_object(id)
        .map_err(|e| ApiError::InvalidArgument(format!("{id} is not a recording: {e}")))?;
    recording::parse(blob.bytes.as_slice())
}

fn intent_view(record: &ChangeRecord) -> proto::IntentView {
    let intent = &record.intent;
    proto::IntentView {
        summary: intent.summary.clone(),
        body: intent.body.clone(),
        refs: intent
            .refs
            .iter()
            .map(|r| match r {
                IntentRef::Issue { id } => format!("issue:{id}"),
                IntentRef::Url { url } => format!("url:{url}"),
                IntentRef::Change { change } => format!("change:{}", wire::id(*change)),
                IntentRef::GitCommit { sha } => format!("git:{sha}"),
            })
            .collect(),
        acceptance: intent
            .acceptance
            .iter()
            .map(|a| match a {
                Acceptance::Test { name } => format!("test: {name}"),
                Acceptance::Check { command } => format!("check: {command}"),
                Acceptance::Invariant { description } => format!("invariant: {description}"),
            })
            .collect(),
    }
}

/// One file's unified diff; binary when either side is not UTF-8.
fn file_diff(path: &RepoPath, before: Option<&[u8]>, after: Option<&[u8]>) -> proto::FileDiff {
    fn text(bytes: Option<&[u8]>) -> Option<&str> {
        bytes.map_or(Some(""), |b| str::from_utf8(b).ok())
    }
    match (text(before), text(after)) {
        (Some(old), Some(new)) => {
            let patch = diffy::create_patch(old, new);
            let body = patch.to_string();
            // diffy prints `--- original` / `+++ modified`; name the file.
            let body = body
                .strip_prefix("--- original\n+++ modified\n")
                .unwrap_or(&body);
            proto::FileDiff {
                path: path.to_string(),
                unified: format!("--- a/{path}\n+++ b/{path}\n{body}"),
                binary: false,
            }
        }
        _ => proto::FileDiff {
            path: path.to_string(),
            unified: String::new(),
            binary: true,
        },
    }
}

/// Names for the definitions in the files a record touches, looked up in
/// its base and result.
pub(crate) struct Names {
    known: BTreeMap<NodeId, (Option<String>, RepoPath)>,
}

impl Names {
    pub(crate) async fn for_records(repo: &Repo, records: &[&ChangeRecord]) -> ApiResult<Self> {
        let mut paths = BTreeSet::new();
        let mut snapshots = BTreeSet::new();
        for record in records {
            snapshots.insert(record.base);
            snapshots.insert(record.result);
            paths.extend(repo.changed_paths(record.base, record.result).await?);
        }
        let mut known = BTreeMap::new();
        // The file root id (ADR 0015) is the whole-file and glue identity.
        for path in &paths {
            known.insert(
                NodeId::file_root(path),
                (Some("(file)".into()), path.clone()),
            );
        }
        for snapshot in snapshots {
            for path in &paths {
                for def in repo.definitions_in(snapshot, path.clone()).await? {
                    known
                        .entry(def.node)
                        .or_insert_with(|| (def.name.map(|n| n.as_str().to_owned()), def.path));
                }
            }
        }
        Ok(Self { known })
    }

    pub(crate) fn view(&self, node: NodeId) -> proto::NodeRef {
        let known = self.known.get(&node);
        proto::NodeRef {
            id: node.to_string(),
            name: known.and_then(|(name, _)| name.clone()),
            path: known.map(|(_, path)| path.to_string()),
        }
    }

    pub(crate) fn text(&self, node: NodeId) -> String {
        let view = self.view(node);
        match (view.name, view.path) {
            (Some(name), Some(path)) => format!("{name} ({path})"),
            (None, Some(path)) => format!("{} ({path})", view.id),
            _ => view.id,
        }
    }
}

/// An op flattened for display, with names (the same shape `hord status`
/// prints).
pub(crate) fn op_view(op: &Op, names: &Names) -> proto::OpView {
    let mut view = proto::OpView::default();
    match op {
        Op::Insert {
            parent,
            index,
            node,
        } => {
            view.op = "insert".into();
            view.parent = Some(parent.to_string());
            view.index = Some(*index);
            view.node = Some(wire::id(*node));
            view.text = format!("insert into {}", names.text(*parent));
        }
        Op::Delete { node } => {
            view.op = "delete".into();
            view.node = Some(node.to_string());
            view.text = format!("delete {}", names.text(*node));
        }
        Op::Replace { node, from, to } => {
            view.op = "replace".into();
            view.node = Some(node.to_string());
            view.from = Some(wire::id(*from));
            view.to = Some(wire::id(*to));
            view.text = format!("replace {}", names.text(*node));
        }
        Op::Move {
            node,
            from_parent,
            to_parent,
            index,
        } => {
            view.op = "move".into();
            view.node = Some(node.to_string());
            view.from = Some(from_parent.to_string());
            view.to = Some(to_parent.to_string());
            view.index = Some(*index);
            view.text = format!(
                "move {} from {} to {}",
                names.text(*node),
                names.text(*from_parent),
                names.text(*to_parent)
            );
        }
        Op::Rename { node, from, to } => {
            view.op = "rename".into();
            view.node = Some(node.to_string());
            view.from = Some(from.as_str().to_owned());
            view.to = Some(to.as_str().to_owned());
            view.text = format!("rename {from} → {to}");
        }
        Op::Blob { path, from, to } => {
            view.op = "blob".into();
            view.path = Some(path.to_string());
            view.from = from.map(wire::id);
            view.to = to.map(wire::id);
            view.text = match (from, to) {
                (None, Some(_)) => format!("create {path}"),
                (Some(_), None) => format!("delete {path}"),
                _ => format!("modify {path}"),
            };
        }
        Op::Tree { path, kind } => {
            view.op = "tree".into();
            view.path = Some(path.to_string());
            view.kind = Some(format!("{kind:?}"));
            view.text = format!("{kind:?} {path}").to_lowercase();
        }
    }
    view
}

/// `hord.v1.Changes` served over [`Hosts`]: each call goes to the
/// repository its `/r/<name>/` prefix names, else the only one.
#[derive(Clone, Debug)]
pub struct GrpcChanges {
    hosts: Arc<Hosts>,
}

impl GrpcChanges {
    /// Serve `hosts`.
    #[must_use]
    pub fn new(hosts: Arc<Hosts>) -> Self {
        Self { hosts }
    }
}

/// Run one call on the addressed repository's `Changes` backend.
macro_rules! changes_call {
    ($self:ident, $request:ident, $method:ident) => {{
        let backend = $self
            .hosts
            .resolve_changes($request.extensions().get::<RepoName>())?;
        let reply = backend.$method($request.into_inner()).await?;
        Ok(Response::new(reply))
    }};
}

#[tonic::async_trait]
impl GrpcTrait for GrpcChanges {
    async fn get_change(
        &self,
        request: Request<proto::GetChangeRequest>,
    ) -> Result<Response<proto::ChangeView>, Status> {
        changes_call!(self, request, get_change)
    }

    async fn change_diff(
        &self,
        request: Request<proto::ChangeDiffRequest>,
    ) -> Result<Response<proto::ChangeDiffResponse>, Status> {
        changes_call!(self, request, change_diff)
    }

    async fn list_recordings(
        &self,
        request: Request<proto::ListRecordingsRequest>,
    ) -> Result<Response<proto::ListRecordingsResponse>, Status> {
        changes_call!(self, request, list_recordings)
    }

    async fn get_recording(
        &self,
        request: Request<proto::GetRecordingRequest>,
    ) -> Result<Response<proto::GetRecordingResponse>, Status> {
        changes_call!(self, request, get_recording)
    }

    async fn node_lineage(
        &self,
        request: Request<proto::NodeLineageRequest>,
    ) -> Result<Response<proto::NodeLineageResponse>, Status> {
        changes_call!(self, request, node_lineage)
    }

    async fn change_trace(
        &self,
        request: Request<proto::ChangeTraceRequest>,
    ) -> Result<Response<proto::ChangeTraceResponse>, Status> {
        changes_call!(self, request, change_trace)
    }

    async fn list_tree(
        &self,
        request: Request<proto::ListTreeRequest>,
    ) -> Result<Response<proto::ListTreeResponse>, Status> {
        changes_call!(self, request, list_tree)
    }

    async fn get_file(
        &self,
        request: Request<proto::GetFileRequest>,
    ) -> Result<Response<proto::GetFileResponse>, Status> {
        changes_call!(self, request, get_file)
    }

    async fn node_edges(
        &self,
        request: Request<proto::NodeEdgesRequest>,
    ) -> Result<Response<proto::NodeEdgesResponse>, Status> {
        changes_call!(self, request, node_edges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_diffs_name_the_file_and_binary_files_are_flagged() -> Result<(), String> {
        let path: RepoPath = "src/lib.rs".parse().map_err(|e| format!("{e:?}"))?;
        let diff = file_diff(&path, Some(b"a\nb\n"), Some(b"a\nc\n"));
        assert!(!diff.binary);
        assert!(
            diff.unified
                .starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n")
        );
        assert!(diff.unified.contains("-b\n+c\n"), "{}", diff.unified);
        let created = file_diff(&path, None, Some(b"x\n"));
        assert!(created.unified.contains("+x"));
        assert!(file_diff(&path, Some(&[0xff, 0xfe]), None).binary);
        Ok(())
    }
}
