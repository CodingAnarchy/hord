//! [`WorkspacesBackend`]: the local-only `Workspaces` service of
//! `hord.proto` (ADR 0024 amendment), in the generated types.

use async_trait::async_trait;

use crate::{ApiResult, proto};

/// Workspace commands for one repository (ADR 0024 amendment): served by
/// its per-repo daemon over the local endpoint, and run in-process by the
/// CLI with `--no-daemon` or against a true remote. The daemon acts for
/// the [`proto::Caller`] in each request.
#[async_trait]
pub trait WorkspacesBackend: Send + Sync {
    /// `hord ws new`.
    async fn ws_new(&self, request: proto::WsNewRequest) -> ApiResult<proto::WsNewResponse>;
    /// `hord ws list`.
    async fn ws_list(&self, request: proto::WsListRequest) -> ApiResult<proto::WsListResponse>;
    /// `hord ws rm`.
    async fn ws_rm(&self, request: proto::WsRmRequest) -> ApiResult<proto::WsRmResponse>;
    /// `hord ws gc`.
    async fn ws_gc(&self, request: proto::WsGcRequest) -> ApiResult<proto::WsGcResponse>;
    /// `hord status`.
    async fn status(&self, request: proto::StatusRequest) -> ApiResult<proto::StatusResponse>;
    /// `hord propose`.
    async fn propose(&self, request: proto::ProposeRequest) -> ApiResult<proto::ProposeResponse>;
    /// `hord policy check`.
    async fn policy_check(
        &self,
        request: proto::PolicyCheckRequest,
    ) -> ApiResult<proto::PolicyCheckResponse>;
    /// Stop serving and release the store. Only a daemon implements it; an
    /// in-process backend returns [`crate::ApiError::Unimplemented`].
    async fn shutdown(&self, request: proto::ShutdownRequest)
    -> ApiResult<proto::ShutdownResponse>;
}
