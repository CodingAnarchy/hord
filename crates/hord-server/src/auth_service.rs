//! `hord.v1.Auth` (spec §10.5.4): login against the user table, agent
//! token minting, who-am-i, and key lookup.

use std::sync::Arc;

use hord_api::auth::Scope;
use hord_api::proto::auth_server::Auth as AuthTrait;
use hord_api::{proto, wire};
use hord_core::{Actor, Bytes};
use tonic::{Request, Response, Status};

use crate::auth::{AuthError, AuthStore, Principal};

type GrpcResult<T> = Result<Response<T>, Status>;

/// `hord.v1.Auth` over the server's auth file, if it has one.
#[derive(Clone, Debug)]
pub struct GrpcAuth {
    store: Option<Arc<AuthStore>>,
}

impl GrpcAuth {
    /// Serve `store`; without one, every call but `WhoAmI` says the server
    /// does not require auth.
    #[must_use]
    pub fn new(store: Option<Arc<AuthStore>>) -> Self {
        Self { store }
    }

    fn store(&self) -> Result<Arc<AuthStore>, Status> {
        self.store
            .clone()
            .ok_or_else(|| Status::failed_precondition("this server does not require auth"))
    }
}

fn status(err: AuthError) -> Status {
    match err {
        AuthError::BadLogin => Status::unauthenticated(err.to_string()),
        AuthError::Rejected(_) | AuthError::UserExists(_) => {
            Status::invalid_argument(err.to_string())
        }
        _ => Status::internal(err.to_string()),
    }
}

/// Run `f` on the blocking pool: it reads and writes the auth file, and
/// hashing a password is slow on purpose.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| Status::internal(err.to_string()))?
        .map_err(status)
}

fn scope_strings(scopes: &[Scope]) -> Vec<String> {
    scopes.iter().map(ToString::to_string).collect()
}

#[tonic::async_trait]
impl AuthTrait for GrpcAuth {
    async fn login(
        &self,
        request: Request<proto::LoginRequest>,
    ) -> GrpcResult<proto::LoginResponse> {
        let store = self.store()?;
        let proto::LoginRequest {
            user,
            password,
            key_id,
        } = request.into_inner();
        let issued = blocking(move || store.login(&user, &password, &key_id)).await?;
        Ok(Response::new(proto::LoginResponse {
            token: issued.token,
            actor: Some(wire::actor(&issued.principal.actor)),
            scopes: scope_strings(&issued.principal.scopes),
        }))
    }

    async fn mint_token(
        &self,
        request: Request<proto::MintTokenRequest>,
    ) -> GrpcResult<proto::MintTokenResponse> {
        let store = self.store()?;
        let request = request.into_inner();
        let scopes = request
            .scopes
            .iter()
            .map(|s| s.parse::<Scope>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        let agent = Actor::Agent {
            id: request.agent_id,
            model: request.model,
            model_hash: Bytes::default(),
            harness: request.harness,
        };
        let (issued, key) = blocking(move || store.mint(&agent, &scopes)).await?;
        let pem = key
            .to_pem()
            .map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(proto::MintTokenResponse {
            token: issued.token,
            actor: Some(wire::actor(&issued.principal.actor)),
            scopes: scope_strings(&issued.principal.scopes),
            key_id: key.public().key_id(),
            private_key_pem: pem,
        }))
    }

    async fn who_am_i(
        &self,
        request: Request<proto::WhoAmIRequest>,
    ) -> GrpcResult<proto::WhoAmIResponse> {
        let principal = request.extensions().get::<Principal>();
        Ok(Response::new(proto::WhoAmIResponse {
            actor: principal.map(|p| wire::actor(&p.actor)),
            scopes: principal
                .map(|p| scope_strings(&p.scopes))
                .unwrap_or_default(),
            auth_required: self.store.is_some(),
        }))
    }

    async fn get_key(
        &self,
        request: Request<proto::GetKeyRequest>,
    ) -> GrpcResult<proto::GetKeyResponse> {
        let store = self.store()?;
        let key_id = request.into_inner().key_id;
        let lookup = key_id.clone();
        let actor = blocking(move || store.key_actor(&lookup))
            .await?
            .ok_or_else(|| Status::not_found(format!("no key {key_id} on this server")))?;
        Ok(Response::new(proto::GetKeyResponse {
            key_id,
            actor: Some(wire::actor(&actor)),
        }))
    }
}
