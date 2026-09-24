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

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp(tag: &str) -> Dir {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-remote-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    Dir(path)
}

fn actor(id: &str) -> Actor {
    Actor::Human { id: id.into() }
}

fn intent(summary: &str) -> Intent {
    Intent {
        summary: summary.into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    }
}

fn path(p: &str) -> RepoPath {
    p.parse().unwrap()
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

async fn serve(hosts: Hosts, config: ServerConfig) -> Running {
    let listener = Server::bind("127.0.0.1:0".parse().unwrap(), &ServeOptions::default())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = Server::new(hosts, config);
    let task = tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    Running {
        addr,
        stop: Some(stop),
        task: Some(task),
    }
}

async fn empty_repo(dir: &Path) {
    drop(Repo::create(dir).await.unwrap());
}

const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n";

/// A repository with `src/lib.rs` and a README landed, closed.
async fn seeded(dir: &Path) {
    let repo = Repo::create(dir).await.unwrap();
    repo.bootstrap(
        vec![
            (path("src/lib.rs"), LIB.as_bytes().to_vec()),
            (path("README.md"), b"# hi\n".to_vec()),
            (path("docs/notes.txt"), b"notes\n".to_vec()),
        ],
        intent("seed"),
        actor("seed"),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_repo_passes_the_conformance_suite_over_tcp() {
    let dir = temp("conf-tcp");
    empty_repo(&dir.0).await;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default())
        .await
        .unwrap();
    let running = serve(hosts, ServerConfig::default()).await;
    let remote = RemoteRepo::connect(&running.url()).await.unwrap();
    hord_api::conformance::run(&remote).await;
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_server_routes_by_repo_prefix() {
    let root = temp("conf-root");
    empty_repo(&root.0.join("team/app")).await;
    empty_repo(&root.0.join("other")).await;
    let hosts = Hosts::open_root(&root.0, RepoOptions::default)
        .await
        .unwrap();
    assert_eq!(hosts.names().collect::<Vec<_>>(), ["other", "team/app"]);
    let running = serve(hosts, ServerConfig::default()).await;
    let app = RemoteRepo::connect(&format!("{}/r/team/app", running.url()))
        .await
        .unwrap();
    hord_api::conformance::run(&app).await;
    // The other repository saw none of it.
    let other = RemoteRepo::connect(&format!("{}/r/other", running.url()))
        .await
        .unwrap();
    let head = other.head(proto::HeadRequest {}).await.unwrap();
    assert_eq!(head.change, None);
    // Without a prefix, a root server cannot tell which repository is meant.
    let bare = RemoteRepo::connect(&running.url()).await.unwrap();
    let err = bare.head(proto::HeadRequest {}).await.unwrap_err();
    assert!(
        matches!(err, hord_api::ApiError::InvalidArgument(_)),
        "{err}"
    );
    let missing = RemoteRepo::connect(&format!("{}/r/nope", running.url()))
        .await
        .unwrap();
    let err = missing.head(proto::HeadRequest {}).await.unwrap_err();
    assert!(matches!(err, hord_api::ApiError::NotFound(_)), "{err}");
    running.stop().await;
}

/// ADR 0021's daemon transport: a Unix socket, or a named pipe on Windows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_repo_passes_the_conformance_suite_over_the_local_endpoint() {
    let dir = temp("conf-local");
    empty_repo(&dir.0).await;
    let endpoint = hord_api::local::endpoint(&dir.0).unwrap();
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default())
        .await
        .unwrap();
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
                .unwrap();
        })
    };
    let remote = connect_local_retrying(&dir.0).await;
    // A second server on the same endpoint is refused.
    let second = Server::new(
        Hosts::from_backends(BTreeMap::new()),
        ServerConfig::default(),
    );
    let err = second.serve_local(&endpoint, async {}).await.unwrap_err();
    assert!(err.to_string().contains("already"), "{err}");
    hord_api::conformance::run(&remote).await;
    let _ = stop.send(());
    serving.await.unwrap();
}

async fn connect_local_retrying(root: &Path) -> RemoteRepo {
    for _ in 0..100 {
        if let Ok(remote) = RemoteRepo::connect_local(root).await
            && remote.head(proto::HeadRequest {}).await.is_ok()
        {
            return remote;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the local server never answered");
}

/// A client-side cache repository reads the server's snapshot lazily,
/// proposes locally, pushes only the new objects, and submits; the
/// server's lander lands it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_workspace_fetches_lazily_pushes_and_lands() {
    let origin = temp("origin");
    seeded(&origin.0).await;
    let hosts = Hosts::open_repo(
        &origin.0,
        RepoOptions {
            verifier: Some(Arc::new(hord_txn::StubVerifier)),
            ..RepoOptions::default()
        },
    )
    .await
    .unwrap();
    let running = serve(hosts, ServerConfig::default()).await;
    let remote = RemoteRepo::connect(&running.url()).await.unwrap();
    let head = remote.head(proto::HeadRequest {}).await.unwrap();
    let head = wire::object_id("head", head.change.as_deref().unwrap()).unwrap();

    let client = temp("client");
    let cache = open_cache(&client.0, remote.clone(), RepoOptions::default())
        .await
        .unwrap();
    let mut ws = cache
        .begin(BeginOptions {
            base: Base::Change(head),
            ..BeginOptions::at_head(actor("remote-agent"))
        })
        .await
        .unwrap();
    let lib = ws.read_file(&path("src/lib.rs")).await.unwrap().unwrap();
    assert_eq!(lib.as_slice(), LIB.as_bytes());
    let base_snapshot = ws.base();
    // Only what was touched came over: not the README's blob.
    let readme_blob = hord_core::ObjectId::of(&hord_core::Blob::new(b"# hi\n".to_vec())).unwrap();
    assert!(!cache.store().contains(readme_blob).unwrap(), "lazy fetch");
    ws.write_file(&path("src/lib.rs"), LIB.replace("    2\n", "    20\n"))
        .await
        .unwrap();
    let proposal = ws.propose(intent("twenty")).await.unwrap();
    assert_eq!(proposal.record.base, base_snapshot);
    assert_eq!(proposal.record.parents, vec![head]);

    let sent = push_change(&remote, &cache, proposal.change).await.unwrap();
    assert!(sent >= 3, "record, snapshot, trees, blob: {sent}");
    // Pushing again sends nothing new except what the server still lacks.
    assert_eq!(
        push_change(&remote, &cache, proposal.change).await.unwrap(),
        0
    );

    let mut events = remote
        .events(proto::EventsRequest { from: None })
        .await
        .unwrap();
    remote
        .submit(proto::SubmitRequest {
            change: wire::id(proposal.change),
        })
        .await
        .unwrap();
    let change = wire::id(proposal.change);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await
            .expect("an event")
            .unwrap()
            .unwrap();
        match event.event.unwrap().kind.unwrap() {
            Kind::Landed(l) if l.change == change => break,
            Kind::Rejected(r) if r.change == change => panic!("rejected: {}", r.reason),
            Kind::Parked(p) if p.change == change => panic!("parked: {}", p.detail),
            _ => {}
        }
    }
    let now = remote.head(proto::HeadRequest {}).await.unwrap();
    assert_eq!(now.change.as_deref(), Some(change.as_str()));

    // A directory workspace on the new head writes its checkout from the
    // server's objects, fetched in batches.
    let mut dir_ws = cache
        .begin_directory(BeginOptions {
            base: Base::Change(proposal.change),
            ..BeginOptions::at_head(actor("remote-agent"))
        })
        .await
        .unwrap();
    let hord_txn::Materialization::Directory { path: checkout } = dir_ws.materialization().clone()
    else {
        panic!("directory workspace");
    };
    assert_eq!(
        std::fs::read_to_string(checkout.join("README.md")).unwrap(),
        "# hi\n"
    );
    assert!(cache.store().contains(readme_blob).unwrap());
    let text = dir_ws
        .read_file(&path("src/lib.rs"))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(text.as_slice()).contains("20"));
    drop(dir_ws);
    drop(ws);
    drop(cache);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_schema_is_served_by_rpc_and_at_schema_json() {
    let dir = temp("schema");
    empty_repo(&dir.0).await;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default())
        .await
        .unwrap();
    let running = serve(hosts, ServerConfig::default()).await;
    let mut client = hord_api::proto::schema_client::SchemaClient::connect(running.url())
        .await
        .unwrap();
    let reply = client
        .get_schema(proto::GetSchemaRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reply.descriptor_set, hord_api::schema::descriptor_set());
    let rpc_schema: serde_json::Value = serde_json::from_str(&reply.json_schema).unwrap();
    let (status, body) = http1(running.addr, "GET", "/schema.json", &[], Vec::new()).await;
    assert_eq!(status, 200);
    let http_schema: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(http_schema, rpc_schema);
    assert!(http_schema["$defs"]["hord.v1.EventEnvelope"].is_object());
    running.stop().await;
}

/// gRPC-Web on the same port (ADR 0024): an HTTP/1.1 POST of a framed
/// `HeadRequest` answers with a framed `HeadResponse` and OK trailers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_web_is_served_on_the_same_port() {
    let dir = temp("grpc-web");
    seeded(&dir.0).await;
    let hosts = Hosts::open_repo(&dir.0, RepoOptions::default())
        .await
        .unwrap();
    let running = serve(hosts, ServerConfig::default()).await;
    // Frame: flag 0, length 0 (an empty HeadRequest).
    let body = vec![0, 0, 0, 0, 0];
    let (status, reply) = http1(
        running.addr,
        "POST",
        "/hord.v1.RepoBackend/Head",
        &[("content-type", "application/grpc-web+proto")],
        body,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(reply[0], 0, "first frame is data");
    let len = u32::from_be_bytes(reply[1..5].try_into().unwrap()) as usize;
    let head = <proto::HeadResponse as prost::Message>::decode(&reply[5..5 + len]).unwrap();
    assert!(head.change.is_some(), "the seeded repository has a head");
    let trailers = String::from_utf8_lossy(&reply[5 + len..]);
    assert!(trailers.contains("grpc-status:0"), "{trailers}");
    running.stop().await;
}

/// Landed events are POSTed as JSON to a webhook that asks for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_receive_the_events_they_ask_for() {
    let received: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let sink = {
        let received = Arc::clone(&received);
        axum::Router::new().route(
            "/hook",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let received = Arc::clone(&received);
                    async move {
                        let kind = headers["x-hord-event"].to_str().unwrap().to_owned();
                        let json = serde_json::from_slice(&body).unwrap();
                        received.lock().unwrap().push((kind, json));
                        "ok"
                    }
                },
            ),
        )
    };
    let hook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_addr = hook_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(hook_listener, sink).await.unwrap() });

    let origin = temp("webhook");
    seeded(&origin.0).await;
    let hosts = Hosts::open_repo(&origin.0, RepoOptions::default())
        .await
        .unwrap();
    let config = ServerConfig {
        bind: None,
        webhooks: vec![WebhookConfig {
            url: format!("http://{hook_addr}/hook"),
            kinds: vec!["landed".into()],
            repos: vec![],
        }],
    };
    let running = serve(hosts, config).await;
    let remote = RemoteRepo::connect(&running.url()).await.unwrap();
    let head = remote
        .head(proto::HeadRequest {})
        .await
        .unwrap()
        .change
        .unwrap();
    let head = wire::object_id("head", &head).unwrap();
    let client = temp("webhook-client");
    let cache = open_cache(&client.0, remote.clone(), RepoOptions::default())
        .await
        .unwrap();
    let mut ws = cache
        .begin(BeginOptions {
            base: Base::Change(head),
            ..BeginOptions::at_head(actor("a"))
        })
        .await
        .unwrap();
    ws.write_file(&path("docs/notes.txt"), "more notes\n")
        .await
        .unwrap();
    let proposal = ws.propose(intent("notes")).await.unwrap();
    push_change(&remote, &cache, proposal.change).await.unwrap();
    // Give the webhook task time to subscribe before anything happens.
    tokio::time::sleep(Duration::from_millis(200)).await;
    remote
        .submit(proto::SubmitRequest {
            change: wire::id(proposal.change),
        })
        .await
        .unwrap();
    let mut waited = 0;
    while received.lock().unwrap().is_empty() && waited < 300 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    let got = received.lock().unwrap().clone();
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
}

/// A plain HTTP/1.1 request; returns the status and the whole body.
async fn http1(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, Vec<u8>) {
    use http_body_util::{BodyExt, Full};
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let mut request = http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string());
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = sender
        .send_request(request.body(Full::new(bytes::Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}
