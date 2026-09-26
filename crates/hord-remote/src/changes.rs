//! [`RemoteChanges`]: a gRPC client for the read-only `Changes` service
//! (ADR 0030).

use async_trait::async_trait;
use hord_api::proto::changes_client::ChangesClient;
use hord_api::{ApiResult, ChangesBackend, MAX_MESSAGE_BYTES, proto};

use crate::transport::Transport;

/// The `Changes` service of the server a [`crate::RemoteRepo`] is connected
/// to, on the same connection.
#[derive(Clone, Debug)]
pub struct RemoteChanges {
    client: ChangesClient<Transport>,
}

impl RemoteChanges {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            client: ChangesClient::new(transport)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        }
    }
}

#[async_trait]
impl ChangesBackend for RemoteChanges {
    async fn get_change(&self, request: proto::GetChangeRequest) -> ApiResult<proto::ChangeView> {
        call!(self, get_change, request)
    }

    async fn change_diff(
        &self,
        request: proto::ChangeDiffRequest,
    ) -> ApiResult<proto::ChangeDiffResponse> {
        call!(self, change_diff, request)
    }

    async fn list_recordings(
        &self,
        request: proto::ListRecordingsRequest,
    ) -> ApiResult<proto::ListRecordingsResponse> {
        call!(self, list_recordings, request)
    }

    async fn get_recording(
        &self,
        request: proto::GetRecordingRequest,
    ) -> ApiResult<proto::GetRecordingResponse> {
        call!(self, get_recording, request)
    }

    async fn node_lineage(
        &self,
        request: proto::NodeLineageRequest,
    ) -> ApiResult<proto::NodeLineageResponse> {
        call!(self, node_lineage, request)
    }

    async fn change_trace(
        &self,
        request: proto::ChangeTraceRequest,
    ) -> ApiResult<proto::ChangeTraceResponse> {
        call!(self, change_trace, request)
    }

    async fn list_tree(
        &self,
        request: proto::ListTreeRequest,
    ) -> ApiResult<proto::ListTreeResponse> {
        call!(self, list_tree, request)
    }

    async fn get_file(&self, request: proto::GetFileRequest) -> ApiResult<proto::GetFileResponse> {
        call!(self, get_file, request)
    }

    async fn node_edges(
        &self,
        request: proto::NodeEdgesRequest,
    ) -> ApiResult<proto::NodeEdgesResponse> {
        call!(self, node_edges, request)
    }
}
