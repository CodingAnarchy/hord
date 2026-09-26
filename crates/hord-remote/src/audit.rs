//! [`RemoteAudit`]: a client of a server's `Audit` service (`hord audit`).

use async_trait::async_trait;
use hord_api::proto::audit_client::AuditClient;
use hord_api::{ApiResult, AuditBackend, MAX_MESSAGE_BYTES, proto};

use crate::transport::Transport;

/// The `Audit` service of the server a [`crate::RemoteRepo`] is connected
/// to (from [`crate::RemoteRepo::audit`]).
#[derive(Clone, Debug)]
pub struct RemoteAudit {
    client: AuditClient<Transport>,
}

impl RemoteAudit {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            client: AuditClient::new(transport)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        }
    }
}

#[async_trait]
impl AuditBackend for RemoteAudit {
    async fn audit_log(&self, request: proto::AuditRequest) -> ApiResult<proto::AuditReport> {
        call!(self, audit_log, request)
    }
}
