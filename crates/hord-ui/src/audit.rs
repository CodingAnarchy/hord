//! The M5 check (ADR 0030): the UI calls no RPC outside `hord.proto`.
//!
//! [`Audited`] wraps the backends the UI is given and logs every call by
//! its gRPC name (`hord.v1.RepoBackend/Queue`). [`unlisted`] returns the
//! logged names that are not RPCs of the published schema
//! (`x-hord-services`). Since the UI holds nothing but these backends
//! (`hord-ui` depends on no store, `hord-txn`, or `hord-diff`), every read
//! and every action it takes is in the log.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use hord_api::schema::json_schema;
use hord_api::{ApiResult, ChangesBackend, EventStream, RepoBackend, proto};

/// Calls made through [`Audited`] backends, in order.
#[derive(Clone, Debug, Default)]
pub struct AuditLog {
    calls: Arc<Mutex<Vec<String>>>,
}

impl AuditLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one call.
    pub fn record(&self, rpc: &str) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(rpc.to_owned());
    }

    /// Every call so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Every RPC of the published schema, as `package.Service/Method`.
#[must_use]
pub fn schema_rpcs() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(services) = json_schema()
        .get("x-hord-services")
        .and_then(|s| s.as_object())
    {
        for (service, methods) in services {
            for method in methods.as_object().into_iter().flat_map(|m| m.keys()) {
                out.insert(format!("{service}/{method}"));
            }
        }
    }
    out
}

/// Calls in `log` that are not RPCs of the published schema.
#[must_use]
pub fn unlisted(log: &AuditLog) -> Vec<String> {
    let rpcs = schema_rpcs();
    log.calls()
        .into_iter()
        .filter(|c| !rpcs.contains(c))
        .collect()
}

/// A backend whose calls are logged by RPC name.
#[derive(Clone, Debug)]
pub struct Audited<T: ?Sized> {
    log: AuditLog,
    inner: Arc<T>,
}

impl<T: ?Sized> Audited<T> {
    /// Log the calls made to `inner` in `log`.
    #[must_use]
    pub fn new(inner: Arc<T>, log: AuditLog) -> Self {
        Self { log, inner }
    }
}

macro_rules! audited {
    ($self:ident, $rpc:literal, $method:ident, $request:ident) => {{
        $self.log.record($rpc);
        $self.inner.$method($request).await
    }};
}

#[async_trait]
impl<T: RepoBackend + ?Sized> RepoBackend for Audited<T> {
    async fn get_objects(
        &self,
        request: proto::GetObjectsRequest,
    ) -> ApiResult<proto::GetObjectsResponse> {
        audited!(self, "hord.v1.RepoBackend/GetObjects", get_objects, request)
    }

    async fn put_objects(
        &self,
        request: proto::PutObjectsRequest,
    ) -> ApiResult<proto::PutObjectsResponse> {
        audited!(self, "hord.v1.RepoBackend/PutObjects", put_objects, request)
    }

    async fn has(&self, request: proto::HasRequest) -> ApiResult<proto::HasResponse> {
        audited!(self, "hord.v1.RepoBackend/Has", has, request)
    }

    async fn head(&self, request: proto::HeadRequest) -> ApiResult<proto::HeadResponse> {
        audited!(self, "hord.v1.RepoBackend/Head", head, request)
    }

    async fn log(&self, request: proto::LogQuery) -> ApiResult<proto::LogPage> {
        audited!(self, "hord.v1.RepoBackend/Log", log, request)
    }

    async fn refs(&self, request: proto::RefsRequest) -> ApiResult<proto::RefsResponse> {
        audited!(self, "hord.v1.RepoBackend/Refs", refs, request)
    }

    async fn submit(&self, request: proto::SubmitRequest) -> ApiResult<proto::SubmitResponse> {
        audited!(self, "hord.v1.RepoBackend/Submit", submit, request)
    }

    async fn queue(&self, request: proto::QueueQuery) -> ApiResult<proto::QueueResponse> {
        audited!(self, "hord.v1.RepoBackend/Queue", queue, request)
    }

    async fn arbitrate(
        &self,
        request: proto::ArbitrateRequest,
    ) -> ApiResult<proto::ArbitrateResponse> {
        audited!(self, "hord.v1.RepoBackend/Arbitrate", arbitrate, request)
    }

    async fn node_history(
        &self,
        request: proto::NodeHistoryRequest,
    ) -> ApiResult<proto::NodeHistoryResponse> {
        audited!(
            self,
            "hord.v1.RepoBackend/NodeHistory",
            node_history,
            request
        )
    }

    async fn edges(&self, request: proto::EdgesRequest) -> ApiResult<proto::EdgesResponse> {
        audited!(self, "hord.v1.RepoBackend/Edges", edges, request)
    }

    async fn resolve_name(
        &self,
        request: proto::ResolveNameRequest,
    ) -> ApiResult<proto::ResolveNameResponse> {
        audited!(
            self,
            "hord.v1.RepoBackend/ResolveName",
            resolve_name,
            request
        )
    }

    async fn attach_evidence(
        &self,
        request: proto::AttachEvidenceRequest,
    ) -> ApiResult<proto::AttachEvidenceResponse> {
        audited!(
            self,
            "hord.v1.RepoBackend/AttachEvidence",
            attach_evidence,
            request
        )
    }

    async fn events(&self, request: proto::EventsRequest) -> ApiResult<EventStream> {
        audited!(self, "hord.v1.RepoBackend/Events", events, request)
    }
}

#[async_trait]
impl<T: ChangesBackend + ?Sized> ChangesBackend for Audited<T> {
    async fn get_change(&self, request: proto::GetChangeRequest) -> ApiResult<proto::ChangeView> {
        audited!(self, "hord.v1.Changes/GetChange", get_change, request)
    }

    async fn change_diff(
        &self,
        request: proto::ChangeDiffRequest,
    ) -> ApiResult<proto::ChangeDiffResponse> {
        audited!(self, "hord.v1.Changes/ChangeDiff", change_diff, request)
    }

    async fn list_recordings(
        &self,
        request: proto::ListRecordingsRequest,
    ) -> ApiResult<proto::ListRecordingsResponse> {
        audited!(
            self,
            "hord.v1.Changes/ListRecordings",
            list_recordings,
            request
        )
    }

    async fn get_recording(
        &self,
        request: proto::GetRecordingRequest,
    ) -> ApiResult<proto::GetRecordingResponse> {
        audited!(self, "hord.v1.Changes/GetRecording", get_recording, request)
    }

    async fn node_lineage(
        &self,
        request: proto::NodeLineageRequest,
    ) -> ApiResult<proto::NodeLineageResponse> {
        audited!(self, "hord.v1.Changes/NodeLineage", node_lineage, request)
    }

    async fn change_trace(
        &self,
        request: proto::ChangeTraceRequest,
    ) -> ApiResult<proto::ChangeTraceResponse> {
        audited!(self, "hord.v1.Changes/ChangeTrace", change_trace, request)
    }

    async fn list_tree(
        &self,
        request: proto::ListTreeRequest,
    ) -> ApiResult<proto::ListTreeResponse> {
        audited!(self, "hord.v1.Changes/ListTree", list_tree, request)
    }

    async fn get_file(&self, request: proto::GetFileRequest) -> ApiResult<proto::GetFileResponse> {
        audited!(self, "hord.v1.Changes/GetFile", get_file, request)
    }

    async fn node_edges(
        &self,
        request: proto::NodeEdgesRequest,
    ) -> ApiResult<proto::NodeEdgesResponse> {
        audited!(self, "hord.v1.Changes/NodeEdges", node_edges, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_lists_the_rpcs_the_ui_uses() {
        let rpcs = schema_rpcs();
        for rpc in [
            "hord.v1.RepoBackend/Queue",
            "hord.v1.RepoBackend/Events",
            "hord.v1.RepoBackend/Arbitrate",
            "hord.v1.Changes/GetChange",
            "hord.v1.Changes/GetRecording",
        ] {
            assert!(rpcs.contains(rpc), "{rpc}");
        }
        let log = AuditLog::new();
        log.record("hord.v1.RepoBackend/Queue");
        log.record("hord.v1.Private/Peek");
        assert_eq!(unlisted(&log), ["hord.v1.Private/Peek"]);
    }
}
