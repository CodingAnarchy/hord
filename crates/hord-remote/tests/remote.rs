//! `RemoteRepo` against an in-process `hord serve` (ADR 0024): the
//! conformance suite over TCP, over a `/r/<name>/` prefix, and over the
//! local endpoint; lazy remote workspaces and `push_change`; the schema
//! endpoints; gRPC-Web; and webhooks.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hord_api::proto::event::Kind;
use hord_api::{RepoBackend, proto, wire};
use hord_core::{Actor, Intent, RepoPath};
use hord_remote::{RemoteRepo, open_cache, push_change};
use hord_server::{Hosts, ServeOptions, Server, ServerConfig, WebhookConfig};
use hord_txn::{Base, BeginOptions, Repo, RepoOptions};
use tokio_stream::StreamExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Dir(PathBuf);

impl Drop for Dir {
    /// A drop cannot return an error, and panicking there could abort a
    /// failing test and lose its failure, so a failed removal is reported
    /// on stderr; `temp` fails loudly if the leftover is still there next
    /// time.
    fn drop(&mut self) {
        if let Err(err) = remove_tree(&self.0) {
            eprintln!("remove temp dir {}: {err}", self.0.display());
        }
    }
}

/// Remove `path` and everything under it, if it exists. Directory
/// workspaces keep a read-only pristine checkout under `.hord/pristine/`
/// (ADR 0016), which `fs::remove_dir_all` alone cannot empty, so write
/// access is restored to every directory first.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(());
    }
    make_writable(path)?;
    std::fs::remove_dir_all(path)
}

fn make_writable(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

fn temp(tag: &str) -> std::io::Result<Dir> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-remote-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    // Paths repeat when the OS reuses a pid: clear a stale leftover, or
    // fail here rather than collide later.
    remove_tree(&path)?;
    std::fs::create_dir_all(&path)?;
    Ok(Dir(path))
}

fn actor(id: &str) -> Actor {
    Actor::Human { id: id.into() }
}

fn intent(summary: &str) -> Intent {
    Intent::from_summary(summary)
}

fn path(p: &str) -> TestResult<RepoPath> {
    Ok(p.parse()?)
}

/// A running server: its address and the handle that stops it.
struct Running {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Running {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

async fn serve(hosts: Hosts, config: ServerConfig) -> TestResult<Running> {
    let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
    let addr = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = Server::new(hosts, config);
    let task = tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .expect("serve the test server");
    });
    Ok(Running {
        addr,
        stop: Some(stop),
        task: Some(task),
    })
}

async fn empty_repo(dir: &Path) -> TestResult {
    drop(Repo::create(dir).await?);
    Ok(())
}

const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n";

/// A repository with `src/lib.rs` and a README landed, closed.
async fn seeded(dir: &Path) -> TestResult {
    let repo = Repo::create(dir).await?;
    repo.bootstrap(
        vec![
            (path("src/lib.rs")?, LIB.as_bytes().to_vec()),
            (path("README.md")?, b"# hi\n".to_vec()),
            (path("docs/notes.txt")?, b"notes\n".to_vec()),
        ],
        intent("seed"),
        actor("seed"),
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_repo_passes_the_conformance_suite_over_tcp() -> TestResult {
    let dir = temp("conf-tcp")?;
    empty_repo(&dir.0).await?;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default()).await?;
    let running = serve(hosts, ServerConfig::default()).await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    hord_api::conformance::run(&remote).await?;
    running.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_server_routes_by_repo_prefix() -> TestResult {
    let root = temp("conf-root")?;
    empty_repo(&root.0.join("team/app")).await?;
    empty_repo(&root.0.join("other")).await?;
    let hosts = Hosts::open_root(&root.0, |_| Ok(RepoOptions::default())).await?;
    assert_eq!(hosts.names().collect::<Vec<_>>(), ["other", "team/app"]);
    let running = serve(hosts, ServerConfig::default()).await?;
    let app = RemoteRepo::connect(&format!("{}/r/team/app", running.url())).await?;
    hord_api::conformance::run(&app).await?;
    // The other repository saw none of it.
    let other = RemoteRepo::connect(&format!("{}/r/other", running.url())).await?;
    let head = other.head(proto::HeadRequest {}).await?;
    assert_eq!(head.change, None);
    // Without a prefix, a root server cannot tell which repository is meant.
    let bare = RemoteRepo::connect(&running.url()).await?;
    let err = bare
        .head(proto::HeadRequest {})
        .await
        .expect_err("a root server refuses a request without a repo prefix");
    assert!(
        matches!(err, hord_api::ApiError::InvalidArgument(_)),
        "{err}"
    );
    let missing = RemoteRepo::connect(&format!("{}/r/nope", running.url())).await?;
    let err = missing
        .head(proto::HeadRequest {})
        .await
        .expect_err("a root server refuses an unknown repo");
    assert!(matches!(err, hord_api::ApiError::NotFound(_)), "{err}");
    running.stop().await;
    Ok(())
}

/// ADR 0021's daemon transport: a Unix socket, or a named pipe on Windows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_repo_passes_the_conformance_suite_over_the_local_endpoint() -> TestResult {
    let dir = temp("conf-local")?;
    empty_repo(&dir.0).await?;
    let endpoint = hord_api::local::endpoint(&dir.0)?;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default()).await?;
    let server = Arc::new(Server::new(hosts, ServerConfig::default()));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = {
        let server = Arc::clone(&server);
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            server
                .serve_local(&endpoint, async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve on the local endpoint");
        })
    };
    let remote = connect_local_retrying(&dir.0).await?;
    // A second server on the same endpoint is refused.
    let second = Server::new(
        Hosts::from_backends(BTreeMap::new()),
        ServerConfig::default(),
    );
    let err = second
        .serve_local(&endpoint, async {})
        .await
        .expect_err("a second server on the same endpoint is refused");
    assert!(err.to_string().contains("already"), "{err}");
    hord_api::conformance::run(&remote).await?;
    let _ = stop.send(());
    serving.await?;
    Ok(())
}

async fn connect_local_retrying(root: &Path) -> TestResult<RemoteRepo> {
    for _ in 0..100 {
        if let Ok(remote) = RemoteRepo::connect_local(root).await
            && remote.head(proto::HeadRequest {}).await.is_ok()
        {
            return Ok(remote);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err("the local server never answered".into())
}

/// A client-side cache repository reads the server's snapshot lazily,
/// proposes locally, pushes only the new objects, and submits; the
/// server's lander lands it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_workspace_fetches_lazily_pushes_and_lands() -> TestResult {
    let origin = temp("origin")?;
    seeded(&origin.0).await?;
    let hosts = Hosts::open_repo(
        &origin.0,
        RepoOptions {
            verifier: Some(Arc::new(hord_txn::StubVerifier)),
            ..RepoOptions::default()
        },
    )
    .await?;
    let running = serve(hosts, ServerConfig::default()).await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    let head = remote.head(proto::HeadRequest {}).await?;
    let head = wire::object_id(
        "head",
        head.change
            .as_deref()
            .ok_or("the seeded repository has a head")?,
    )?;

    let client = temp("client")?;
    let cache = open_cache(&client.0, remote.clone(), RepoOptions::default()).await?;
    let mut ws = cache
        .begin(BeginOptions {
            base: Base::Change(head),
            ..BeginOptions::at_head(actor("remote-agent"))
        })
        .await?;
    let lib = ws
        .read_file(&path("src/lib.rs")?)
        .await?
        .ok_or("src/lib.rs exists")?;
    assert_eq!(lib.as_slice(), LIB.as_bytes());
    let base_snapshot = ws.base();
    // Only what was touched came over: not the README's blob.
    let readme_blob = hord_core::ObjectId::of(&hord_core::Blob::new(b"# hi\n".to_vec()))?;
    assert!(!cache.store().contains(readme_blob)?, "lazy fetch");
    ws.write_file(&path("src/lib.rs")?, LIB.replace("    2\n", "    20\n"))
        .await?;
    let proposal = ws.propose(intent("twenty")).await?;
    assert_eq!(proposal.record.base, base_snapshot);
    assert_eq!(proposal.record.parents, vec![head]);

    let sent = push_change(&remote, &cache, proposal.change).await?;
    assert!(sent >= 3, "record, snapshot, trees, blob: {sent}");
    // Pushing again sends nothing new except what the server still lacks.
    assert_eq!(push_change(&remote, &cache, proposal.change).await?, 0);

    let mut events = remote.events(proto::EventsRequest { from: None }).await?;
    remote
        .submit(proto::SubmitRequest {
            change: wire::id(proposal.change),
        })
        .await?;
    let change = wire::id(proposal.change);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await
            .map_err(|e| format!("wait for {change} to land: {e}"))?
            .ok_or("the event stream ended")??;
        match event
            .event
            .ok_or("an event envelope")?
            .kind
            .ok_or("an event kind")?
        {
            Kind::Landed(l) if l.change == change => break,
            Kind::Rejected(r) if r.change == change => {
                return Err(format!("rejected: {}", r.reason).into());
            }
            Kind::Parked(p) if p.change == change => {
                return Err(format!("parked: {}", p.detail).into());
            }
            _ => {}
        }
    }
    let now = remote.head(proto::HeadRequest {}).await?;
    assert_eq!(now.change.as_deref(), Some(change.as_str()));

    // A directory workspace on the new head writes its checkout from the
    // server's objects, fetched in batches.
    let mut dir_ws = cache
        .begin_directory(BeginOptions {
            base: Base::Change(proposal.change),
            ..BeginOptions::at_head(actor("remote-agent"))
        })
        .await?;
    let hord_txn::Materialization::Directory { path: checkout } = dir_ws.materialization().clone()
    else {
        return Err("expected a directory workspace".into());
    };
    assert_eq!(
        std::fs::read_to_string(checkout.join("README.md"))?,
        "# hi\n"
    );
    assert!(cache.store().contains(readme_blob)?);
    let text = dir_ws
        .read_file(&path("src/lib.rs")?)
        .await?
        .ok_or("src/lib.rs exists")?;
    assert!(String::from_utf8_lossy(text.as_slice()).contains("20"));
    drop(dir_ws);
    drop(ws);
    drop(cache);
    running.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_schema_is_served_by_rpc_and_at_schema_json() -> TestResult {
    let dir = temp("schema")?;
    empty_repo(&dir.0).await?;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default()).await?;
    let running = serve(hosts, ServerConfig::default()).await?;
    let mut client = hord_api::proto::schema_client::SchemaClient::connect(running.url()).await?;
    let reply = client
        .get_schema(proto::GetSchemaRequest {})
        .await?
        .into_inner();
    assert_eq!(reply.descriptor_set, hord_api::schema::descriptor_set());
    let rpc_schema: serde_json::Value = serde_json::from_str(&reply.json_schema)?;
    let (status, body) = http1(running.addr, "GET", "/schema.json", &[], Vec::new()).await?;
    assert_eq!(status, 200);
    let http_schema: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(http_schema, rpc_schema);
    assert!(http_schema["$defs"]["hord.v1.EventEnvelope"].is_object());
    running.stop().await;
    Ok(())
}

/// gRPC-Web on the same port (ADR 0024): an HTTP/1.1 POST of a framed
/// `HeadRequest` answers with a framed `HeadResponse` and OK trailers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_web_is_served_on_the_same_port() -> TestResult {
    let dir = temp("grpc-web")?;
    seeded(&dir.0).await?;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default()).await?;
    let running = serve(hosts, ServerConfig::default()).await?;
    // Frame: flag 0, length 0 (an empty HeadRequest).
    let body = vec![0, 0, 0, 0, 0];
    let (status, reply) = http1(
        running.addr,
        "POST",
        "/hord.v1.RepoBackend/Head",
        &[("content-type", "application/grpc-web+proto")],
        body,
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(reply[0], 0, "first frame is data");
    let len = u32::from_be_bytes(reply[1..5].try_into()?) as usize;
    let head = <proto::HeadResponse as prost::Message>::decode(&reply[5..5 + len])?;
    assert!(head.change.is_some(), "the seeded repository has a head");
    let trailers = String::from_utf8_lossy(&reply[5 + len..]);
    assert!(trailers.contains("grpc-status:0"), "{trailers}");
    running.stop().await;
    Ok(())
}

/// Landed events are POSTed as JSON to a webhook that asks for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_receive_the_events_they_ask_for() -> TestResult {
    let received: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let sink = {
        let received = Arc::clone(&received);
        axum::Router::new().route(
            "/hook",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let received = Arc::clone(&received);
                    async move {
                        let kind = headers["x-hord-event"]
                            .to_str()
                            .expect("x-hord-event header is ASCII")
                            .to_owned();
                        let json = serde_json::from_slice(&body).expect("webhook body is JSON");
                        received
                            .lock()
                            .expect("lock received webhooks")
                            .push((kind, json));
                        "ok"
                    }
                },
            ),
        )
    };
    let hook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let hook_addr = hook_listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(hook_listener, sink)
            .await
            .expect("serve the webhook sink")
    });

    let origin = temp("webhook")?;
    seeded(&origin.0).await?;
    let hosts = Hosts::open_repo(&origin.0, RepoOptions::default()).await?;
    let config = ServerConfig {
        bind: None,
        webhooks: vec![WebhookConfig {
            url: format!("http://{hook_addr}/hook"),
            kinds: vec!["landed".into()],
            repos: vec![],
        }],
        auth: None,
        tls: None,
    };
    let running = serve(hosts, config).await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    let head = remote
        .head(proto::HeadRequest {})
        .await?
        .change
        .ok_or("the seeded repository has a head")?;
    let head = wire::object_id("head", &head)?;
    let client = temp("webhook-client")?;
    let cache = open_cache(&client.0, remote.clone(), RepoOptions::default()).await?;
    let mut ws = cache
        .begin(BeginOptions {
            base: Base::Change(head),
            ..BeginOptions::at_head(actor("a"))
        })
        .await?;
    ws.write_file(&path("docs/notes.txt")?, "more notes\n")
        .await?;
    let proposal = ws.propose(intent("notes")).await?;
    push_change(&remote, &cache, proposal.change).await?;
    // Give the webhook task time to subscribe before anything happens.
    tokio::time::sleep(Duration::from_millis(200)).await;
    remote
        .submit(proto::SubmitRequest {
            change: wire::id(proposal.change),
        })
        .await?;
    let mut waited = 0;
    while received.lock().expect("lock received webhooks").is_empty() && waited < 300 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    let got = received.lock().expect("lock received webhooks").clone();
    assert_eq!(got.len(), 1, "only the landed event: {got:?}");
    assert_eq!(got[0].0, "landed");
    assert_eq!(
        got[0].1["event"]["landed"]["change"],
        wire::id(proposal.change)
    );
    assert!(got[0].1["cursor"].is_string(), "uint64 maps to a string");
    drop(ws);
    drop(cache);
    running.stop().await;
    Ok(())
}

/// A plain HTTP/1.1 request; returns the status and the whole body.
async fn http1(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> TestResult<(u16, Vec<u8>)> {
    use http_body_util::{BodyExt, Full};
    let stream = tokio::net::TcpStream::connect(addr).await?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let mut request = http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string());
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = sender
        .send_request(request.body(Full::new(bytes::Bytes::from(body)))?)
        .await?;
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok((status, bytes.to_vec()))
}
