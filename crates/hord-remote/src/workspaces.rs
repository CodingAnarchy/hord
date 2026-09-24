//! [`RemoteWorkspaces`]: a client of a daemon's `Workspaces` service (ADR
//! 0024 amendment).

use async_trait::async_trait;
use hord_api::proto::workspaces_client::WorkspacesClient;
use hord_api::{ApiError, ApiResult, MAX_MESSAGE_BYTES, WorkspacesBackend, proto};

use crate::transport::Transport;

/// The `Workspaces` service of the daemon a [`crate::RemoteRepo`] is
/// connected to (from [`crate::RemoteRepo::workspaces`]).
#[derive(Clone, Debug)]
pub struct RemoteWorkspaces {
    client: WorkspacesClient<Transport>,
}

impl RemoteWorkspaces {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            client: WorkspacesClient::new(transport)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        }
    }
}

macro_rules! call {
    ($self:ident, $method:ident, $request:ident) => {
        $self
            .client
            .clone()
            .$method($request)
            .await
            .map(tonic::Response::into_inner)
            .map_err(ApiError::from)
    };
}

#[async_trait]
impl WorkspacesBackend for RemoteWorkspaces {
    async fn ws_new(&self, request: proto::WsNewRequest) -> ApiResult<proto::WsNewResponse> {
        call!(self, ws_new, request)
    }

    async fn ws_list(&self, request: proto::WsListRequest) -> ApiResult<proto::WsListResponse> {
        call!(self, ws_list, request)
    }

    async fn ws_rm(&self, request: proto::WsRmRequest) -> ApiResult<proto::WsRmResponse> {
        call!(self, ws_rm, request)
    }

    async fn ws_gc(&self, request: proto::WsGcRequest) -> ApiResult<proto::WsGcResponse> {
        call!(self, ws_gc, request)
    }

    async fn status(&self, request: proto::StatusRequest) -> ApiResult<proto::StatusResponse> {
        call!(self, status, request)
    }

    async fn propose(&self, request: proto::ProposeRequest) -> ApiResult<proto::ProposeResponse> {
        call!(self, propose, request)
    }

    async fn policy_check(
        &self,
        request: proto::PolicyCheckRequest,
    ) -> ApiResult<proto::PolicyCheckResponse> {
        call!(self, policy_check, request)
    }

    async fn verify(&self, request: proto::WsVerifyRequest) -> ApiResult<proto::WsVerifyResponse> {
        call!(self, verify, request)
    }

    async fn shutdown(
        &self,
        request: proto::ShutdownRequest,
    ) -> ApiResult<proto::ShutdownResponse> {
        call!(self, shutdown, request)
    }
}
