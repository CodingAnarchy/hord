//! The authorization layer (spec §10.5.4): a tower layer in front of every
//! route that reads the bearer token, checks it against the auth file, and
//! enforces the scope [`hord_api::auth::requirement`] names for the RPC.
//!
//! It runs after the `/r/<name>/` prefix is stripped, so it sees gRPC paths.
//! The caller's [`Principal`] goes into the request's extensions, where
//! the services read it (provenance and evidence checks). A server without
//! an auth file passes everything through, with no principal.

use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hord_api::auth::{AUTHORIZATION, Requirement, bearer, requirement};
use tonic::Status;
use tower::{Layer, Service};

use crate::auth::{AuthStore, Principal};

/// Tower layer that authorizes every request against an [`AuthStore`].
#[derive(Clone, Debug)]
pub struct AuthLayer(pub Option<Arc<AuthStore>>);

impl<S> Layer<S> for AuthLayer {
    type Service = Authorized<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Authorized {
            inner,
            store: self.0.clone(),
        }
    }
}

/// Service made by [`AuthLayer`].
#[derive(Clone, Debug)]
pub struct Authorized<S> {
    inner: S,
    store: Option<Arc<AuthStore>>,
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl<S, B, RB> Service<http::Request<B>> for Authorized<S>
where
    S: Service<http::Request<B>, Response = http::Response<RB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
    RB: Default,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<S::Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let Some(store) = self.store.clone() else {
            return Box::pin(self.inner.call(request));
        };
        // The service that was polled ready handles this request; the clone
        // stays for the next one.
        let clone = self.inner.clone();
        let mut inner = mem::replace(&mut self.inner, clone);
        let path = request.uri().path().to_owned();
        let token = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer)
            .map(str::to_owned);
        Box::pin(async move {
            match authorize(store, &path, token).await {
                Ok(principal) => {
                    if let Some(principal) = principal {
                        request.extensions_mut().insert(principal);
                    }
                    inner.call(request).await
                }
                Err(status) => Ok(status.into_http::<RB>()),
            }
        })
    }
}

/// The caller's principal, if the request may proceed. A public call
/// with a valid token still carries its principal.
async fn authorize(
    store: Arc<AuthStore>,
    path: &str,
    token: Option<String>,
) -> Result<Option<Principal>, Status> {
    let Some(required) = requirement(path) else {
        return Err(Status::permission_denied(format!(
            "{path}: not an RPC of hord.proto"
        )));
    };
    let principal = match &token {
        Some(token) => match store.authenticate_cached(token) {
            Some(principal) => Some(principal),
            // Not seen yet: another process may have issued it.
            None => {
                let token = token.clone();
                tokio::task::spawn_blocking(move || store.authenticate(&token))
                    .await
                    .map_err(|err| Status::internal(err.to_string()))?
                    .map_err(|err| Status::internal(err.to_string()))?
            }
        },
        None => None,
    };
    if required == Requirement::Public {
        return Ok(principal);
    }
    let principal = match (token, principal) {
        (None, _) => {
            return Err(Status::unauthenticated(
                "this server requires a bearer token: run `hord login <remote>`",
            ));
        }
        (Some(_), None) => {
            return Err(Status::unauthenticated(
                "unknown bearer token: run `hord login <remote>` again",
            ));
        }
        (Some(_), Some(principal)) => principal,
    };
    if !required.admits(&principal.scopes) {
        let held: Vec<String> = principal.scopes.iter().map(ToString::to_string).collect();
        return Err(Status::permission_denied(format!(
            "{path} requires {required}; the token of {} has [{}]",
            principal.actor.id(),
            held.join(", ")
        )));
    }
    Ok(Some(principal))
}
