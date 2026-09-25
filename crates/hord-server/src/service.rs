//! The tonic services: `hord.v1.RepoBackend` over the hosted backends, and
//! `hord.v1.Schema`.

use std::pin::Pin;
use std::sync::Arc;

use hord_api::proto::repo_backend_server::RepoBackend as GrpcTrait;
use hord_api::proto::schema_server::Schema as SchemaTrait;
use hord_api::proto::workspaces_server::Workspaces as WorkspacesTrait;
use hord_api::{ApiError, MAX_BATCH_IDS, RepoBackend, WorkspacesBackend, proto};
use hord_core::{Evidence, EvidenceKind};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::auth::{AuthStore, Principal};
use crate::hosts::Hosts;
use crate::ingest::{check_evidence_async, check_submit};
use crate::route::RepoName;

type GrpcResult<T> = Result<Response<T>, Status>;
type GrpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// `hord.v1.RepoBackend` served over [`Hosts`]: each call goes to the
/// repository its `/r/<name>/` prefix names ([`RepoName`]), else the only
/// one.
#[derive(Clone, Debug)]
pub struct GrpcRepoBackend {
    hosts: Arc<Hosts>,
    auth: Option<Arc<AuthStore>>,
}

impl GrpcRepoBackend {
    /// Serve `hosts`, checking callers against `auth` when set (spec
    /// §10.5.4).
    #[must_use]
    pub fn new(hosts: Arc<Hosts>, auth: Option<Arc<AuthStore>>) -> Self {
        Self { hosts, auth }
    }

    fn backend<T>(&self, request: &Request<T>) -> Result<Arc<dyn RepoBackend>, Status> {
        Ok(self.hosts.resolve(request.extensions().get::<RepoName>())?)
    }
}

/// Run one unary call: pick the backend, call it, and map its error.
macro_rules! unary {
    ($self:ident, $request:ident, $method:ident) => {{
        let backend = $self.backend(&$request)?;
        let reply = backend.$method($request.into_inner()).await?;
        Ok(Response::new(reply))
    }};
}

#[tonic::async_trait]
impl GrpcTrait for GrpcRepoBackend {
    async fn get_objects(
        &self,
        request: Request<proto::GetObjectsRequest>,
    ) -> GrpcResult<proto::GetObjectsResponse> {
        unary!(self, request, get_objects)
    }

    async fn put_objects(
        &self,
        request: Request<proto::PutObjectsRequest>,
    ) -> GrpcResult<proto::PutObjectsResponse> {
        unary!(self, request, put_objects)
    }

    async fn has(&self, request: Request<proto::HasRequest>) -> GrpcResult<proto::HasResponse> {
        unary!(self, request, has)
    }

    type StreamObjectsStream = GrpcStream<proto::Object>;

    /// Batches of at most [`MAX_BATCH_IDS`], each fetched with
    /// `get_objects`; a batch over the byte limit is fetched one object at
    /// a time.
    async fn stream_objects(
        &self,
        request: Request<proto::GetObjectsRequest>,
    ) -> GrpcResult<Self::StreamObjectsStream> {
        let backend = self.backend(&request)?;
        let ids = request.into_inner().ids;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            for chunk in ids.chunks(MAX_BATCH_IDS) {
                let batch = backend
                    .get_objects(proto::GetObjectsRequest {
                        ids: chunk.to_vec(),
                    })
                    .await;
                let objects = match batch {
                    Ok(batch) => batch.objects,
                    Err(ApiError::ResourceExhausted(_)) => {
                        let mut one_by_one = Vec::with_capacity(chunk.len());
                        for id in chunk {
                            match backend
                                .get_objects(proto::GetObjectsRequest {
                                    ids: vec![id.clone()],
                                })
                                .await
                            {
                                Ok(one) => one_by_one.extend(one.objects),
                                Err(err) => {
                                    let _ = tx.send(Err(err.into())).await;
                                    return;
                                }
                            }
                        }
                        one_by_one
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err.into())).await;
                        return;
                    }
                };
                for object in objects {
                    if tx.send(Ok(object)).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn head(&self, request: Request<proto::HeadRequest>) -> GrpcResult<proto::HeadResponse> {
        unary!(self, request, head)
    }

    async fn log(&self, request: Request<proto::LogQuery>) -> GrpcResult<proto::LogPage> {
        unary!(self, request, log)
    }

    async fn refs(&self, request: Request<proto::RefsRequest>) -> GrpcResult<proto::RefsResponse> {
        unary!(self, request, refs)
    }

    /// The change must be the caller's own, signed (spec §10.5.4).
    async fn submit(
        &self,
        request: Request<proto::SubmitRequest>,
    ) -> GrpcResult<proto::SubmitResponse> {
        let backend = self.backend(&request)?;
        let principal = request.extensions().get::<Principal>().cloned();
        let request = request.into_inner();
        check_submit(
            backend.as_ref(),
            self.auth.clone(),
            principal,
            &request.change,
        )
        .await?;
        Ok(Response::new(backend.submit(request).await?))
    }

    async fn queue(&self, request: Request<proto::QueueQuery>) -> GrpcResult<proto::QueueResponse> {
        unary!(self, request, queue)
    }

    async fn arbitrate(
        &self,
        request: Request<proto::ArbitrateRequest>,
    ) -> GrpcResult<proto::ArbitrateResponse> {
        unary!(self, request, arbitrate)
    }

    async fn node_history(
        &self,
        request: Request<proto::NodeHistoryRequest>,
    ) -> GrpcResult<proto::NodeHistoryResponse> {
        unary!(self, request, node_history)
    }

    async fn edges(
        &self,
        request: Request<proto::EdgesRequest>,
    ) -> GrpcResult<proto::EdgesResponse> {
        unary!(self, request, edges)
    }

    async fn resolve_name(
        &self,
        request: Request<proto::ResolveNameRequest>,
    ) -> GrpcResult<proto::ResolveNameResponse> {
        unary!(self, request, resolve_name)
    }

    /// The evidence must be the caller's own, signed, and of a kind its
    /// token may sign (spec §10.5.4). A review that lands on a change
    /// parked for missing evidence submits it again, so the lander
    /// re-evaluates head's policy with the review indexed.
    async fn attach_evidence(
        &self,
        request: Request<proto::AttachEvidenceRequest>,
    ) -> GrpcResult<proto::AttachEvidenceResponse> {
        let backend = self.backend(&request)?;
        let principal = request.extensions().get::<Principal>().cloned();
        let request = request.into_inner();
        let evidence = hord_encoding::decode::<Evidence>(&request.evidence).ok();
        if let Some(evidence) = &evidence {
            check_evidence_async(self.auth.clone(), principal, evidence.clone()).await?;
        }
        let change = request.change.clone();
        let reply = backend.attach_evidence(request).await?;
        if evidence.is_some_and(|e| e.kind == EvidenceKind::Review) {
            requeue_parked(backend.as_ref(), &change).await;
        }
        Ok(Response::new(reply))
    }

    type EventsStream = GrpcStream<proto::EventEnvelope>;

    async fn events(
        &self,
        request: Request<proto::EventsRequest>,
    ) -> GrpcResult<Self::EventsStream> {
        let backend = self.backend(&request)?;
        let stream = backend.events(request.into_inner()).await?;
        Ok(Response::new(Box::pin(
            stream.map(|item| item.map_err(Status::from)),
        )))
    }
}

/// Submit `change` again if its latest queue entry is parked for missing
/// evidence. Best effort: the review is attached either way, and the author
/// can resubmit.
async fn requeue_parked(backend: &dyn RepoBackend, change: &str) {
    let Ok(queue) = backend
        .queue(proto::QueueQuery {
            change: Some(change.to_owned()),
            ..Default::default()
        })
        .await
    else {
        return;
    };
    if let Some(entry) = queue.entries.last()
        && entry.status == proto::QueueStatus::Parked as i32
    {
        let _ = backend
            .submit(proto::SubmitRequest {
                change: entry.change.clone(),
            })
            .await;
    }
}

/// `hord.v1.Schema`: the descriptor set and JSON Schema (ADR 0024).
#[derive(Clone, Copy, Debug, Default)]
pub struct GrpcSchema;

#[tonic::async_trait]
impl SchemaTrait for GrpcSchema {
    async fn get_schema(
        &self,
        _request: Request<proto::GetSchemaRequest>,
    ) -> GrpcResult<proto::GetSchemaResponse> {
        Ok(Response::new(hord_api::schema::get_schema_response()))
    }
}

/// `hord.v1.Workspaces` over one [`WorkspacesBackend`]: the per-repo
/// daemon's service (ADR 0024 amendment).
#[derive(Clone)]
pub struct GrpcWorkspaces {
    inner: Arc<dyn WorkspacesBackend>,
}

impl std::fmt::Debug for GrpcWorkspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcWorkspaces").finish_non_exhaustive()
    }
}

impl GrpcWorkspaces {
    /// Serve `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn WorkspacesBackend>) -> Self {
        Self { inner }
    }
}

macro_rules! ws_call {
    ($self:ident, $request:ident, $method:ident) => {{
        let reply = $self.inner.$method($request.into_inner()).await?;
        Ok(Response::new(reply))
    }};
}

#[tonic::async_trait]
impl WorkspacesTrait for GrpcWorkspaces {
    async fn ws_new(
        &self,
        request: Request<proto::WsNewRequest>,
    ) -> GrpcResult<proto::WsNewResponse> {
        ws_call!(self, request, ws_new)
    }

    async fn ws_list(
        &self,
        request: Request<proto::WsListRequest>,
    ) -> GrpcResult<proto::WsListResponse> {
        ws_call!(self, request, ws_list)
    }

    async fn ws_rm(&self, request: Request<proto::WsRmRequest>) -> GrpcResult<proto::WsRmResponse> {
        ws_call!(self, request, ws_rm)
    }

    async fn ws_gc(&self, request: Request<proto::WsGcRequest>) -> GrpcResult<proto::WsGcResponse> {
        ws_call!(self, request, ws_gc)
    }

    async fn status(
        &self,
        request: Request<proto::StatusRequest>,
    ) -> GrpcResult<proto::StatusResponse> {
        ws_call!(self, request, status)
    }

    async fn propose(
        &self,
        request: Request<proto::ProposeRequest>,
    ) -> GrpcResult<proto::ProposeResponse> {
        ws_call!(self, request, propose)
    }

    async fn policy_check(
        &self,
        request: Request<proto::PolicyCheckRequest>,
    ) -> GrpcResult<proto::PolicyCheckResponse> {
        ws_call!(self, request, policy_check)
    }

    async fn verify(
        &self,
        request: Request<proto::WsVerifyRequest>,
    ) -> GrpcResult<proto::WsVerifyResponse> {
        ws_call!(self, request, verify)
    }

    async fn shutdown(
        &self,
        request: Request<proto::ShutdownRequest>,
    ) -> GrpcResult<proto::ShutdownResponse> {
        ws_call!(self, request, shutdown)
    }
}
