//! [`ChangesBackend`]: the read-only `Changes` service of `hord.proto`
//! (ADR 0030), in the generated types.

use async_trait::async_trait;

use crate::{ApiResult, proto};

/// Read-only views for the web UI (ADR 0030): a change decoded with names,
/// its text diff, flight recordings, and (views 4–6) a definition's
/// lineage, a change's provenance trace, and the repository browser. `hord-server` implements it over
/// a local repository; `hord-remote`'s `RemoteRepo` over the gRPC client.
#[async_trait]
pub trait ChangesBackend: Send + Sync {
    /// One change decoded. An unknown change is
    /// [`crate::ApiError::NotFound`].
    async fn get_change(&self, request: proto::GetChangeRequest) -> ApiResult<proto::ChangeView>;
    /// A change's text diff, base → result.
    async fn change_diff(
        &self,
        request: proto::ChangeDiffRequest,
    ) -> ApiResult<proto::ChangeDiffResponse>;
    /// Registered flight recordings.
    async fn list_recordings(
        &self,
        request: proto::ListRecordingsRequest,
    ) -> ApiResult<proto::ListRecordingsResponse>;
    /// One flight recording. An id that is not a stored recording is
    /// [`crate::ApiError::NotFound`].
    async fn get_recording(
        &self,
        request: proto::GetRecordingRequest,
    ) -> ApiResult<proto::GetRecordingResponse>;
    /// A definition's lineage. A NodeId no landed change touched and no
    /// snapshot at head has is [`crate::ApiError::NotFound`].
    async fn node_lineage(
        &self,
        request: proto::NodeLineageRequest,
    ) -> ApiResult<proto::NodeLineageResponse>;
    /// A change's provenance trace. An unknown change is
    /// [`crate::ApiError::NotFound`].
    async fn change_trace(
        &self,
        request: proto::ChangeTraceRequest,
    ) -> ApiResult<proto::ChangeTraceResponse>;
    /// One directory of a snapshot. A path with nothing under it is
    /// [`crate::ApiError::NotFound`].
    async fn list_tree(&self, request: proto::ListTreeRequest)
    -> ApiResult<proto::ListTreeResponse>;
    /// One file of a snapshot. A missing file is
    /// [`crate::ApiError::NotFound`].
    async fn get_file(&self, request: proto::GetFileRequest) -> ApiResult<proto::GetFileResponse>;
    /// A definition's edges in a snapshot. A NodeId the snapshot does not
    /// have is [`crate::ApiError::NotFound`].
    async fn node_edges(
        &self,
        request: proto::NodeEdgesRequest,
    ) -> ApiResult<proto::NodeEdgesResponse>;
}
