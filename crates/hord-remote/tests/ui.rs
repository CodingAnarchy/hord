//! The web UI (ADR 0030) end to end: a real conflict landed and parked by
//! `hord serve`'s lander, its event stream recorded as a flight recording,
//! and every view rendered over the wire.
//!
//! - The UI router runs over `RemoteRepo` and `RemoteChanges`, wrapped in
//!   `hord_ui::audit::Audited`: the M5 check that the UI calls no RPC
//!   outside `hord.proto`.
//! - `hord serve` mounts the same router at `/` and `/r/<name>/`.
//! - The recording plays back to the end on the landing strip.

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::{self, Body};
use hord_api::auth::Scope;
use hord_api::proto::event::Kind;
use hord_api::recording::Recorder;
use hord_api::{ChangesBackend, RepoBackend, proto, wire};
use hord_core::Evidence;
use hord_core::sign::{self, SigningKey};
use hord_core::{Actor, Bytes, ChangeId, Intent, RepoPath};
use hord_remote::RemoteRepo;
use hord_server::{
    AuthStore, Hosts, Server, ServerConfig, UI_REVIEW_KIND, UiSigner, save_recording,
};
use hord_txn::{BeginOptions, ReplayFuture, ReplayHarness, Repo, RepoOptions};
use hord_ui::UI_TOKEN_COOKIE;
use hord_ui::audit::{AuditLog, Audited, unlisted};
use hord_ui::{SingleRepo, UiRepo};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::handshake;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tower::ServiceExt;

use common::{Dir, Running, TestResult, temp};

const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n";

fn agent(id: &str) -> Actor {
    Actor::Agent {
        id: id.into(),
        model: "test-model".into(),
        model_hash: Bytes::new(Vec::new()),
        harness: "ui-test".into(),
    }
}

fn path(p: &str) -> TestResult<RepoPath> {
    Ok(p.parse()?)
}

async fn serve(root: &Path) -> TestResult<Running> {
    serve_with(root, |server| server).await
}

async fn serve_with(root: &Path, setup: impl FnOnce(Server) -> Server) -> TestResult<Running> {
    serve_harness(root, None, setup).await
}

async fn serve_harness(
    root: &Path,
    harness: Option<Arc<dyn ReplayHarness>>,
    setup: impl FnOnce(Server) -> Server,
) -> TestResult<Running> {
    let hosts = Hosts::open_repo(
        root,
        RepoOptions {
            verifier: Some(Arc::new(hord_txn::StubVerifier)),
            harness,
            ..RepoOptions::default()
        },
    )
    .await?;
    Running::start(setup(Server::new(hosts, ServerConfig::default()))).await
}

/// The scenario: two agents change `two` from the same base; the first
/// lands, the second is parked on a hard merge conflict.
struct Scenario {
    dir: Dir,
    running: Running,
    landed: String,
    parked: String,
    recording: String,
}

async fn propose(repo: &Repo, who: &str, body: &str, summary: &str) -> TestResult<ChangeId> {
    let mut ws = repo.begin(BeginOptions::at_head(agent(who))).await?;
    ws.write_file(&path("src/lib.rs")?, LIB.replace("    2\n", body))
        .await?;
    let mut intent = Intent::from_summary(summary);
    intent.body = format!("{who} needs `two` to return a different value.");
    Ok(ws.propose(intent).await?.change)
}

fn names(envelope: &proto::EventEnvelope, change: &str) -> Option<&'static str> {
    match envelope.kind()? {
        Kind::Landed(l) if l.change == change || l.submitted.as_deref() == Some(change) => {
            Some("landed")
        }
        Kind::Parked(p) if p.change == change => Some("parked"),
        Kind::Rejected(r) if r.change == change => Some("rejected"),
        _ => None,
    }
}

async fn scenario() -> TestResult<Scenario> {
    let dir = temp("scenario")?;
    let (a, b) = {
        let repo = Repo::create(&dir.0).await?;
        repo.bootstrap(
            vec![(path("src/lib.rs")?, LIB.as_bytes().to_vec())],
            Intent::from_summary("seed"),
            Actor::Human { id: "seed".into() },
        )
        .await?;
        let a = propose(&repo, "agent-a", "    20\n", "Make two return twenty").await?;
        let b = propose(&repo, "agent-b", "    22\n", "Make two return twenty-two").await?;
        (wire::id(a), wire::id(b))
    };

    // Run the lander with a flight recorder on the event stream.
    let running = serve(&dir.0).await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    let mut stream = remote
        .events(proto::EventsRequest { from: Some(0) })
        .await?;
    let mut recorder = Recorder::new(
        Vec::new(),
        proto::RecordingHeader {
            repo: "ui-test".into(),
            description: "two agents, one conflict".into(),
            ..Default::default()
        },
    )?;
    remote
        .submit(proto::SubmitRequest { change: a.clone() })
        .await?;
    remote
        .submit(proto::SubmitRequest { change: b.clone() })
        .await?;
    let mut settled = (None, None);
    while settled.0.is_none() || settled.1.is_none() {
        let next = timeout(Duration::from_secs(60), stream.next())
            .await?
            .ok_or("the event stream ended")??;
        recorder.record(&next)?;
        settled.0 = settled.0.or(names(&next, &a));
        settled.1 = settled.1.or(names(&next, &b));
    }
    assert_eq!(settled, (Some("landed"), Some("parked")), "{a} then {b}");
    drop(stream);
    running.stop().await;

    // Store the recording in the repository, then serve it again.
    let recording = {
        let repo = Repo::open(&dir.0).await?;
        save_recording(&repo, recorder.finish()?)?
    };
    let running = serve(&dir.0).await?;
    Ok(Scenario {
        dir,
        running,
        landed: a,
        parked: b,
        recording: wire::id(recording),
    })
}

async fn get(router: &axum::Router, uri: &str) -> TestResult<(u16, String)> {
    let response = router
        .clone()
        .oneshot(http::Request::get(uri).body(Body::empty())?)
        .await?;
    let status = response.status().as_u16();
    let body = response.into_body().collect().await?.to_bytes();
    Ok((status, String::from_utf8(body.to_vec())?))
}

async fn post(router: &axum::Router, uri: &str, form: &str) -> TestResult<(u16, String)> {
    let response = router
        .clone()
        .oneshot(
            http::Request::post(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_owned()))?,
        )
        .await?;
    let status = response.status().as_u16();
    let body = response.into_body().collect().await?.to_bytes();
    Ok((status, String::from_utf8(body.to_vec())?))
}

fn has(page: &str, needle: &str) -> TestResult {
    if page.contains(needle) {
        Ok(())
    } else {
        Err(format!("page lacks {needle:?}:\n{page}").into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_view_renders_over_the_wire_and_calls_only_listed_rpcs() -> TestResult {
    let sc = scenario().await?;
    let remote = RemoteRepo::connect(&sc.running.url()).await?;
    let log = AuditLog::new();
    let router = hord_ui::router(Arc::new(SingleRepo(UiRepo {
        backend: Arc::new(Audited::new(Arc::new(remote.clone()), log.clone())),
        changes: Arc::new(Audited::new(Arc::new(remote.changes()), log.clone())),
        review: None,
        arbiter: None,
        name: "ui-test".into(),
        base: String::new(),
    })));

    // View 1: the strip, labeled by intent and actor, with both outcomes.
    let (status, strip) = get(&router, "/").await?;
    assert_eq!(status, 200, "{strip}");
    has(&strip, "Make two return twenty-two")?;
    has(&strip, "agent:agent-a (test-model)")?;
    has(&strip, "stage-landed")?;
    has(&strip, "collide with a landed change")?;
    has(&strip, &format!("/arbitrate/{}", sc.parked))?;

    // View 2: intent first, then ops on named definitions, evidence,
    // provenance, sets, history; the text diff is the secondary tab.
    let (status, change) = get(&router, &format!("/changes/{}", sc.landed)).await?;
    assert_eq!(status, 200, "{change}");
    has(&change, "Make two return twenty")?;
    has(&change, "agent-a needs `two` to return a different value.")?;
    has(&change, "replace two (src/lib.rs)")?;
    has(&change, "-    2\n+    20")?;
    has(&change, "harness")?;
    has(&change, "landed at #")?;
    has(&change, "Review signing is not available")?;
    let intent_at = change.find("Operations").ok_or("ops section")?;
    let diff_at = change.find("+    20").ok_or("diff")?;
    assert!(intent_at < diff_at, "the text diff is not the default view");

    // View 3: why it was parked, both intents, the contested definition
    // with both sides' ops, and the ladder.
    let (status, bench) = get(&router, &format!("/arbitrate/{}", sc.parked)).await?;
    assert_eq!(status, 200, "{bench}");
    has(&bench, "hard merge conflict")?;
    has(&bench, "Make two return twenty-two")?;
    has(&bench, "Make two return twenty<")?;
    has(&bench, "replace two (src/lib.rs)")?;
    has(&bench, "conflict check")?;
    // Actions go through Arbitrate; until the ladder ships it, the
    // workbench says so rather than failing.
    let (status, acted) = post(
        &router,
        &format!("/arbitrate/{}", sc.parked),
        "action=replay&note=keep+both",
    )
    .await?;
    assert_eq!(status, 200, "{acted}");
    has(&acted, "class=\"flash\"")?;
    let (_, ws) = post(
        &router,
        &format!("/arbitrate/{}", sc.parked),
        "action=workspace",
    )
    .await?;
    has(&ws, &format!("hord arbitrate {} --edit", sc.parked))?;

    // Playback: the recording is listed, and scrubbing to the end shows
    // what the live strip shows.
    let (_, list) = get(&router, "/recordings").await?;
    has(&list, "two agents, one conflict")?;
    let (status, player) = get(&router, &format!("/recordings/{}", sc.recording)).await?;
    assert_eq!(status, 200, "{player}");
    let total: usize = player
        .split("data-total=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .ok_or("data-total")?
        .parse()?;
    assert!(total >= 4, "{total} events");
    let (_, start) = get(
        &router,
        &format!("/recordings/{}/frames?at=0", sc.recording),
    )
    .await?;
    assert!(!start.contains("<li class=\"row"), "{start}");
    let end = router
        .clone()
        .oneshot(
            http::Request::get(format!(
                "/recordings/{}/frames?at={total}&speed=4",
                sc.recording
            ))
            .body(Body::empty())?,
        )
        .await?;
    assert!(end.headers().contains_key("hord-next-delay"));
    let end = String::from_utf8(end.into_body().collect().await?.to_bytes().to_vec())?;
    has(&end, "Make two return twenty-two")?;
    has(&end, "stage-landed")?;
    has(&end, "stage-arbitration")?;
    let (status, _) = get(
        &router,
        &format!("/recordings/{}/frames?at=1&speed=0", sc.recording),
    )
    .await?;
    assert_eq!(status, 400, "speed 0 is refused");

    // The M5 check: every call was an RPC of hord.proto, and the views
    // used both services.
    let calls = log.calls();
    assert!(unlisted(&log).is_empty(), "{:?}", unlisted(&log));
    for rpc in [
        "hord.v1.RepoBackend/Queue",
        "hord.v1.RepoBackend/Head",
        "hord.v1.RepoBackend/Arbitrate",
        "hord.v1.Changes/GetChange",
        "hord.v1.Changes/ChangeDiff",
        "hord.v1.Changes/ListRecordings",
        "hord.v1.Changes/GetRecording",
    ] {
        assert!(calls.iter().any(|c| c == rpc), "{rpc} in {calls:?}");
    }
    sc.running.stop().await;
    drop(sc.dir);
    Ok(())
}

/// A plain HTTP/1.1 GET on the server; with `first_frame`, returns only the
/// first body frame (for an SSE stream that never ends).
async fn http1(addr: SocketAddr, uri: &str, first_frame: bool) -> TestResult<(u16, String)> {
    let request = http::Request::get(uri)
        .header("host", addr.to_string())
        .body(Full::new(body::Bytes::new()))?;
    send(addr, request, first_frame)
        .await
        .map(|(status, _, text)| (status, text))
}

/// An HTTP/1.1 request with a cookie (and a form body, for a POST).
async fn with_cookie(
    addr: SocketAddr,
    method: &str,
    uri: &str,
    cookie: &str,
    form: &str,
) -> TestResult<(u16, http::HeaderMap, String)> {
    let request = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", addr.to_string())
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Full::new(body::Bytes::from(form.to_owned())))?;
    send(addr, request, false).await
}

async fn send(
    addr: SocketAddr,
    request: http::Request<Full<body::Bytes>>,
    first_frame: bool,
) -> TestResult<(u16, http::HeaderMap, String)> {
    let stream = TcpStream::connect(addr).await?;
    let (mut sender, conn) = handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let response = sender.send_request(request).await?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = response.into_body();
    let text = if first_frame {
        let frame = timeout(Duration::from_secs(10), body.frame())
            .await?
            .ok_or("no frame")??;
        let data = frame.into_data().map_err(|_| "not a data frame")?;
        String::from_utf8(data.to_vec())?
    } else {
        String::from_utf8(body.collect().await?.to_bytes().to_vec())?
    };
    Ok((status, headers, text))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hord_serve_mounts_the_ui_beside_the_grpc_services() -> TestResult {
    let sc = scenario().await?;
    let addr = sc.running.addr;
    let name = sc
        .dir
        .0
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("dir name")?
        .to_owned();

    let (status, strip) = http1(addr, "/", false).await?;
    assert_eq!(status, 200, "{strip}");
    has(&strip, "data-live=\"/events\"")?;
    // The same page under the repository's prefix links under it.
    let (status, prefixed) = http1(addr, &format!("/r/{name}/"), false).await?;
    assert_eq!(status, 200, "{prefixed}");
    has(
        &prefixed,
        &format!("href=\"/r/{name}/changes/{}\"", sc.landed),
    )?;
    let (status, _) = http1(
        addr,
        &format!("/r/{name}/recordings/{}/frames?at=2", sc.recording),
        false,
    )
    .await?;
    assert_eq!(status, 200);
    let (status, css) = http1(addr, "/static/hord.css", false).await?;
    assert_eq!(status, 200);
    has(&css, "prefers-color-scheme")?;
    let (status, _) = http1(addr, "/changes/not-an-id", false).await?;
    assert_eq!(status, 400);

    // The live region: SSE, starting with every row.
    let (status, first) = http1(addr, "/events", true).await?;
    assert_eq!(status, 200);
    has(&first, "event: reset")?;
    has(&first, "Make two return twenty")?;

    // gRPC still answers on the same port, and so does Changes.
    let remote = RemoteRepo::connect(&sc.running.url()).await?;
    let view = ChangesBackend::get_change(
        &remote.changes(),
        proto::GetChangeRequest {
            change: sc.parked.clone(),
        },
    )
    .await?;
    assert_eq!(
        view.queue.as_ref().map(proto::QueueEntry::status),
        Some(proto::QueueStatus::Conflicted)
    );
    assert!(
        view.history
            .iter()
            .any(|e| matches!(e.kind(), Some(Kind::Parked(_))))
    );
    sc.running.stop().await;
    Ok(())
}

/// With an auth file, the UI identifies the browser by its sign-in cookie
/// and holds every call it makes to the scope the RPC needs; a review from
/// the UI is signed with the server's key and verifies with it, and only
/// that key's actor may make one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_auth_the_ui_checks_scopes_and_signs_reviews() -> TestResult {
    let sc = scenario().await?;
    sc.running.stop().await;
    let auth_file = sc.dir.0.join("auth.toml");
    let reviewer = [Scope::Read, Scope::Review(UI_REVIEW_KIND.into())];
    AuthStore::add_user(&auth_file, "ada", "pw-ada", &reviewer)?;
    AuthStore::add_user(&auth_file, "bob", "pw-bob", &reviewer)?;
    let ada_key = Arc::new(SigningKey::generate()?);
    let bob_key = SigningKey::generate()?;
    let store = AuthStore::open(&auth_file)?;
    let ada = store.login("ada", "pw-ada", &ada_key.public().key_id())?;
    let bob = store.login("bob", "pw-bob", &bob_key.public().key_id())?;
    let signer = UiSigner {
        actor: ada.principal.actor.clone(),
        key: Arc::clone(&ada_key),
    };
    let running = serve_with(&sc.dir.0, |server| {
        server.with_auth(store).with_ui_signer(signer)
    })
    .await?;
    let addr = running.addr;
    let ada_cookie = format!("{UI_TOKEN_COOKIE}={}", ada.token);
    let bob_cookie = format!("{UI_TOKEN_COOKIE}={}", bob.token);

    // Signed out, the pages ask for a token; signing in sets the cookie.
    let (status, page) = http1(addr, "/", false).await?;
    assert_eq!(status, 401, "{page}");
    has(&page, "/login")?;
    let (status, headers, _) =
        with_cookie(addr, "POST", "/login", "", &format!("token={}", ada.token)).await?;
    assert_eq!(status, 303);
    let set = headers
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .ok_or("set-cookie")?;
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict"),
        "{set}"
    );
    let (status, _, page) = with_cookie(addr, "GET", "/", &ada_cookie, "").await?;
    assert_eq!(status, 200, "{page}");

    // A cookie never authorizes a gRPC call.
    let bare = RemoteRepo::connect(&running.url()).await?;
    assert!(bare.head(proto::HeadRequest {}).await.is_err());

    // Ada may read and review, not arbitrate.
    let (status, _, acted) = with_cookie(
        addr,
        "POST",
        &format!("/arbitrate/{}", sc.parked),
        &ada_cookie,
        "action=pick_theirs",
    )
    .await?;
    assert_eq!(status, 200, "{acted}");
    has(&acted, "permission denied")?;

    // Bob may review, but the server's key is Ada's: ingest refuses.
    let (_, _, refused) = with_cookie(
        addr,
        "POST",
        &format!("/changes/{}/review", sc.parked),
        &bob_cookie,
        "verdict=approve&message=lgtm",
    )
    .await?;
    has(&refused, "Review failed")?;

    // Ada's review is signed with her key and attached to the change.
    let (status, _, reviewed) = with_cookie(
        addr,
        "POST",
        &format!("/changes/{}/review", sc.parked),
        &ada_cookie,
        "verdict=approve&message=lgtm",
    )
    .await?;
    assert_eq!(status, 200, "{reviewed}");
    has(&reviewed, "Approval recorded as evidence")?;
    has(&reviewed, "review:human")?;
    let remote = RemoteRepo::connect_with_token(&running.url(), &ada.token).await?;
    let view = remote
        .changes()
        .get_change(proto::GetChangeRequest {
            change: sc.parked.clone(),
        })
        .await?;
    let review = view
        .evidence
        .iter()
        .find(|e| e.qualifier.as_deref() == Some(UI_REVIEW_KIND))
        .ok_or("the review is listed")?;
    let object = remote
        .get_objects(proto::GetObjectsRequest {
            ids: vec![review.id.clone()],
        })
        .await?;
    let evidence: Evidence = hord_encoding::decode(&object.objects.first().ok_or("object")?.cbor)?;
    sign::verify_evidence(&evidence, &ada_key.public())?;
    running.stop().await;
    Ok(())
}

/// The arbitration round-trip (M5): a parked change resolved from the
/// workbench lands as a change with both parents, and the `Arbitrated`
/// event carries the arbiter's signature over the decision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_change_resolved_from_the_workbench_lands_with_both_parents() -> TestResult {
    let sc = scenario().await?;
    sc.running.stop().await;
    let key = Arc::new(SigningKey::generate()?);
    let signer = UiSigner {
        actor: Actor::Human {
            id: "arbiter".into(),
        },
        key: Arc::clone(&key),
    };
    let running = serve_with(&sc.dir.0, |server| server.with_ui_signer(signer)).await?;
    let addr = running.addr;

    // The workbench offers the decision while the change is parked.
    let (status, bench) = http1(addr, &format!("/arbitrate/{}", sc.parked), false).await?;
    assert_eq!(status, 200, "{bench}");
    has(&bench, "value=\"pick_theirs\"")?;
    has(&bench, "conflict check")?;

    let (status, _, acted) = with_cookie(
        addr,
        "POST",
        &format!("/arbitrate/{}", sc.parked),
        "",
        "action=pick_theirs",
    )
    .await?;
    assert_eq!(status, 200, "{acted}");
    has(&acted, "lands with both as parents")?;

    // The lander lands the resolution.
    let remote = RemoteRepo::connect(&running.url()).await?;
    let mut entry = None;
    for _ in 0..300 {
        let queue = remote
            .queue(proto::QueueQuery {
                change: Some(sc.parked.clone()),
                ..Default::default()
            })
            .await?;
        if let Some(last) = queue.entries.last()
            && last.status() == proto::QueueStatus::Arbitrated
        {
            entry = Some(last.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entry = entry.ok_or("the parked change was never arbitrated")?;
    let landed = entry.landed.clone().ok_or("the resolution landed")?;

    // Both parents: the parked change and what it collided with.
    let resolution = remote
        .changes()
        .get_change(proto::GetChangeRequest {
            change: landed.clone(),
        })
        .await?;
    assert!(
        resolution.parents.contains(&sc.parked),
        "{:?}",
        resolution.parents
    );
    let collided = remote
        .changes()
        .get_change(proto::GetChangeRequest {
            change: sc.landed.clone(),
        })
        .await?
        .queue
        .and_then(|q| q.landed)
        .unwrap_or_else(|| sc.landed.clone());
    assert!(
        resolution.parents.contains(&collided),
        "{:?}",
        resolution.parents
    );

    // A signed Arbitrated event, verifiable with the arbiter's key. The
    // queue entry can settle before the event is in the log: wait for it.
    let mut arbitrated = None;
    for _ in 0..300 {
        let view = remote
            .changes()
            .get_change(proto::GetChangeRequest {
                change: sc.parked.clone(),
            })
            .await?;
        arbitrated = view.history.iter().find_map(|e| match e.kind() {
            Some(Kind::Arbitrated(a)) => Some(a.clone()),
            _ => None,
        });
        if arbitrated.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let arbitrated = arbitrated.ok_or("an Arbitrated event")?;
    assert_eq!(
        arbitrated.key_id.as_deref(),
        Some(key.public().key_id().as_str())
    );
    let signature = hord_core::Signature {
        key_id: arbitrated.key_id.clone().ok_or("key id")?,
        bytes: Bytes::new(arbitrated.signature.clone().ok_or("signature")?),
    };
    hord_txn::verify_arbitration(
        wire::object_id("parked", &sc.parked)?,
        &hord_txn::Arbitration::PickTheirs,
        &signature,
        &key.public(),
    )?;

    // The workbench and the strip now say it was resolved.
    let (_, after) = http1(addr, &format!("/arbitrate/{}", sc.parked), false).await?;
    has(&after, "arbitrated, landed as")?;
    assert!(
        !after.contains("value=\"pick_theirs\""),
        "no decision left to make"
    );
    has(&after, "signed with")?;
    let (_, strip) = http1(addr, "/", false).await?;
    has(&strip, "stage-arbitrated")?;
    running.stop().await;
    Ok(())
}

/// One scripted replay: give up (`None`), or replace `from` with `to`.
type Step = Option<(&'static str, &'static str)>;

/// A replay harness that plays a script in process: `None` gives up, and
/// `Some((from, to))` edits `src/lib.rs` in the replay workspace and
/// proposes. It records the notes it was sent.
#[derive(Clone, Default)]
struct Scripted {
    steps: Arc<Mutex<VecDeque<Step>>>,
    notes: Arc<Mutex<Vec<Option<String>>>>,
}

impl ReplayHarness for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }

    fn replay(&self, request: proto::ReplayRequest, repo: Repo) -> ReplayFuture {
        if let Ok(mut notes) = self.notes.lock() {
            notes.push(request.note.clone());
        }
        let step = self.steps.lock().ok().and_then(|mut s| s.pop_front());
        Box::pin(async move {
            use proto::replay_result::Status;
            let Some(Some((from, to))) = step else {
                return Ok(proto::ReplayResult {
                    status: Some(Status::GaveUp(proto::ReplayGaveUp {
                        reason: "the script says no".into(),
                    })),
                    ..Default::default()
                });
            };
            let id = request
                .workspace
                .parse()
                .map_err(|e| format!("workspace: {e:?}"))?;
            let mut ws = repo
                .open_workspace(id, agent("replayer"), None)
                .await
                .map_err(|e| e.to_string())?;
            let file: RepoPath = "src/lib.rs".parse().map_err(|e| format!("{e:?}"))?;
            let text = ws
                .read_file(&file)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("src/lib.rs")?;
            let text = String::from_utf8(text.as_slice().to_vec()).map_err(|e| e.to_string())?;
            ws.write_file(&file, text.replacen(from, to, 1))
                .await
                .map_err(|e| e.to_string())?;
            let proposal = ws
                .propose(Intent::from_summary("replay: keep both"))
                .await
                .map_err(|e| e.to_string())?;
            Ok(proto::ReplayResult {
                status: Some(Status::Proposed(proto::ReplayProposed {
                    change: proposal.change.to_hex(),
                })),
                tokens: Some(42),
                cost_micros: Some(1_500),
                model: Some("scripted".into()),
            })
        })
    }
}

/// Rung 2 from the workbench: a replay with the arbiter's note reaches the
/// harness, each attempt shows how it ended, and a replay that proposes a
/// change resolves the parked one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_workbench_reruns_replay_with_a_note_and_shows_each_attempt() -> TestResult {
    let sc = scenario().await?;
    sc.running.stop().await;
    let harness = Scripted {
        steps: Arc::new(Mutex::new(VecDeque::from([
            None,
            Some(("    20\n", "    22\n")),
        ]))),
        notes: Arc::default(),
    };
    let key = Arc::new(SigningKey::generate()?);
    let signer = UiSigner {
        actor: Actor::Human {
            id: "arbiter".into(),
        },
        key,
    };
    let running = serve_harness(
        &sc.dir.0,
        Some(Arc::new(harness.clone()) as Arc<dyn ReplayHarness>),
        |server| server.with_ui_signer(signer),
    )
    .await?;
    let addr = running.addr;
    let remote = RemoteRepo::connect(&running.url()).await?;

    // First replay: the harness gives up.
    let (status, _, acted) = with_cookie(
        addr,
        "POST",
        &format!("/arbitrate/{}", sc.parked),
        "",
        "action=replay&note=keep+both+values",
    )
    .await?;
    assert_eq!(status, 200, "{acted}");
    has(&acted, "Replay requested")?;
    let mut settled = false;
    for _ in 0..300 {
        let e = latest(&remote, &sc.parked).await?;
        let done = e.escalation.as_ref().is_some_and(|x| {
            x.attempts
                .iter()
                .any(|a| a.outcome() == proto::ReplayOutcome::GaveUp)
        });
        if done && present_status_is_open(&e) {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(settled, "the first replay never settled");
    let notes = harness.notes.lock().map(|n| n.clone()).unwrap_or_default();
    assert_eq!(notes.first(), Some(&Some("keep both values".to_owned())));
    let (_, bench) = http1(addr, &format!("/arbitrate/{}", sc.parked), false).await?;
    has(&bench, "Replay attempts")?;
    has(&bench, "gave up: the script says no")?;
    has(&bench, "keep both values")?;
    has(&bench, "replay #1 (scripted)")?;

    // Second replay: the harness proposes a change, which lands and
    // resolves the parked one.
    with_cookie(
        addr,
        "POST",
        &format!("/arbitrate/{}", sc.parked),
        "",
        "action=replay",
    )
    .await?;
    let mut resolved = None;
    for _ in 0..300 {
        let e = latest(&remote, &sc.parked).await?;
        if e.status() == proto::QueueStatus::Replayed {
            resolved = Some(e);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let resolved = resolved.ok_or("the second replay never resolved it")?;
    assert!(resolved.landed.is_some());
    let (_, bench) = http1(addr, &format!("/arbitrate/{}", sc.parked), false).await?;
    has(&bench, "resolved by replay, landed as")?;
    has(&bench, "proposed a change")?;
    has(&bench, "42 tokens")?;
    assert!(
        !bench.contains("value=\"pick_theirs\""),
        "no decision left to make"
    );
    running.stop().await;
    Ok(())
}

/// The latest queue entry naming `change`.
async fn latest(remote: &RemoteRepo, change: &str) -> TestResult<proto::QueueEntry> {
    let queue = remote
        .queue(proto::QueueQuery {
            change: Some(change.to_owned()),
            ..Default::default()
        })
        .await?;
    Ok(queue.entries.last().cloned().ok_or("a queue entry")?)
}

/// Whether an arbiter can act on the entry (conflicted or out of replays).
fn present_status_is_open(entry: &proto::QueueEntry) -> bool {
    matches!(
        entry.status(),
        proto::QueueStatus::Conflicted | proto::QueueStatus::NeedsArbitration
    )
}
