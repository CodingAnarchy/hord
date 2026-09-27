//! [`AuditBackend`]: the `Audit` service of `hord.proto`, M6's acceptance
//! auditor (`hord audit`, spec §12 M6), in the generated types.

use async_trait::async_trait;

use crate::{ApiResult, proto};

/// Audit a window of a repository's log. `hord-server` implements it over a
/// local repository; `hord-remote` over the gRPC client.
#[async_trait]
pub trait AuditBackend: Send + Sync {
    /// Check the window's landings, reviews, arbitrations, and bridge
    /// checks.
    async fn audit_log(&self, request: proto::AuditRequest) -> ApiResult<proto::AuditReport>;
}
