//! [`ChangesBackend`]: the read-only `Changes` service of `hord.proto`
//! (ADR 0030), in the generated types.

use async_trait::async_trait;

use crate::{ApiResult, proto};

/// Read-only views for the web UI (ADR 0030): a change decoded with names,
/// its text diff, and flight recordings. `hord-server` implements it over
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
}
