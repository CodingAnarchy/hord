//! [`Server`]: the routes, and serving them on TCP or a local endpoint.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use axum::routing::get;
use hord_api::MAX_MESSAGE_BYTES;
use hord_api::proto::audit_server::AuditServer;
use hord_api::proto::auth_server::AuthServer;
use hord_api::proto::changes_server::ChangesServer;
use hord_api::proto::repo_backend_server::RepoBackendServer;
use hord_api::proto::schema_server::SchemaServer;
use hord_api::proto::workspaces_server::WorkspacesServer;
use tokio::net::TcpListener;
use tonic::service::Routes;
use tonic::transport::{Identity, ServerTlsConfig};

use crate::audit::GrpcAudit;
use crate::auth::AuthStore;
use crate::auth_service::GrpcAuth;
use crate::authz::AuthLayer;
use crate::changes::GrpcChanges;
use crate::config::{ServerConfig, TlsConfig};
use crate::hosts::Hosts;
use crate::route::RepoPrefixLayer;
use crate::service::{GrpcRepoBackend, GrpcSchema};
use crate::ui::{HostedUi, UiSigner};
use crate::{Error, Result};

/// How long shutdown waits for open calls (such as event streams) to end.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often a serving server checks whether its auth file changed.
const AUTH_RELOAD_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// How often shutdown checks that the served routes released the hosts.
const RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// How long shutdown waits for request tasks to release the repositories
/// before it returns anyway (the store's lock timeout keeps a reopen safe).
const RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Options for [`Server::bind`].
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    /// Allow a non-loopback plaintext address (`--insecure-bind`).
    pub insecure_bind: bool,
    /// The listener will serve TLS ([`Server::with_tls`]), so any address
    /// may be bound.
    pub tls: bool,
}

/// Refuse a non-loopback `addr` for plaintext unless `--insecure-bind`
/// (ADR 0024, ADR 0032): bearer tokens must not cross a network in the
/// clear. With TLS, any address is allowed.
pub fn check_bind(addr: SocketAddr, options: &ServeOptions) -> Result<()> {
    if addr.ip().is_loopback() || options.insecure_bind || options.tls {
        Ok(())
    } else {
        Err(Error::InsecureBind(addr))
    }
}

/// Read `tls`'s certificate chain and key into tonic's server TLS config,
/// checking that rustls accepts them (ADR 0032).
fn load_tls(tls: &TlsConfig) -> Result<ServerTlsConfig> {
    let read = |path: &std::path::Path| {
        std::fs::read(path).map_err(|err| Error::Tls {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })
    };
    let config =
        ServerTlsConfig::new().identity(Identity::from_pem(read(&tls.cert)?, read(&tls.key)?));
    // tonic parses the PEM files when the config is applied: do it now, so
    // a bad file fails at startup and names itself.
    tonic::transport::Server::builder()
        .tls_config(config.clone())
        .map_err(|err| Error::Tls {
            path: tls.cert.clone(),
            reason: format!("{err} (with key {})", tls.key.display()),
        })?;
    Ok(config)
}

/// A configured server: hosted repositories and `server.toml`.
pub struct Server {
    hosts: Arc<Hosts>,
    config: ServerConfig,
    workspaces: Option<Arc<dyn hord_api::WorkspacesBackend>>,
    activity: Arc<crate::activity::Activity>,
    auth: Option<Arc<AuthStore>>,
    ui_signer: Option<UiSigner>,
    tls: Option<ServerTlsConfig>,
    audit_keys: OnceLock<Option<Arc<AuthStore>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("hosts", &self.hosts)
            .field("config", &self.config)
            .field("workspaces", &self.workspaces.is_some())
            .field("auth", &self.auth.as_ref().map(|a| a.path()))
            .field("ui_signer", &self.ui_signer)
            .field("tls", &self.tls.is_some())
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
            tls: None,
            audit_keys: OnceLock::new(),
        }
    }

    /// Serve TLS on TCP listeners ([`Self::serve`]) with `tls`'s
    /// certificate chain and key (ADR 0032). gRPC, gRPC-Web and the web UI
    /// share the port as before; clients negotiate HTTP/2 by ALPN. The
    /// local endpoint ([`Self::serve_local`]) stays plaintext: it belongs
    /// to the server's OS user.
    pub fn with_tls(mut self, tls: &TlsConfig) -> Result<Self> {
        self.tls = Some(load_tls(tls)?);
        Ok(self)
    }

    /// The auth file tokens are checked against, if any: `hord serve`
    /// reloads it on SIGHUP ([`AuthStore::reload`]).
    #[must_use]
    pub fn auth_store(&self) -> Option<Arc<AuthStore>> {
        self.auth.clone()
    }

    /// Whether [`Self::serve`] serves TLS.
    #[must_use]
    pub fn tls(&self) -> bool {
        self.tls.is_some()
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
        check_bind(addr, options)?;
        Ok(TcpListener::bind(addr).await?)
    }

    /// The key bindings `hord audit` checks signatures against: the auth
    /// file tokens are checked against, else `server.toml`'s `[auth] file`
    /// (a daemon or local endpoint, which takes no tokens, still reads the
    /// bindings), else none.
    fn audit_keys(&self) -> Option<Arc<AuthStore>> {
        self.audit_keys
            .get_or_init(|| self.open_audit_keys())
            .clone()
    }

    fn open_audit_keys(&self) -> Option<Arc<AuthStore>> {
        self.auth.clone().or_else(|| {
            let file = &self.config.auth.as_ref()?.file;
            match AuthStore::open(file) {
                Ok(store) => Some(Arc::new(store)),
                Err(err) => {
                    eprintln!(
                        "hord serve: audit without key bindings: {}: {err}",
                        file.display()
                    );
                    None
                }
            }
        })
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
            ))))
            .add_service(AuditServer::new(GrpcAudit::new(
                Arc::clone(&self.hosts),
                self.audit_keys(),
            )));
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
        self.serve_incoming(incoming, self.tls.clone(), shutdown)
            .await
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
        self.serve_incoming(incoming, None, shutdown).await
    }

    async fn serve_incoming<I, IO, IE>(
        &self,
        incoming: I,
        tls: Option<ServerTlsConfig>,
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
        // Revoked tokens and keys stop working, and new ones start, without
        // a restart: reload the auth files when they change.
        let reloader = tokio::spawn(reload_auth(
            [self.auth.clone(), self.audit_keys()]
                .into_iter()
                .flatten()
                .collect(),
        ));
        let (stop, mut stopped) = tokio::sync::watch::channel(());
        let mut builder = tonic::transport::Server::builder();
        if let Some(tls) = tls {
            builder = builder.tls_config(tls)?;
        }
        let served = {
            let serving = builder
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
        reloader.abort();
        let _ = reloader.await;
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

/// Every [`AUTH_RELOAD_POLL`], reload each of `stores` whose file changed.
/// A file that does not parse (mid-edit) keeps what was read before and is
/// reported once until it parses again.
async fn reload_auth(mut stores: Vec<Arc<AuthStore>>) {
    stores.dedup_by(|a, b| Arc::ptr_eq(a, b));
    if stores.is_empty() {
        return;
    }
    let mut failing = vec![false; stores.len()];
    let mut tick = tokio::time::interval(AUTH_RELOAD_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        for (store, failing) in stores.iter().zip(&mut failing) {
            let store = Arc::clone(store);
            let reloaded = tokio::task::spawn_blocking(move || {
                let result = store.reload_if_changed();
                (store, result)
            })
            .await;
            match reloaded {
                Ok((store, Ok(changed))) => {
                    if changed {
                        eprintln!("hord serve: reloaded {}", store.path().display());
                    }
                    *failing = false;
                }
                Ok((store, Err(err))) if !*failing => {
                    eprintln!(
                        "hord serve: keeping the previous {}: {err}",
                        store.path().display()
                    );
                    *failing = true;
                }
                Ok(_) | Err(_) => {}
            }
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
        let plain = ServeOptions::default();
        assert!(check_bind(local, &plain).is_ok());
        assert!(check_bind(v6, &plain).is_ok());
        assert!(matches!(
            check_bind(any, &plain),
            Err(Error::InsecureBind(_))
        ));
        let insecure = ServeOptions {
            insecure_bind: true,
            tls: false,
        };
        assert!(check_bind(any, &insecure).is_ok());
        let tls = ServeOptions {
            insecure_bind: false,
            tls: true,
        };
        assert!(check_bind(any, &tls).is_ok());
        Ok(())
    }

    #[test]
    fn a_bad_or_missing_certificate_fails_when_loaded() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("hord-server-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let bad = TlsConfig {
            cert: dir.join("cert.pem"),
            key: dir.join("key.pem"),
        };
        std::fs::write(&bad.cert, "not a certificate")?;
        std::fs::write(&bad.key, "not a key")?;
        let loaded = load_tls(&bad);
        let missing = load_tls(&TlsConfig {
            cert: dir.join("nope.pem"),
            key: dir.join("nope.pem"),
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(loaded, Err(Error::Tls { .. })), "{loaded:?}");
        assert!(matches!(missing, Err(Error::Tls { .. })), "{missing:?}");
        Ok(())
    }
}
