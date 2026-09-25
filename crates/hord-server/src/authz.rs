//! The authorization layer (spec §10.5.4): a tower layer in front of every
//! route that reads the bearer token, checks it against the auth file, and
//! enforces the scope [`hord_api::auth::requirement`] names for the RPC.
//!
//! It runs after the `/r/<name>/` prefix is stripped, so it sees gRPC paths.
//! Any other path is the web UI's (ADR 0030): the layer only identifies the
//! caller there, from the bearer header or the UI's sign-in cookie, and the
//! UI checks each RPC it makes in process against the same requirements
//! (`crate::ui`).
//! The caller's [`Principal`] goes into the request's extensions, where
//! the services read it (provenance and evidence checks). A server without
//! an auth file passes everything through, with no principal.

use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use cookie::Cookie;
use hord_api::auth::{AUTHORIZATION, Requirement, bearer, requirement};
use hord_ui::UI_TOKEN_COOKIE;
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
            .map(str::to_owned)
            .or_else(|| {
                ui_path(&path)
                    .then(|| ui_cookie(request.headers()))
                    .flatten()
            });
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

/// Whether `path` is a web UI route, not a gRPC method (`/hord.v1.…`).
fn ui_path(path: &str) -> bool {
    !path.starts_with("/hord.")
}

/// The token in the UI's sign-in cookie, if any. Read on UI paths only, so
/// a cookie never authorizes a gRPC call.
fn ui_cookie(headers: &http::HeaderMap) -> Option<String> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(Cookie::split_parse)
        .filter_map(Result::ok)
        .find(|c| c.name() == UI_TOKEN_COOKIE)
        .map(|c| c.value().to_owned())
        .filter(|t| !t.is_empty())
}

/// The caller's principal, if the request may proceed. A public call
/// with a valid token still carries its principal.
async fn authorize(
    store: Arc<AuthStore>,
    path: &str,
    token: Option<String>,
) -> Result<Option<Principal>, Status> {
    let required = match requirement(path) {
        Some(required) => Some(required),
        // The UI identifies its caller and checks every RPC it makes.
        None if ui_path(path) => Some(Requirement::Public),
        None => None,
    };
    let Some(required) = required else {
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
