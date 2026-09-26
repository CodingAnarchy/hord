//! Web UI views 4–6 (spec §10.4, M6) end to end over `RemoteRepo`: node
//! lineage across a rename and a cross-file move, the provenance trace of a
//! change that parked, was arbitrated, and landed (with its JSON export),
//! and the repository browser (tree, file, a node's edges).
//!
//! - The `Changes` RPCs answer over the wire.
//! - The UI router over `Audited` remote backends calls only RPCs of
//!   `hord.proto` (the ADR 0030 check).
//! - With auth on, the pages need the `read` scope.

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{self, Body};
use hord_api::auth::Scope;
use hord_api::proto::event::Kind;
use hord_api::{ChangesBackend, RepoBackend, proto, wire};
use hord_core::sign::SigningKey;
use hord_core::{Actor, Bytes, ChangeId, Intent, RepoPath};
use hord_remote::RemoteRepo;
use hord_server::{AuthStore, Hosts, Server, ServerConfig};
use hord_txn::{BeginOptions, QueueStatus, ReplayFuture, ReplayHarness, Repo, RepoOptions};
use hord_ui::UI_TOKEN_COOKIE;
use hord_ui::audit::{AuditLog, Audited, unlisted};
use hord_ui::{SingleRepo, UiRepo};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::handshake;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tower::ServiceExt;

use common::{Dir, Running, TestResult, temp};

/// `parse` before its rename, with a function that keeps it company.
const LIB: &str = "pub fn parse(input: &str) -> usize {\n    let trimmed = input.trim();\n    trimmed.len() + 1\n}\n\npub fn keep() -> u32 {\n    7\n}\n";

/// `parse` renamed.
const LIB_RENAMED: &str = "pub fn parse_all(input: &str) -> usize {\n    let trimmed = input.trim();\n    trimmed.len() + 1\n}\n\npub fn keep() -> u32 {\n    7\n}\n";

/// `lib.rs` once `parse_all` moved out.
const LIB_AFTER_MOVE: &str = "pub fn keep() -> u32 {\n    7\n}\n";

/// `parse_all` moved unchanged (ADR 0033), beside a caller and a test.
const UTIL: &str = "pub fn parse_all(input: &str) -> usize {\n    let trimmed = input.trim();\n    trimmed.len() + 1\n}\n\npub fn twice(input: &str) -> usize {\n    parse_all(input) * 2\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn parses() {\n        assert_eq!(parse_all(\" a \"), 2);\n    }\n}\n";

fn agent(id: &str) -> Actor {
    Actor::Agent {
        id: id.into(),
        model: "test-model".into(),
        model_hash: Bytes::new(Vec::new()),
        harness: "browse-test".into(),
    }
}

fn path(p: &str) -> TestResult<RepoPath> {
    Ok(p.parse()?)
}

async fn serve(root: &Path, setup: impl FnOnce(Server) -> Server) -> TestResult<Running> {
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

/// Write `files` (deleting those given as `None`) in a workspace at head
/// and propose them.
async fn propose(
    repo: &Repo,
    who: &str,
    summary: &str,
    files: &[(&str, Option<&str>)],
) -> TestResult<ChangeId> {
    let mut ws = repo.begin(BeginOptions::at_head(agent(who))).await?;
    for (file, text) in files {
        match text {
            Some(text) => ws.write_file(&path(file)?, *text).await?,
            None => ws.delete_file(&path(file)?).await?,
        }
    }
    let mut intent = Intent::from_summary(summary);
    intent.body = format!("{who}: {summary}.");
    Ok(ws.propose(intent).await?.change)
}

/// Propose and land in process.
async fn land(repo: &Repo, summary: &str, files: &[(&str, Option<&str>)]) -> TestResult {
    let change = propose(repo, "agent-l", summary, files).await?;
    repo.submit(change).await?;
    repo.land_local().await?;
    let status = repo.status(change).await?.status;
    if matches!(status, QueueStatus::Landed { .. }) {
        Ok(())
    } else {
        Err(format!("{summary}: {status:?}").into())
    }
}

/// The history: seed, rename `parse` → `parse_all`, move it to
/// `src/util.rs`, edit it there. Then two agents change `twice` from the
/// same head; the server lands the first and parks the second, and an
/// arbiter picks theirs.
struct Scenario {
    dir: Dir,
    running: Running,
    parked: String,
}

/// The history in `dir`, and two proposals that change `twice` from
/// head: (triple, quadruple).
async fn seeded(dir: &Dir) -> TestResult<(String, String)> {
    Ok({
        let repo = Repo::create(&dir.0).await?;
        repo.bootstrap(
            vec![(path("src/lib.rs")?, LIB.as_bytes().to_vec())],
            Intent::from_summary("seed"),
            Actor::Human { id: "seed".into() },
        )
        .await?;
        land(
            &repo,
            "Rename parse to parse_all",
            &[("src/lib.rs", Some(LIB_RENAMED))],
        )
        .await?;
        land(
            &repo,
            "Move parse_all to util",
            &[
                ("src/lib.rs", Some(LIB_AFTER_MOVE)),
                ("src/util.rs", Some(UTIL)),
            ],
        )
        .await?;
        land(
            &repo,
            "Count the trimmed input twice",
            &[(
                "src/util.rs",
                Some(&UTIL.replace("trimmed.len() + 1", "trimmed.len() + 2")),
            )],
        )
        .await?;
        let edited = UTIL.replace("trimmed.len() + 1", "trimmed.len() + 2");
        let a = propose(
            &repo,
            "agent-a",
            "Make twice triple",
            &[(
                "src/util.rs",
                Some(&edited.replace("parse_all(input) * 2", "parse_all(input) * 3")),
            )],
        )
        .await?;
        let b = propose(
            &repo,
            "agent-b",
            "Make twice quadruple",
            &[(
                "src/util.rs",
                Some(&edited.replace("parse_all(input) * 2", "parse_all(input) * 4")),
            )],
        )
        .await?;
        (wire::id(a), wire::id(b))
    })
}

/// Submit both; the first lands, the second parks on a hard conflict.
async fn park(remote: &RemoteRepo, a: &str, b: &str) -> TestResult {
    for change in [a, b] {
        remote
            .submit(proto::SubmitRequest {
                change: change.to_owned(),
            })
            .await?;
    }
    wait_for(remote, b, |s| s == proto::QueueStatus::Conflicted).await
}

async fn scenario() -> TestResult<Scenario> {
    let dir = temp("scenario")?;
    let (a, b) = seeded(&dir).await?;
    let running = serve(&dir.0, |s| s).await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    park(&remote, &a, &b).await?;
    // Unsigned: a server without auth accepts it.
    remote
        .arbitrate(proto::ArbitrateRequest {
            change: b.clone(),
            action: Some(proto::Arbitration {
                action: Some(proto::arbitration::Action::PickTheirs(true)),
            }),
            ..Default::default()
        })
        .await?;
    wait_for(&remote, &b, |s| s == proto::QueueStatus::Arbitrated).await?;
    // The queue can settle before the events are in the log.
    for _ in 0..300 {
        let view = remote
            .changes()
            .get_change(proto::GetChangeRequest { change: b.clone() })
            .await?;
        let has = |f: fn(&Kind) -> bool| {
            view.history.iter().any(|e| {
                e.event
                    .as_ref()
                    .and_then(|e| e.kind.as_ref())
                    .is_some_and(f)
            })
        };
        if has(|k| matches!(k, Kind::Arbitrated(_))) && has(|k| matches!(k, Kind::Landed(_))) {
            return Ok(Scenario {
                dir,
                running,
                parked: b,
            });
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err("the arbitration never reached the event log".into())
}

async fn wait_for(
    remote: &RemoteRepo,
    change: &str,
    done: impl Fn(proto::QueueStatus) -> bool,
) -> TestResult {
    let mut last = None;
    for _ in 0..600 {
        let queue = remote
            .queue(proto::QueueQuery {
                change: Some(change.to_owned()),
                ..Default::default()
            })
            .await?;
        if let Some(entry) = queue.entries.last()
            && done(entry.status())
        {
            return Ok(());
        }
        last = queue.entries.last().cloned();
        sleep(Duration::from_millis(50)).await;
    }
    Err(format!("{change} never settled: {last:?}").into())
}

/// The definition named `name` in `file` at head.
async fn node_named(changes: &dyn ChangesBackend, file: &str, name: &str) -> TestResult<String> {
    let reply = changes
        .get_file(proto::GetFileRequest {
            snapshot: String::new(),
            path: file.into(),
        })
        .await?;
    reply
        .definitions
        .iter()
        .filter_map(|d| d.node.as_ref())
        .find(|n| n.name.as_deref() == Some(name))
        .map(|n| n.id.clone())
        .ok_or_else(|| format!("{name} in {file}").into())
}

fn names(nodes: &[proto::NodeRef]) -> Vec<&str> {
    nodes.iter().filter_map(|n| n.name.as_deref()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lineage_trace_and_browser_answer_over_the_wire() -> TestResult {
    let sc = scenario().await?;
    let remote = RemoteRepo::connect(&sc.running.url()).await?;
    let changes = remote.changes();

    // Lineage: one NodeId from the seed through the rename, the move to
    // another file, and the edit there.
    let parse = node_named(&changes, "src/util.rs", "parse_all").await?;
    let lineage = changes
        .node_lineage(proto::NodeLineageRequest {
            node: parse.clone(),
        })
        .await?;
    assert!(lineage.live);
    assert_eq!(
        lineage.node.as_ref().and_then(|n| n.path.as_deref()),
        Some("src/util.rs")
    );
    let summaries: Vec<&str> = lineage
        .entries
        .iter()
        .filter_map(|e| e.change.as_ref().map(|c| c.summary.as_str()))
        .collect();
    for summary in [
        "Rename parse to parse_all",
        "Move parse_all to util",
        "Count the trimmed input twice",
    ] {
        assert!(summaries.contains(&summary), "{summary} in {summaries:?}");
    }
    let entry = |summary: &str| {
        lineage
            .entries
            .iter()
            .find(|e| e.change.as_ref().is_some_and(|c| c.summary == summary))
            .ok_or_else(|| format!("entry {summary}"))
    };
    let renamed = entry("Rename parse to parse_all")?;
    assert_eq!(
        renamed.before.as_ref().and_then(|n| n.name.as_deref()),
        Some("parse")
    );
    assert_eq!(
        renamed.after.as_ref().and_then(|n| n.name.as_deref()),
        Some("parse_all")
    );
    let moved = entry("Move parse_all to util")?;
    assert_eq!(
        moved.before.as_ref().and_then(|n| n.path.as_deref()),
        Some("src/lib.rs")
    );
    assert_eq!(
        moved.after.as_ref().and_then(|n| n.path.as_deref()),
        Some("src/util.rs")
    );
    assert!(moved.ops.iter().any(|o| o.op == "move"), "{:?}", moved.ops);
    let edited = entry("Count the trimmed input twice")?;
    assert!(
        edited.ops.iter().any(|o| o.op == "replace"),
        "{:?}",
        edited.ops
    );
    assert_eq!(
        edited
            .change
            .as_ref()
            .and_then(|c| c.actor.as_ref())
            .map(wire::actor_id),
        Some("agent-l")
    );
    let unknown = changes
        .node_lineage(proto::NodeLineageRequest {
            node: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        })
        .await;
    assert!(unknown.is_err(), "a NodeId with no history is not found");

    // Trace: parked on a hard conflict, arbitrated, landed.
    let trace = changes
        .change_trace(proto::ChangeTraceRequest {
            change: sc.parked.clone(),
        })
        .await?;
    let stages: Vec<proto::TraceStage> = trace.steps.iter().map(|s| s.stage()).collect();
    for stage in [
        proto::TraceStage::Intent,
        proto::TraceStage::Proposed,
        proto::TraceStage::Submitted,
        proto::TraceStage::Conflict,
        proto::TraceStage::Parked,
        proto::TraceStage::Arbitrated,
        proto::TraceStage::Landed,
    ] {
        assert!(stages.contains(&stage), "{stage:?} in {stages:?}");
    }
    let at = |stage| stages.iter().position(|s| *s == stage);
    // Causal order: parked, then the arbiter's decision, then the
    // resolution landing (the lander records `Arbitrated` after the
    // landing; the trace must not read that way).
    assert!(at(proto::TraceStage::Parked) < at(proto::TraceStage::Arbitrated));
    assert!(
        at(proto::TraceStage::Arbitrated) < at(proto::TraceStage::Landed),
        "{stages:?}"
    );
    assert!(
        trace.steps.windows(2).all(|w| w[0].at_ms <= w[1].at_ms),
        "the timeline is in time order"
    );
    let parked = trace
        .steps
        .iter()
        .find(|s| s.stage() == proto::TraceStage::Parked)
        .ok_or("a parked step")?;
    assert_eq!(parked.park, Some(proto::ParkReason::MergeConflict.into()));
    let landed = trace.landed.as_ref().ok_or("the resolution it landed as")?;
    assert!(landed.parents.contains(&sc.parked), "{:?}", landed.parents);
    assert_eq!(
        trace.submitted.as_ref().map(|s| s.change.as_str()),
        Some(sc.parked.as_str())
    );

    // Browser: the tree, a file with its definitions, and edges.
    let root = changes.list_tree(proto::ListTreeRequest::default()).await?;
    assert_eq!(
        root.entries
            .iter()
            .map(|e| (e.name.as_str(), e.dir))
            .collect::<Vec<_>>(),
        [("src", true)]
    );
    let src = changes
        .list_tree(proto::ListTreeRequest {
            snapshot: root.snapshot.clone(),
            path: "src".into(),
        })
        .await?;
    let files: Vec<&str> = src.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(files, ["src/lib.rs", "src/util.rs"]);
    assert!(src.entries.iter().all(|e| e.blob.is_some()));
    assert!(
        changes
            .list_tree(proto::ListTreeRequest {
                snapshot: root.snapshot.clone(),
                path: "nope".into(),
            })
            .await
            .is_err()
    );
    let file = changes
        .get_file(proto::GetFileRequest {
            snapshot: root.snapshot.clone(),
            path: "src/util.rs".into(),
        })
        .await?;
    assert!(file.text.contains("parse_all(input) * 4"), "theirs landed");
    let parse_def = file
        .definitions
        .iter()
        .find(|d| d.node.as_ref().is_some_and(|n| n.id == parse))
        .ok_or("parse_all in the file")?;
    assert_eq!((parse_def.start_line, parse_def.end_line), (1, 4));
    assert!(
        changes
            .get_file(proto::GetFileRequest {
                snapshot: String::new(),
                path: "src/missing.rs".into(),
            })
            .await
            .is_err()
    );

    let twice = node_named(&changes, "src/util.rs", "twice").await?;
    let edges = changes
        .node_edges(proto::NodeEdgesRequest {
            snapshot: String::new(),
            node: parse.clone(),
        })
        .await?;
    assert!(names(&edges.referenced_by).contains(&"twice"), "{edges:?}");
    let tested_by = names(&edges.tested_by);
    assert!(
        tested_by.iter().any(|n| n.ends_with("parses")),
        "{tested_by:?}"
    );
    let from_twice = changes
        .node_edges(proto::NodeEdgesRequest {
            snapshot: root.snapshot.clone(),
            node: twice,
        })
        .await?;
    assert!(
        from_twice.references.iter().any(|n| n.id == parse),
        "{from_twice:?}"
    );
    sc.running.stop().await;
    drop(sc.dir);
    Ok(())
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

fn has(page: &str, needle: &str) -> TestResult {
    if page.contains(needle) {
        Ok(())
    } else {
        Err(format!("page lacks {needle:?}:\n{page}").into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn views_four_to_six_render_and_call_only_listed_rpcs() -> TestResult {
    let sc = scenario().await?;
    let remote = RemoteRepo::connect(&sc.running.url()).await?;
    let log = AuditLog::new();
    let router = hord_ui::router(Arc::new(SingleRepo(UiRepo {
        backend: Arc::new(Audited::new(Arc::new(remote.clone()), log.clone())),
        changes: Arc::new(Audited::new(Arc::new(remote.changes()), log.clone())),
        review: None,
        arbiter: None,
        name: "browse-test".into(),
        base: String::new(),
    })));
    let parse = node_named(&remote.changes(), "src/util.rs", "parse_all").await?;

    // View 4: lineage, newest first, saying what each change did.
    let (status, page) = get(&router, &format!("/nodes/{parse}")).await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "Lineage: parse_all")?;
    has(&page, "renamed parse → parse_all")?;
    has(&page, "moved src/lib.rs → src/util.rs")?;
    has(&page, "/trace/")?;
    has(&page, &format!("/graph/head/{parse}"))?;
    let newest = page.find("Count the trimmed input twice").ok_or("edit")?;
    let oldest = page.find("Rename parse to parse_all").ok_or("rename")?;
    assert!(newest < oldest, "newest first");

    // View 5: why it was parked, in words, then who resolved it.
    let (status, page) = get(&router, &format!("/trace/{}", sc.parked)).await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "Make twice quadruple")?;
    has(&page, "collide with a landed change")?;
    let decided = page.find("resolved it as").ok_or("the decision")?;
    let landed = page.find("landed at #").ok_or("the landing")?;
    let parked = page
        .find("collide with a landed change")
        .ok_or("the park")?;
    assert!(
        parked < decided && decided < landed,
        "parked, then decided, then landed"
    );
    has(&page, "export JSON")?;
    let (status, json) = get(&router, &format!("/trace/{}/json", sc.parked)).await?;
    assert_eq!(status, 200, "{json}");
    let exported: proto::ChangeTraceResponse = serde_json::from_str(&json)?;
    assert_eq!(exported.change, sc.parked);
    assert!(exported.steps.len() >= 6);
    let value: serde_json::Value = serde_json::from_str(&json)?;
    assert!(
        value["steps"][0]["stage"]
            .as_str()
            .is_some_and(|s| s.starts_with("TRACE_STAGE_")),
        "canonical JSON spells enums by name: {}",
        value["steps"][0]
    );

    // View 6: the browser.
    let (status, page) = get(&router, "/tree").await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "src/</a>")?;
    let snapshot = page
        .split("/tree/")
        .nth(1)
        .and_then(|s| s.split(['"', '?']).next())
        .ok_or("snapshot link")?
        .to_owned();
    let (status, page) = get(&router, &format!("/tree/{snapshot}?path=src")).await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "util.rs")?;
    let (status, page) = get(&router, &format!("/file/{snapshot}?path=src/util.rs")).await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "id=\"L1\"")?;
    has(&page, &format!("/graph/{snapshot}/{parse}"))?;
    has(&page, &format!("/nodes/{parse}"))?;
    let (status, page) = get(&router, &format!("/graph/{snapshot}/{parse}")).await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "<svg")?;
    has(&page, "twice")?;
    has(&page, "parses")?;
    has(&page, &format!("/nodes/{parse}"))?;
    let (status, _) = get(&router, "/graph/head/01ARZ3NDEKTSV4RRFFQ69G5FAV").await?;
    assert_eq!(status, 404);

    // Views 1–3 link into 4–6.
    let (_, page) = get(&router, &format!("/changes/{}", sc.parked)).await?;
    has(&page, &format!("/trace/{}", sc.parked))?;
    has(&page, "/nodes/")?;
    has(&page, "browse its result")?;
    let (_, page) = get(&router, &format!("/arbitrate/{}", sc.parked)).await?;
    has(&page, &format!("/trace/{}", sc.parked))?;

    let calls = log.calls();
    assert!(unlisted(&log).is_empty(), "{:?}", unlisted(&log));
    for rpc in [
        "hord.v1.Changes/NodeLineage",
        "hord.v1.Changes/ChangeTrace",
        "hord.v1.Changes/ListTree",
        "hord.v1.Changes/GetFile",
        "hord.v1.Changes/NodeEdges",
    ] {
        assert!(calls.iter().any(|c| c == rpc), "{rpc} in {calls:?}");
    }
    sc.running.stop().await;
    drop(sc.dir);
    Ok(())
}

async fn http1(addr: SocketAddr, uri: &str, cookie: &str) -> TestResult<(u16, String)> {
    let request = http::Request::get(uri)
        .header("host", addr.to_string())
        .header("cookie", cookie)
        .body(Full::new(body::Bytes::new()))?;
    let stream = TcpStream::connect(addr).await?;
    let (mut sender, conn) = handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let response = timeout(Duration::from_secs(60), sender.send_request(request)).await??;
    let status = response.status().as_u16();
    let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
    Ok((status, text))
}

/// With an auth file, views 4–6 render for a token with `read` and are
/// refused without a token or without the scope; the RPCs are refused on
/// the wire without a token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_auth_views_four_to_six_need_the_read_scope() -> TestResult {
    let sc = scenario().await?;
    let parse = node_named(
        &RemoteRepo::connect(&sc.running.url()).await?.changes(),
        "src/util.rs",
        "parse_all",
    )
    .await?;
    sc.running.stop().await;
    let auth_file = sc.dir.0.join("auth.toml");
    AuthStore::add_user(&auth_file, "ada", "pw-ada", &[Scope::Read])?;
    AuthStore::add_user(&auth_file, "eve", "pw-eve", &[Scope::Propose])?;
    let store = AuthStore::open(&auth_file)?;
    let ada = store.login("ada", "pw-ada", &SigningKey::generate()?.public().key_id())?;
    let eve = store.login("eve", "pw-eve", &SigningKey::generate()?.public().key_id())?;
    let running = serve(&sc.dir.0, |server| server.with_auth(store)).await?;
    let addr = running.addr;
    let reader = format!("{UI_TOKEN_COOKIE}={}", ada.token);
    let proposer = format!("{UI_TOKEN_COOKIE}={}", eve.token);

    for uri in [
        format!("/nodes/{parse}"),
        format!("/trace/{}", sc.parked),
        format!("/trace/{}/json", sc.parked),
        "/tree".to_owned(),
        "/file/head?path=src/util.rs".to_owned(),
        format!("/graph/head/{parse}"),
    ] {
        let (status, page) = http1(addr, &uri, &reader).await?;
        assert_eq!(status, 200, "{uri} with read: {page}");
        let (status, _) = http1(addr, &uri, "").await?;
        assert_eq!(status, 401, "{uri} without a token");
        let (status, _) = http1(addr, &uri, &proposer).await?;
        assert_eq!(status, 403, "{uri} without the read scope");
    }

    let bare = RemoteRepo::connect(&running.url()).await?;
    assert!(
        bare.changes()
            .node_lineage(proto::NodeLineageRequest { node: parse })
            .await
            .is_err()
    );
    let reading = RemoteRepo::connect_with_token(&running.url(), &ada.token).await?;
    reading
        .changes()
        .list_tree(proto::ListTreeRequest::default())
        .await?;
    running.stop().await;
    Ok(())
}

/// One scripted replay: give up (`None`), or replace `from` with `to` in
/// `src/util.rs`.
type Step = Option<(&'static str, &'static str)>;

/// A replay harness that plays a script in process and reports its spend.
#[derive(Clone, Default)]
struct Scripted {
    steps: Arc<Mutex<VecDeque<Step>>>,
}

impl ReplayHarness for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }

    fn replay(&self, request: proto::ReplayRequest, repo: Repo) -> ReplayFuture {
        let step = self.steps.lock().ok().and_then(|mut s| s.pop_front());
        Box::pin(async move {
            use proto::replay_result::Status;
            let Some(Some((from, to))) = step else {
                return Ok(proto::ReplayResult {
                    status: Some(Status::GaveUp(proto::ReplayGaveUp {
                        reason: "the script says no".into(),
                    })),
                    tokens: Some(10),
                    cost_micros: Some(500),
                    model: Some("scripted-model".into()),
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
            let file: RepoPath = "src/util.rs".parse().map_err(|e| format!("{e:?}"))?;
            let text = ws
                .read_file(&file)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("src/util.rs")?;
            let text = String::from_utf8(text.as_slice().to_vec()).map_err(|e| e.to_string())?;
            ws.write_file(&file, text.replacen(from, to, 1))
                .await
                .map_err(|e| e.to_string())?;
            let proposal = ws
                .propose(Intent::from_summary("replay: quadruple on top of triple"))
                .await
                .map_err(|e| e.to_string())?;
            Ok(proto::ReplayResult {
                status: Some(Status::Proposed(proto::ReplayProposed {
                    change: proposal.change.to_hex(),
                })),
                tokens: Some(42),
                cost_micros: Some(1_500),
                model: Some("scripted-model".into()),
            })
        })
    }
}

/// The trace of a change resolved by replay: each attempt with its
/// outcome, spend, and model, in order, then the replay landing, and the
/// landed replay tied to the original by `parent_intent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_change_traces_each_attempt_and_its_landing() -> TestResult {
    let dir = temp("replay")?;
    let (a, b) = seeded(&dir).await?;
    let harness = Scripted {
        steps: Arc::new(Mutex::new(VecDeque::from([
            None,
            Some(("parse_all(input) * 3", "parse_all(input) * 4")),
        ]))),
    };
    let running = serve_harness(
        &dir.0,
        Some(Arc::new(harness) as Arc<dyn ReplayHarness>),
        |s| s,
    )
    .await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    // With a harness, the ladder replays a parked change by itself
    // (spec §6.4 rung 2): the first attempt gives up, the second lands.
    for change in [&a, &b] {
        remote
            .submit(proto::SubmitRequest {
                change: change.clone(),
            })
            .await?;
    }
    wait_for(&remote, &b, |s| s == proto::QueueStatus::Replayed).await?;

    let trace = remote
        .changes()
        .change_trace(proto::ChangeTraceRequest { change: b.clone() })
        .await?;
    let replays: Vec<&proto::TraceStep> = trace
        .steps
        .iter()
        .filter(|s| s.stage() == proto::TraceStage::Replay)
        .collect();
    assert_eq!(replays.len(), 2, "{:?}", trace.steps);
    let (gave_up, proposed) = (replays[0], replays[1]);
    assert!(
        gave_up.text.starts_with("replay #1 (scripted"),
        "{}",
        gave_up.text
    );
    assert!(
        gave_up.text.contains("gave up: the script says no"),
        "{}",
        gave_up.text
    );
    assert!(gave_up.text.contains("10 tokens"), "{}", gave_up.text);
    assert!(gave_up.text.contains("$0.0005"), "{}", gave_up.text);
    assert!(
        gave_up.text.contains("model scripted-model"),
        "{}",
        gave_up.text
    );
    assert_eq!(gave_up.outcome.as_deref(), Some("fail"));
    assert!(
        proposed.text.starts_with("replay #2 (scripted"),
        "{}",
        proposed.text
    );
    assert!(
        proposed.text.contains("proposed a change"),
        "{}",
        proposed.text
    );
    assert!(
        proposed
            .text
            .contains("42 tokens · $0.0015 · model scripted-model"),
        "{}",
        proposed.text
    );
    assert_eq!(proposed.outcome.as_deref(), Some("pass"));
    let proposal = proposed.change.clone().ok_or("the replay's proposal")?;

    // The conflict, then the attempts in order, then the replay landing.
    let at = |stage: proto::TraceStage| trace.steps.iter().position(|s| s.stage() == stage);
    let first_replay = trace
        .steps
        .iter()
        .position(|s| ptr::eq(s, gave_up))
        .ok_or("attempt 1")?;
    let second_replay = trace
        .steps
        .iter()
        .position(|s| ptr::eq(s, proposed))
        .ok_or("attempt 2")?;
    let landed_at = trace
        .steps
        .iter()
        .rposition(|s| s.stage() == proto::TraceStage::Landed)
        .ok_or("a landed step")?;
    assert!(at(proto::TraceStage::Conflict) < Some(first_replay));
    assert!(first_replay < second_replay && second_replay < landed_at);

    // The proposal and the record it landed as both name the original.
    let parent = |view: &proto::ChangeView| {
        view.provenance
            .as_ref()
            .and_then(|p| p.parent_intent.clone())
    };
    let replayed = trace
        .replays
        .iter()
        .find(|v| v.change == proposal)
        .ok_or("the proposal is among the trace's sources")?;
    assert_eq!(parent(replayed), Some(b.clone()));
    let landed = trace.landed.as_ref().ok_or("the replay it landed as")?;
    assert_eq!(parent(landed), Some(b.clone()));
    assert_eq!(
        trace.steps[landed_at].change.as_deref(),
        Some(landed.change.as_str())
    );

    // On the page: both attempts, then the landing.
    let router = hord_ui::router(Arc::new(SingleRepo(UiRepo {
        backend: Arc::new(remote.clone()),
        changes: Arc::new(remote.changes()),
        review: None,
        arbiter: None,
        name: "browse-test".into(),
        base: String::new(),
    })));
    let (status, page) = get(&router, &format!("/trace/{b}")).await?;
    assert_eq!(status, 200, "{page}");
    let one = page.find("replay #1").ok_or("attempt 1 on the page")?;
    let two = page.find("replay #2").ok_or("attempt 2 on the page")?;
    let landing = page.find("landed at #").ok_or("the landing on the page")?;
    assert!(one < two && two < landing);
    has(&page, "replay proposal")?;
    running.stop().await;
    drop(dir);
    Ok(())
}
