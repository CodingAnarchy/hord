//! [`Server`]: the routes, and serving them on TCP or a local endpoint.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use hord_api::MAX_MESSAGE_BYTES;
use hord_api::proto::auth_server::AuthServer;
use hord_api::proto::changes_server::ChangesServer;
use hord_api::proto::repo_backend_server::RepoBackendServer;
use hord_api::proto::schema_server::SchemaServer;
use hord_api::proto::workspaces_server::WorkspacesServer;
use tokio::net::TcpListener;
use tonic::service::Routes;

use crate::auth::AuthStore;
use crate::auth_service::GrpcAuth;
use crate::authz::AuthLayer;
use crate::changes::GrpcChanges;
use crate::config::ServerConfig;
use crate::hosts::Hosts;
use crate::route::RepoPrefixLayer;
use crate::service::{GrpcRepoBackend, GrpcSchema};
use crate::ui::{HostedUi, UiSigner};
use crate::{Error, Result};

/// How long shutdown waits for open calls (such as event streams) to end.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often shutdown checks that the served routes released the hosts.
const RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// How long shutdown waits for request tasks to release the repositories
/// before it returns anyway (the store's lock timeout keeps a reopen safe).
const RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Options for [`Server::bind`].
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    /// Allow a non-loopback address (`--insecure-bind`).
    pub insecure_bind: bool,
}

/// Refuse a non-loopback `addr` unless `insecure` (ADR 0024: `hord serve`
/// binds loopback only in M4, which has no authentication).
pub fn check_bind(addr: SocketAddr, insecure: bool) -> Result<()> {
    if addr.ip().is_loopback() || insecure {
        Ok(())
    } else {
        Err(Error::InsecureBind(addr))
    }
}

/// A configured server: hosted repositories and `server.toml`.
pub struct Server {
    hosts: Arc<Hosts>,
    config: ServerConfig,
    workspaces: Option<Arc<dyn hord_api::WorkspacesBackend>>,
    activity: Arc<crate::activity::Activity>,
    auth: Option<Arc<AuthStore>>,
    ui_signer: Option<UiSigner>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("hosts", &self.hosts)
            .field("config", &self.config)
            .field("workspaces", &self.workspaces.is_some())
            .field("auth", &self.auth.as_ref().map(|a| a.path()))
            .field("ui_signer", &self.ui_signer)
            .finish()
    }
}

impl Server {
    /// Serve `hosts` with `config`.
    #[must_use]
    pub fn new(hosts: Hosts, config: ServerConfig) -> Self {
        Self {
            hosts: Arc::new(hosts),
            config,
            workspaces: None,
            activity: Arc::default(),
            auth: None,
            ui_signer: None,
        }
    }

    /// Sign reviews made in the web UI with `signer` (ADR 0030): the key of
    /// whoever runs the server. With auth, ingest accepts such a review
    /// only from a signed-in actor who is `signer`'s actor.
    #[must_use]
    pub fn with_ui_signer(mut self, signer: UiSigner) -> Self {
        self.ui_signer = Some(signer);
        self
    }

    /// Require a bearer token on every call but the public ones, checked
    /// against `auth`, and enforce each RPC's scope (spec §10.5.4).
    #[must_use]
    pub fn with_auth(mut self, auth: AuthStore) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    /// Also serve `hord.v1.Workspaces` over `workspaces`: a repository's
    /// daemon only (ADR 0024 amendment), never a server for true remotes.
    #[must_use]
    pub fn with_workspaces(mut self, workspaces: Arc<dyn hord_api::WorkspacesBackend>) -> Self {
        self.workspaces = Some(workspaces);
        self
    }

    /// Requests in flight and the time of the last one (a daemon exits
    /// when idle).
    #[must_use]
    pub fn activity(&self) -> Arc<crate::activity::Activity> {
        Arc::clone(&self.activity)
    }

    /// Bind a TCP listener at `addr`, checked with [`check_bind`].
    pub async fn bind(addr: SocketAddr, options: &ServeOptions) -> Result<TcpListener> {
        check_bind(addr, options.insecure_bind)?;
        Ok(TcpListener::bind(addr).await?)
    }

    /// Every route: the gRPC services, `GET /schema.json`, and the web UI
    /// (ADR 0030).
    #[must_use]
    pub fn routes(&self) -> Routes {
        let backend = RepoBackendServer::new(GrpcRepoBackend::new(
            Arc::clone(&self.hosts),
            self.auth.clone(),
        ))
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
        // gRPC-Web wraps only the gRPC services: tonic-web answers any
        // other HTTP/1 request with 400, which would hide `/schema.json`.
        let mut routes = Routes::new(backend)
            .add_service(SchemaServer::new(GrpcSchema))
            .add_service(AuthServer::new(GrpcAuth::new(self.auth.clone())))
            .add_service(ChangesServer::new(GrpcChanges::new(Arc::clone(
                &self.hosts,
            ))));
        if let Some(workspaces) = &self.workspaces {
            routes = routes.add_service(
                WorkspacesServer::new(crate::service::GrpcWorkspaces::new(Arc::clone(workspaces)))
                    .max_decoding_message_size(MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(MAX_MESSAGE_BYTES),
            );
        }
        let router = routes
            .into_axum_router()
            .layer(tonic_web::GrpcWebLayer::new())
            .route("/schema.json", get(schema_json))
            .merge(hord_ui::router(Arc::new(HostedUi::new(
                Arc::clone(&self.hosts),
                self.auth.clone(),
                self.ui_signer.clone(),
            ))));
        Routes::from(router)
    }

    /// Serve on `listener` until `shutdown` resolves, then stop the
    /// landers. Webhooks run meanwhile.
    ///
    /// Returns once nothing the server started holds a repository: event
    /// streams, webhooks, and landers have ended, replays and verification
    /// in progress are cancelled (nothing is recorded for them; the next
    /// start resumes), and the tasks the repositories spawned are done.
    /// Requests still open are waited for up to 10 s, then named on stderr.
    /// Dropping the [`Server`] then closes every repository, which may be
    /// reopened at once.
    pub async fn serve(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<()> {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        self.serve_incoming(incoming, shutdown).await
    }

    /// Serve on the local endpoint (a Unix socket or named pipe, see
    /// [`hord_api::local::endpoint`]) until `shutdown` resolves. Fails with
    /// `AddrInUse` when another server already listens there.
    pub async fn serve_local(
        &self,
        endpoint: &str,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<()> {
        let (incoming, _cleanup) = crate::local::listen(endpoint).await.map_err(|err| {
            if crate::local::in_use(&err) {
                std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("{endpoint}: a server is already listening ({err})"),
                )
                .into()
            } else {
                Error::Io(err)
            }
        })?;
        self.serve_incoming(incoming, shutdown).await
    }

    async fn serve_incoming<I, IO, IE>(
        &self,
        incoming: I,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<()>
    where
        I: tokio_stream::Stream<Item = std::result::Result<IO, IE>> + Send,
        IO: tokio::io::AsyncRead
            + tokio::io::AsyncWrite
            + tonic::transport::server::Connected
            + Unpin
            + Send
            + 'static,
        IE: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let hooks: Vec<_> = self
            .hosts
            .backends()
            .iter()
            .map(|(name, backend)| {
                tokio::spawn(crate::webhook::deliver(
                    name.clone(),
                    Arc::clone(backend),
                    self.config.webhooks.clone(),
                ))
            })
            .collect();
        let (stop, mut stopped) = tokio::sync::watch::channel(());
        let served = {
            let serving = tonic::transport::Server::builder()
                .accept_http1(true)
                .layer(crate::activity::ActivityLayer(Arc::clone(&self.activity)))
                .layer(RepoPrefixLayer::new(self.hosts.names().map(str::to_owned)))
                .layer(AuthLayer(self.auth.clone()))
                .add_routes(self.routes())
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = stopped.changed().await;
                });
            tokio::pin!(serving);
            tokio::select! {
                served = &mut serving => served,
                () = shutdown => {
                    let _ = stop.send(());
                    // Graceful shutdown waits for every connection to close. A
                    // live event stream (gRPC `Events`, the UI's SSE relay)
                    // never ends by itself: end them all first, so the drain
                    // finishes. The grace period only bounds a client that keeps
                    // a request open.
                    self.hosts.close_events().await;
                    tokio::time::timeout(SHUTDOWN_GRACE, &mut serving)
                        .await
                        .unwrap_or(Ok(()))
                }
            }
        };
        // Nothing may hold a repository once this returns, so a caller can
        // reopen it at once: the webhook tasks, the landers and every task
        // the repositories spawned, and then the services' own handles.
        for hook in hooks {
            hook.abort();
            let _ = hook.await;
        }
        self.hosts.shutdown().await;
        self.released().await;
        Ok(served?)
    }

    /// Wait until the served routes (each service holds the hosts) are
    /// gone, which happens once every connection task has ended. After
    /// [`RELEASE_WAIT`], say which repositories requests still hold, and
    /// return.
    async fn released(&self) {
        let deadline = tokio::time::Instant::now() + RELEASE_WAIT;
        while Arc::strong_count(&self.hosts) > 1 {
            if tokio::time::Instant::now() >= deadline {
                let held: Vec<&str> = self.hosts.names().collect();
                eprintln!(
                    "hord serve: stopped with requests still holding {} after {}s",
                    held.join(", "),
                    RELEASE_WAIT.as_secs()
                );
                return;
            }
            tokio::time::sleep(RELEASE_POLL).await;
        }
    }
}

async fn schema_json() -> impl axum::response::IntoResponse {
    (
        [(http::header::CONTENT_TYPE, "application/schema+json")],
        hord_api::schema::json_schema().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_binds_without_the_flag() -> Result<(), Box<dyn std::error::Error>> {
        let local: SocketAddr = "127.0.0.1:0".parse()?;
        let v6: SocketAddr = "[::1]:0".parse()?;
        let any: SocketAddr = "0.0.0.0:0".parse()?;
        assert!(check_bind(local, false).is_ok());
        assert!(check_bind(v6, false).is_ok());
        assert!(matches!(
            check_bind(any, false),
            Err(Error::InsecureBind(_))
        ));
        assert!(check_bind(any, true).is_ok());
        Ok(())
    }
}
