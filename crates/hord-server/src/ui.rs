//! The web UI (ADR 0030) mounted over [`Hosts`].
//!
//! Each page request gets a [`UiRepo`] whose backends are this server's own
//! gRPC services, called in process with the request's repository prefix
//! and [`Principal`] in the call's extensions. So the UI takes exactly the
//! path a remote client takes: the scope each RPC requires
//! ([`hord_api::auth::requirement`], checked here as [`crate::authz`]
//! checks it on the wire), provenance and signature checks on ingest, and
//! the re-queue of a parked change after a review.
//!
//! Reviews from the UI are signed with the server's signer
//! ([`Server::with_ui_signer`](crate::Server::with_ui_signer)): the key of
//! whoever runs `hord serve`. With auth, ingest accepts the review only when
//! the signed-in actor is that key's actor, so the UI never signs for
//! someone else.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::http::Extensions;
use hord_api::auth::{Requirement, requirement};
use hord_api::proto::changes_server::Changes as ChangesTrait;
use hord_api::proto::repo_backend_server::RepoBackend as GrpcTrait;
use hord_api::{ApiError, ApiResult, ChangesBackend, EventStream, RepoBackend, proto, wire};
use hord_core::sign::{self, SigningKey};
use hord_core::{Actor, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, Timestamp};
use hord_ui::{ReviewBackend, UiHosts, UiRepo};
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

use crate::auth::{AuthStore, Principal};
use crate::changes::GrpcChanges;
use crate::hosts::Hosts;
use crate::route::RepoName;
use crate::service::GrpcRepoBackend;

/// The review qualifier the UI signs (ADR 0026): a person at the UI.
pub const UI_REVIEW_KIND: &str = "human";

/// A key the UI signs reviews with, and the actor it is bound to.
#[derive(Clone)]
pub struct UiSigner {
    /// The signer's actor (the review's `produced_by`).
    pub actor: Actor,
    /// Its signing key.
    pub key: Arc<SigningKey>,
}

impl std::fmt::Debug for UiSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiSigner")
            .field("actor", &self.actor)
            .field("key", &self.key.public().key_id())
            .finish()
    }
}

/// [`UiHosts`] over the server's [`Hosts`] and its gRPC services.
#[derive(Debug)]
pub(crate) struct HostedUi {
    hosts: Arc<Hosts>,
    auth: Option<Arc<AuthStore>>,
    signer: Option<UiSigner>,
}

impl HostedUi {
    pub(crate) fn new(
        hosts: Arc<Hosts>,
        auth: Option<Arc<AuthStore>>,
        signer: Option<UiSigner>,
    ) -> Self {
        Self {
            hosts,
            auth,
            signer,
        }
    }
}

impl UiHosts for HostedUi {
    fn resolve(&self, extensions: &Extensions) -> ApiResult<UiRepo> {
        let prefix = extensions.get::<RepoName>().cloned();
        let name = self.hosts.addressed(prefix.as_ref())?.to_owned();
        // Fail early on an unknown repository.
        self.hosts.resolve(prefix.as_ref())?;
        let caller = Arc::new(Caller {
            backend: GrpcRepoBackend::new(Arc::clone(&self.hosts), self.auth.clone()),
            changes: GrpcChanges::new(Arc::clone(&self.hosts)),
            prefix: prefix.clone(),
            principal: extensions.get::<Principal>().cloned(),
            auth_required: self.auth.is_some(),
        });
        let review = self.signer.clone().map(|signer| {
            Arc::new(SigningReviewer {
                caller: Arc::clone(&caller),
                signer,
            }) as Arc<dyn ReviewBackend>
        });
        Ok(UiRepo {
            backend: Arc::clone(&caller) as Arc<dyn RepoBackend>,
            changes: caller,
            review,
            name,
            base: prefix.map_or_else(String::new, |RepoName(name)| format!("/r/{name}")),
        })
    }
}

/// This server's gRPC services called in process for one page request.
struct Caller {
    backend: GrpcRepoBackend,
    changes: GrpcChanges,
    prefix: Option<RepoName>,
    principal: Option<Principal>,
    auth_required: bool,
}

impl Caller {
    /// A request for `rpc` (`/hord.v1.Service/Method`), after the scope
    /// check the auth layer makes on the wire.
    fn request<T>(&self, rpc: &str, message: T) -> ApiResult<Request<T>> {
        if self.auth_required {
            let required = requirement(rpc)
                .ok_or_else(|| ApiError::PermissionDenied(format!("{rpc}: not an RPC")))?;
            if required != Requirement::Public {
                let principal = self.principal.as_ref().ok_or_else(|| {
                    ApiError::Unauthenticated(
                        "this server requires a token: sign in at /login".into(),
                    )
                })?;
                if !required.admits(&principal.scopes) {
                    return Err(ApiError::PermissionDenied(format!(
                        "{rpc} requires {required}; {} may not",
                        principal.actor.id()
                    )));
                }
            }
        }
        let mut request = Request::new(message);
        if let Some(prefix) = &self.prefix {
            request.extensions_mut().insert(prefix.clone());
        }
        if let Some(principal) = &self.principal {
            request.extensions_mut().insert(principal.clone());
        }
        Ok(request)
    }
}

fn reply<T>(result: Result<Response<T>, Status>) -> ApiResult<T> {
    result.map(Response::into_inner).map_err(ApiError::from)
}

/// Call one unary RPC of a service through [`Caller::request`].
macro_rules! via {
    ($self:ident, $svc:ident, $trait:ident, $rpc:literal, $method:ident, $message:ident) => {{
        let request = $self.request($rpc, $message)?;
        reply($trait::$method(&$self.$svc, request).await)
    }};
}

#[async_trait]
impl RepoBackend for Caller {
    async fn get_objects(
        &self,
        m: proto::GetObjectsRequest,
    ) -> ApiResult<proto::GetObjectsResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/GetObjects",
            get_objects,
            m
        )
    }

    async fn put_objects(
        &self,
        m: proto::PutObjectsRequest,
    ) -> ApiResult<proto::PutObjectsResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/PutObjects",
            put_objects,
            m
        )
    }

    async fn has(&self, m: proto::HasRequest) -> ApiResult<proto::HasResponse> {
        via!(self, backend, GrpcTrait, "/hord.v1.RepoBackend/Has", has, m)
    }

    async fn head(&self, m: proto::HeadRequest) -> ApiResult<proto::HeadResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/Head",
            head,
            m
        )
    }

    async fn log(&self, m: proto::LogQuery) -> ApiResult<proto::LogPage> {
        via!(self, backend, GrpcTrait, "/hord.v1.RepoBackend/Log", log, m)
    }

    async fn refs(&self, m: proto::RefsRequest) -> ApiResult<proto::RefsResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/Refs",
            refs,
            m
        )
    }

    async fn submit(&self, m: proto::SubmitRequest) -> ApiResult<proto::SubmitResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/Submit",
            submit,
            m
        )
    }

    async fn queue(&self, m: proto::QueueQuery) -> ApiResult<proto::QueueResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/Queue",
            queue,
            m
        )
    }

    async fn arbitrate(&self, m: proto::ArbitrateRequest) -> ApiResult<proto::ArbitrateResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/Arbitrate",
            arbitrate,
            m
        )
    }

    async fn node_history(
        &self,
        m: proto::NodeHistoryRequest,
    ) -> ApiResult<proto::NodeHistoryResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/NodeHistory",
            node_history,
            m
        )
    }

    async fn edges(&self, m: proto::EdgesRequest) -> ApiResult<proto::EdgesResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/Edges",
            edges,
            m
        )
    }

    async fn resolve_name(
        &self,
        m: proto::ResolveNameRequest,
    ) -> ApiResult<proto::ResolveNameResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/ResolveName",
            resolve_name,
            m
        )
    }

    async fn attach_evidence(
        &self,
        m: proto::AttachEvidenceRequest,
    ) -> ApiResult<proto::AttachEvidenceResponse> {
        via!(
            self,
            backend,
            GrpcTrait,
            "/hord.v1.RepoBackend/AttachEvidence",
            attach_evidence,
            m
        )
    }

    async fn events(&self, m: proto::EventsRequest) -> ApiResult<EventStream> {
        let request = self.request("/hord.v1.RepoBackend/Events", m)?;
        let stream = reply(GrpcTrait::events(&self.backend, request).await)?;
        Ok(Box::pin(stream.map(|item| item.map_err(ApiError::from))))
    }
}

#[async_trait]
impl ChangesBackend for Caller {
    async fn get_change(&self, m: proto::GetChangeRequest) -> ApiResult<proto::ChangeView> {
        via!(
            self,
            changes,
            ChangesTrait,
            "/hord.v1.Changes/GetChange",
            get_change,
            m
        )
    }

    async fn change_diff(
        &self,
        m: proto::ChangeDiffRequest,
    ) -> ApiResult<proto::ChangeDiffResponse> {
        via!(
            self,
            changes,
            ChangesTrait,
            "/hord.v1.Changes/ChangeDiff",
            change_diff,
            m
        )
    }

    async fn list_recordings(
        &self,
        m: proto::ListRecordingsRequest,
    ) -> ApiResult<proto::ListRecordingsResponse> {
        via!(
            self,
            changes,
            ChangesTrait,
            "/hord.v1.Changes/ListRecordings",
            list_recordings,
            m
        )
    }

    async fn get_recording(
        &self,
        m: proto::GetRecordingRequest,
    ) -> ApiResult<proto::GetRecordingResponse> {
        via!(
            self,
            changes,
            ChangesTrait,
            "/hord.v1.Changes/GetRecording",
            get_recording,
            m
        )
    }
}

/// Signs `Evidence { kind: Review, qualifier: human }` against the change's
/// result snapshot, as `hord review` does, and attaches it through
/// `AttachEvidence`.
struct SigningReviewer {
    caller: Arc<Caller>,
    signer: UiSigner,
}

#[async_trait]
impl ReviewBackend for SigningReviewer {
    async fn review(&self, change: String, approve: bool, message: String) -> ApiResult<String> {
        let reply = self
            .caller
            .get_objects(proto::GetObjectsRequest {
                ids: vec![change.clone()],
            })
            .await?;
        let object = reply
            .objects
            .first()
            .ok_or_else(|| ApiError::NotFound(format!("change {change}")))?;
        let record: ChangeRecord = hord_encoding::decode(&object.cbor)
            .map_err(|e| ApiError::InvalidArgument(format!("{change} is not a change: {e}")))?;
        let verdict = if approve { "approve" } else { "reject" };
        let mut evidence = Evidence {
            kind: EvidenceKind::Review,
            qualifier: Some(UI_REVIEW_KIND.into()),
            snapshot: record.result,
            toolchain: record.provenance.toolchain,
            command: format!("web UI review --as {UI_REVIEW_KIND} --{verdict} -m {message:?}"),
            scope: None,
            result: if approve {
                EvidenceResult::Pass
            } else {
                EvidenceResult::Fail { summary: message }
            },
            log: None,
            cost_ms: 0,
            produced_by: self.signer.actor.clone(),
            produced_at: now(),
            signature: None,
        };
        sign::sign_evidence(&mut evidence, &self.signer.key)
            .map_err(|e| ApiError::Internal(format!("sign the review: {e}")))?;
        let bytes = hord_encoding::encode(&evidence)
            .map_err(|e| ApiError::Internal(format!("encode the review: {e}")))?;
        let attached = self
            .caller
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(wire::object_id("change", &change)?),
                evidence: bytes,
            })
            .await?;
        Ok(attached.evidence)
    }
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    Timestamp::from_millis(ms)
}
