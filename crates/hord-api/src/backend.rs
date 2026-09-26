//! [`RepoBackend`]: spec §10.5.2's trait in the generated wire types.

use std::pin::Pin;

use async_trait::async_trait;
use tokio_stream::Stream;

use crate::ApiResult;
use crate::proto;

/// Most ids (or objects) one batched object call carries (ADR 0024).
pub const MAX_BATCH_IDS: usize = 1_000;
/// Most bytes of object data one batched object call carries (ADR 0024).
pub const MAX_BATCH_BYTES: usize = 16 << 20;
/// Largest gRPC message the server and client accept or send: the batch
/// limit plus room for one object larger than it (a batch of one object is
/// never refused for size) and framing.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;
/// Page size of [`RepoBackend::log`] when [`proto::LogQuery::limit`] is 0.
pub const DEFAULT_LOG_LIMIT: usize = 100;

/// Position in a repository's event log (spec §10.5.3): monotonic, from 1.
/// [`proto::EventEnvelope::cursor`] carries it.
pub type EventCursor = u64;

/// Id of one submission: its lander queue sequence number.
pub type SubmissionId = u64;

/// Events from [`RepoBackend::events`], in cursor order. Live streams do not
/// end on their own; drop the stream to unsubscribe.
pub type EventStream = Pin<Box<dyn Stream<Item = ApiResult<proto::EventEnvelope>> + Send>>;

/// One repository, local or remote (spec §10.5.2, ADR 0024).
///
/// Method for method the spec's trait, in the messages of `hord.proto`, so
/// a gRPC server is a thin adapter over an implementation and a gRPC client
/// is one. `hord_txn::LocalRepo` implements it over `.hord/`; `RemoteRepo`
/// (`hord-remote`) over the gRPC client. Ids travel as strings (see
/// [`crate::wire`]); a malformed one is [`crate::ApiError::InvalidArgument`].
///
/// The conformance suite (`hord_api::conformance`, feature `conformance`) is
/// written once against `&dyn RepoBackend` and runs against every
/// implementation.
#[async_trait]
pub trait RepoBackend: Send + Sync {
    // ------------------------------------------------------------ objects

    /// Objects by id, in request order. At most [`MAX_BATCH_IDS`] ids and a
    /// response of at most [`MAX_BATCH_BYTES`]
    /// ([`crate::ApiError::ResourceExhausted`] otherwise; one object alone
    /// is always returned, whatever its size). A missing id is
    /// [`crate::ApiError::NotFound`].
    async fn get_objects(
        &self,
        request: proto::GetObjectsRequest,
    ) -> ApiResult<proto::GetObjectsResponse>;

    /// Store objects. Each [`proto::Object::id`] must be the BLAKE3 hash of
    /// its canonical CBOR ([`crate::ApiError::InvalidArgument`] otherwise).
    /// Idempotent. Same limits as [`Self::get_objects`].
    async fn put_objects(
        &self,
        request: proto::PutObjectsRequest,
    ) -> ApiResult<proto::PutObjectsResponse>;

    /// Whether each id is stored, in request order. At most
    /// [`MAX_BATCH_IDS`] ids.
    async fn has(&self, request: proto::HasRequest) -> ApiResult<proto::HasResponse>;

    // ------------------------------------------------------------ log & refs

    /// The latest landed change; unset before anything lands (spec §3.7).
    async fn head(&self, request: proto::HeadRequest) -> ApiResult<proto::HeadResponse>;

    /// Landed changes, newest first, filtered and paged by the query.
    async fn log(&self, request: proto::LogQuery) -> ApiResult<proto::LogPage>;

    /// Named refs (spec §3.7).
    async fn refs(&self, request: proto::RefsRequest) -> ApiResult<proto::RefsResponse>;

    // ------------------------------------------------------------ lander

    /// Hand a proposed change to the lander (spec §6.2). Durable on return.
    /// Submitting a change that is queued or landed returns its entry. An
    /// unknown change is [`crate::ApiError::NotFound`].
    async fn submit(&self, request: proto::SubmitRequest) -> ApiResult<proto::SubmitResponse>;

    /// Lander queue entries in submission order, filtered by the query.
    async fn queue(&self, request: proto::QueueQuery) -> ApiResult<proto::QueueResponse>;

    /// Resolve a parked change (spec §6.4 rung 3): keep what landed, take
    /// the parked change, replay it again, or land a given change, as a
    /// change whose parents include both colliding changes. An unknown
    /// change is [`crate::ApiError::NotFound`]; one that is not conflicted
    /// or waiting for arbitration is
    /// [`crate::ApiError::FailedPrecondition`].
    async fn arbitrate(
        &self,
        request: proto::ArbitrateRequest,
    ) -> ApiResult<proto::ArbitrateResponse>;

    // ------------------------------------------------------------ queries

    /// Landed changes that touched a node, in landing order.
    async fn node_history(
        &self,
        request: proto::NodeHistoryRequest,
    ) -> ApiResult<proto::NodeHistoryResponse>;

    /// Targets of one kind of edge leaving a node in a snapshot, in id
    /// order (spec §3.8).
    async fn edges(&self, request: proto::EdgesRequest) -> ApiResult<proto::EdgesResponse>;

    /// Definitions of a snapshot whose qualified name is the given name, or
    /// else whose name ends in `::name`, in id order. Empty when none match.
    async fn resolve_name(
        &self,
        request: proto::ResolveNameRequest,
    ) -> ApiResult<proto::ResolveNameResponse>;

    // ------------------------------------------------------------ evidence

    /// Store an Evidence object and attach it to the change's result
    /// snapshot (ADR 0025: evidence lives beside snapshots, never in a
    /// record). The evidence must be about that snapshot
    /// ([`crate::ApiError::InvalidArgument`] otherwise).
    async fn attach_evidence(
        &self,
        request: proto::AttachEvidenceRequest,
    ) -> ApiResult<proto::AttachEvidenceResponse>;

    // ------------------------------------------------------------ events

    /// The event stream (spec §10.5.3). With `from` set, every recorded
    /// event whose cursor is greater, then live events; unset, live events
    /// only. Cursors are persisted, so a client resumes after a disconnect
    /// or a restart by passing the last cursor it saw.
    async fn events(&self, request: proto::EventsRequest) -> ApiResult<EventStream>;

    // ------------------------------------------------------------ git bridge

    /// Record one git bridge divergence check (ADR 0036) as a
    /// [`proto::BridgeChecked`] event, and return its cursor. Backends that
    /// keep no event log return [`crate::ApiError::Unimplemented`].
    async fn record_bridge_check(
        &self,
        request: proto::BridgeChecked,
    ) -> ApiResult<proto::RecordBridgeCheckResponse> {
        let _ = request;
        Err(crate::ApiError::Unimplemented(
            "this backend does not record git bridge checks".to_owned(),
        ))
    }
}
