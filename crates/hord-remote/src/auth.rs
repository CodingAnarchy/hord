//! [`RemoteAuth`]: a client of a server's `Auth` service (spec §10.5.4).

use hord_api::proto::auth_client::AuthClient;
use hord_api::{ApiResult, proto};

use crate::transport::Transport;

/// The `Auth` service of the server a [`crate::RemoteRepo`] is connected
/// to (from [`crate::RemoteRepo::auth`]).
#[derive(Clone, Debug)]
pub struct RemoteAuth {
    client: AuthClient<Transport>,
}

impl RemoteAuth {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            client: AuthClient::new(transport),
        }
    }

    /// Exchange a user's password for a token, binding their key.
    pub async fn login(&self, request: proto::LoginRequest) -> ApiResult<proto::LoginResponse> {
        call!(self, login, request)
    }

    /// Mint an agent token and signing key (`admin`).
    pub async fn mint_token(
        &self,
        request: proto::MintTokenRequest,
    ) -> ApiResult<proto::MintTokenResponse> {
        call!(self, mint_token, request)
    }

    /// The caller's actor and scopes.
    pub async fn who_am_i(&self) -> ApiResult<proto::WhoAmIResponse> {
        let request = proto::WhoAmIRequest {};
        call!(self, who_am_i, request)
    }

    /// A bound key's actor.
    pub async fn get_key(&self, key_id: &str) -> ApiResult<proto::GetKeyResponse> {
        let request = proto::GetKeyRequest {
            key_id: key_id.to_owned(),
        };
        call!(self, get_key, request)
    }
}
