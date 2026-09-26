//! Arbitration end to end against an in-process `hord serve` (spec §6.4,
//! §10.5.4, §12 M5):
//!
//! - with an auth file, an arbiter token (`arbitrate`, key bound to its
//!   actor) resolves a parked change from the workbench; the resolution
//!   lands with both parents and the `Arbitrated` event's signature
//!   verifies with the arbiter's key. The refusals: a claimed arbiter that
//!   is not the token's actor, a key bound to another actor, a signature
//!   over a different decision, and a token without `arbitrate`;
//! - replay candidates (ADR 0029): a scripted harness proposes the same
//!   edit twice, a scripted verifier fails replays, the two attempts are
//!   one candidate on the workbench, and picking it lands it.

mod common;

use std::collections::VecDeque;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body;
use hord_api::auth::Scope;
use hord_api::proto::arbitration::Action;
use hord_api::proto::event::Kind;
use hord_api::{ApiError, ChangesBackend, RepoBackend, proto, wire};
use hord_core::sign::SigningKey;
use hord_core::{Actor, Bytes, ChangeId, Intent, RepoPath, Signature};
use hord_remote::RemoteRepo;
use hord_server::{AuthStore, Hosts, Server, ServerConfig, UiSigner};
use hord_txn::{
    Arbitration, BeginOptions, ReplayFuture, ReplayHarness, Repo, RepoOptions, StubVerifier,
    Verdict, Verifier, VerifyFuture, VerifyRequest, sign_arbitration, verify_arbitration,
};
use hord_ui::UI_TOKEN_COOKIE;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::handshake;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use common::{Running, TestResult, temp};

const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n";

fn agent(id: &str) -> Actor {
    Actor::Agent {
        id: id.into(),
        model: "test-model".into(),
        model_hash: Bytes::new(Vec::new()),
        harness: "arbitration-test".into(),
    }
}

fn human(id: &str) -> Actor {
    Actor::Human { id: id.into() }
}

fn lib_path() -> TestResult<RepoPath> {
    Ok("src/lib.rs".parse()?)
}

/// Serve `root` with `verifier` and `harness`, configured by `setup`.
async fn serve(
    root: &Path,
    verifier: Arc<dyn Verifier>,
    harness: Option<Arc<dyn ReplayHarness>>,
    setup: impl FnOnce(Server) -> Server,
) -> TestResult<Running> {
    let hosts = Hosts::open_repo(
        root,
        RepoOptions {
            verifier: Some(verifier),
            harness,
            ..RepoOptions::default()
        },
    )
    .await?;
    Running::start(setup(Server::new(hosts, ServerConfig::default()))).await
}

async fn propose(repo: &Repo, who: &str, body: &str, summary: &str) -> TestResult<ChangeId> {
    let mut ws = repo.begin(BeginOptions::at_head(agent(who))).await?;
    ws.write_file(&lib_path()?, LIB.replace("    2\n", body))
        .await?;
    Ok(ws.propose(Intent::from_summary(summary)).await?.change)
}

/// A seeded repository with two proposals that both change `two`: the
/// first (`landed`) lands, the second (`parked`) hits a hard merge
/// conflict with it. Returns their ids; nothing is submitted yet.
async fn two_agents(dir: &Path) -> TestResult<(String, String)> {
    let repo = Repo::create(dir).await?;
    repo.bootstrap(
        vec![(lib_path()?, LIB.as_bytes().to_vec())],
        Intent::from_summary("seed"),
        human("seed"),
    )
    .await?;
    let a = propose(&repo, "agent-a", "    20\n", "Make two return twenty").await?;
    let b = propose(&repo, "agent-b", "    22\n", "Make two return twenty-two").await?;
    Ok((wire::id(a), wire::id(b)))
}

/// Submit `landed` then `parked`, and wait until `parked` settles in
/// `status`.
async fn collide(
    remote: &RemoteRepo,
    landed: &str,
    parked: &str,
    status: proto::QueueStatus,
) -> TestResult<proto::QueueEntry> {
    for change in [landed, parked] {
        remote
            .submit(proto::SubmitRequest {
                change: change.to_owned(),
            })
            .await?;
    }
    wait_for(remote, parked, |e| e.status() == status).await
}

/// The latest queue entry naming `change`, polled until `done`.
async fn wait_for(
    remote: &RemoteRepo,
    change: &str,
    done: impl Fn(&proto::QueueEntry) -> bool,
) -> TestResult<proto::QueueEntry> {
    let mut last = None;
    for _ in 0..600 {
        let queue = remote
            .queue(proto::QueueQuery {
                change: Some(change.to_owned()),
                ..Default::default()
            })
            .await?;
        if let Some(entry) = queue.entries.last() {
            if done(entry) {
                return Ok(entry.clone());
            }
            last = Some(entry.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(format!("{change} never settled as expected: {last:#?}").into())
}

/// The `Arbitrated` event about `parked`, once it is in the log.
async fn arbitrated_event(remote: &RemoteRepo, parked: &str) -> TestResult<proto::Arbitrated> {
    for _ in 0..300 {
        let view = remote
            .changes()
            .get_change(proto::GetChangeRequest {
                change: parked.to_owned(),
            })
            .await?;
        let found = view.history.iter().find_map(|e| match e.kind() {
            Some(Kind::Arbitrated(a)) => Some(a.clone()),
            _ => None,
        });
        if let Some(found) = found {
            return Ok(found);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(format!("no Arbitrated event for {parked}").into())
}

fn event_signature(event: &proto::Arbitrated) -> TestResult<Signature> {
    Ok(Signature {
        key_id: event.key_id.clone().ok_or("the event names its key")?,
        bytes: Bytes::new(event.signature.clone().ok_or("the event is signed")?),
    })
}

/// An HTTP/1.1 request on the server with a cookie and a form body.
async fn http(
    addr: SocketAddr,
    method: &str,
    uri: &str,
    cookie: &str,
    form: &str,
) -> TestResult<(u16, String)> {
    let request = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", addr.to_string())
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Full::new(body::Bytes::from(form.to_owned())))?;
    let stream = TcpStream::connect(addr).await?;
    let (mut sender, conn) = handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let response = sender.send_request(request).await?;
    let status = response.status().as_u16();
    let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
    Ok((status, text))
}

fn has(page: &str, needle: &str) -> TestResult {
    if page.contains(needle) {
        Ok(())
    } else {
        Err(format!("page lacks {needle:?}:\n{page}").into())
    }
}

/// An `Arbitrate` request for `action` on `change`, claimed by `arbiter`
/// and signed by `key` over `signed` (which a forger makes differ from
/// `action`).
fn request(
    change: &str,
    action: Action,
    signed: &Arbitration,
    arbiter: &Actor,
    key: &SigningKey,
) -> TestResult<proto::ArbitrateRequest> {
    let signature = sign_arbitration(wire::object_id("change", change)?, signed, key)?;
    Ok(proto::ArbitrateRequest {
        change: change.to_owned(),
        action: Some(proto::Arbitration {
            action: Some(action),
        }),
        arbiter: Some(wire::actor(arbiter)),
        note: None,
        key_id: Some(signature.key_id),
        signature: Some(signature.bytes.as_slice().to_vec()),
    })
}

fn denied<T: Debug>(result: Result<T, ApiError>, needle: &str) -> TestResult {
    match result {
        Err(ApiError::PermissionDenied(m)) if m.contains(needle) => Ok(()),
        other => Err(format!("expected permission denied ({needle}), got {other:?}").into()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_auth_an_arbiter_resolves_from_the_workbench_and_forgeries_are_refused() -> TestResult
{
    let dir = temp("auth")?;
    let repo_dir = dir.0.join("repo");
    let (landed, parked) = two_agents(&repo_dir).await?;
    let auth = dir.0.join("auth.toml");
    let arbiter_scopes = [Scope::Read, Scope::Arbitrate];
    AuthStore::add_user(&auth, "judge", "pw", &arbiter_scopes)?;
    AuthStore::add_user(&auth, "rita", "pw", &arbiter_scopes)?;
    AuthStore::add_user(&auth, "reader", "pw", &[Scope::Read])?;
    let store = AuthStore::open(&auth)?;
    let judge_key = Arc::new(SigningKey::generate()?);
    let rita_key = SigningKey::generate()?;
    let judge = store.login("judge", "pw", &judge_key.public().key_id())?;
    let rita = store.login("rita", "pw", &rita_key.public().key_id())?;
    let reader = store.login("reader", "pw", &SigningKey::generate()?.public().key_id())?;
    // Agents submit the colliding changes (their records predate auth, so
    // the collision is set up before tokens are required).
    let seeding = serve(&repo_dir, Arc::new(StubVerifier), None, |s| s).await?;
    let remote = RemoteRepo::connect(&seeding.url()).await?;
    collide(&remote, &landed, &parked, proto::QueueStatus::Conflicted).await?;
    seeding.stop().await;

    let signer = UiSigner {
        actor: human("judge"),
        key: Arc::clone(&judge_key),
    };
    let running = serve(&repo_dir, Arc::new(StubVerifier), None, |s| {
        s.with_auth(store).with_ui_signer(signer)
    })
    .await?;
    let as_judge = RemoteRepo::connect_with_token(&running.url(), &judge.token).await?;
    let as_reader = RemoteRepo::connect_with_token(&running.url(), &reader.token).await?;

    // The refusals. A token without `arbitrate`:
    let theirs = || Action::PickTheirs(true);
    denied(
        as_reader
            .arbitrate(request(
                &parked,
                theirs(),
                &Arbitration::PickTheirs,
                &human("reader"),
                &judge_key,
            )?)
            .await,
        "requires scope arbitrate",
    )?;
    // An arbiter claim that is not the token's actor, signed by a key bound
    // to the claimed actor:
    denied(
        as_judge
            .arbitrate(request(
                &parked,
                theirs(),
                &Arbitration::PickTheirs,
                &human("rita"),
                &rita_key,
            )?)
            .await,
        "a decision by human rita",
    )?;
    // The token's actor, with a key bound to another actor:
    denied(
        as_judge
            .arbitrate(request(
                &parked,
                theirs(),
                &Arbitration::PickTheirs,
                &human("judge"),
                &rita_key,
            )?)
            .await,
        "bound to human rita",
    )?;
    // The right key, over a different decision:
    let forged = as_judge
        .arbitrate(request(
            &parked,
            theirs(),
            &Arbitration::PickOurs,
            &human("judge"),
            &judge_key,
        )?)
        .await;
    assert!(
        matches!(&forged, Err(ApiError::InvalidArgument(m)) if m.contains("bad signature")),
        "{forged:?}"
    );
    // None of them moved the change.
    let entry = wait_for(&as_judge, &parked, |_| true).await?;
    assert_eq!(entry.status(), proto::QueueStatus::Conflicted, "{entry:#?}");
    assert!(
        entry
            .escalation
            .as_ref()
            .and_then(|e| e.resolution.as_ref())
            .is_none()
    );

    // The workbench: signed in as someone the server's key is not bound to,
    // the decision is refused; as the judge, it is taken.
    let addr = running.addr;
    let uri = format!("/arbitrate/{parked}");
    let (status, page) = http(
        addr,
        "GET",
        &uri,
        &format!("{UI_TOKEN_COOKIE}={}", judge.token),
        "",
    )
    .await?;
    assert_eq!(status, 200, "{page}");
    has(&page, "value=\"pick_theirs\"")?;
    let (_, refused) = http(
        addr,
        "POST",
        &uri,
        &format!("{UI_TOKEN_COOKIE}={}", rita.token),
        "action=pick_theirs",
    )
    .await?;
    has(&refused, "Arbitration failed")?;
    let (status, acted) = http(
        addr,
        "POST",
        &uri,
        &format!("{UI_TOKEN_COOKIE}={}", judge.token),
        "action=pick_theirs",
    )
    .await?;
    assert_eq!(status, 200, "{acted}");
    has(&acted, "lands with both as parents")?;

    // The resolution lands with both parents.
    let entry = wait_for(&as_judge, &parked, |e| {
        e.status() == proto::QueueStatus::Arbitrated
    })
    .await?;
    let resolution = entry.landed.clone().ok_or("the resolution landed")?;
    let view = as_judge
        .changes()
        .get_change(proto::GetChangeRequest {
            change: resolution.clone(),
        })
        .await?;
    assert!(view.parents.contains(&parked), "{:?}", view.parents);
    assert!(view.parents.contains(&landed), "{:?}", view.parents);
    let head = as_judge.head(proto::HeadRequest {}).await?;
    assert_eq!(head.change.as_deref(), Some(resolution.as_str()));

    // The Arbitrated event: by the judge, and signed with the judge's key
    // over this decision only.
    let event = arbitrated_event(&as_judge, &parked).await?;
    assert_eq!(event.by, Some(wire::actor(&human("judge"))));
    assert_eq!(event.result, resolution);
    let signature = event_signature(&event)?;
    let parked_id = wire::object_id("parked", &parked)?;
    verify_arbitration(
        parked_id,
        &Arbitration::PickTheirs,
        &signature,
        &judge_key.public(),
    )?;
    assert!(
        verify_arbitration(
            parked_id,
            &Arbitration::PickTheirs,
            &signature,
            &rita_key.public()
        )
        .is_err()
    );
    assert!(
        verify_arbitration(
            parked_id,
            &Arbitration::PickOurs,
            &signature,
            &judge_key.public()
        )
        .is_err()
    );
    running.stop().await;
    Ok(())
}

/// One scripted replay: replace `from` with `to` in `src/lib.rs` and
/// propose.
type Step = (&'static str, &'static str);

/// A replay harness that plays a script in process, one step per attempt.
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
            let Some((from, to)) = step else {
                return Ok(proto::ReplayResult {
                    status: Some(Status::GaveUp(proto::ReplayGaveUp {
                        reason: "the script ran out".into(),
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
                .propose(Intent::from_summary("replay: take twenty-two"))
                .await
                .map_err(|e| e.to_string())?;
            Ok(proto::ReplayResult {
                status: Some(Status::Proposed(proto::ReplayProposed {
                    change: proposal.change.to_hex(),
                })),
                ..Default::default()
            })
        })
    }
}

/// Fails every replay's own proposal (a record replaying another, with
/// one parent), and passes everything else, arbitration resolutions
/// included (they add the parked change as a parent).
struct FailReplays;

impl Verifier for FailReplays {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        let replay =
            request.change.provenance.parent_intent.is_some() && request.change.parents.len() < 2;
        Box::pin(async move {
            if replay {
                Verdict::Fail {
                    evidence: Vec::new(),
                    reason: "scripted: the replay's tests fail".into(),
                }
            } else {
                Verdict::Pass {
                    evidence: Vec::new(),
                }
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn equal_failed_replays_are_one_candidate_and_picking_it_lands_it() -> TestResult {
    let dir = temp("candidates")?;
    let (landed, parked) = two_agents(&dir.0).await?;
    // Both attempts make the same edit on the same head: equal ops.
    let harness = Scripted {
        steps: Arc::new(Mutex::new(VecDeque::from([
            ("    20\n", "    22\n"),
            ("    20\n", "    22\n"),
        ]))),
    };
    let key = Arc::new(SigningKey::generate()?);
    let signer = UiSigner {
        actor: human("arbiter"),
        key: Arc::clone(&key),
    };
    let running = serve(
        &dir.0,
        Arc::new(FailReplays),
        Some(Arc::new(harness) as Arc<dyn ReplayHarness>),
        |s| s.with_ui_signer(signer),
    )
    .await?;
    let remote = RemoteRepo::connect(&running.url()).await?;
    let entry = collide(
        &remote,
        &landed,
        &parked,
        proto::QueueStatus::NeedsArbitration,
    )
    .await?;

    // Two attempts proposed, both failed verification, one candidate.
    let escalation = entry.escalation.as_ref().ok_or("an escalation")?;
    assert_eq!(escalation.attempts.len(), 2, "{escalation:#?}");
    for attempt in &escalation.attempts {
        assert_eq!(
            attempt.outcome(),
            proto::ReplayOutcome::Proposed,
            "{attempt:#?}"
        );
        let replay = attempt.change.as_deref().ok_or("the attempt's change")?;
        let settled = wait_for(&remote, replay, |_| true).await?;
        assert_eq!(
            settled.status(),
            proto::QueueStatus::Conflicted,
            "{settled:#?}"
        );
        let report = settled.report.as_ref().ok_or("a report")?;
        assert!(
            report
                .verification
                .as_deref()
                .is_some_and(|v| v.contains("replay's tests fail")),
            "{settled:#?}"
        );
    }
    let first = escalation.attempts[0].change.clone().ok_or("attempt 1")?;
    let second = escalation.attempts[1].change.clone().ok_or("attempt 2")?;
    assert_ne!(first, second, "two attempts, two proposals");
    assert_eq!(escalation.candidates.len(), 1, "{escalation:#?}");
    let candidate = &escalation.candidates[0];
    assert_eq!(candidate.change, first);
    assert_eq!(candidate.attempts, [1, 2]);

    // The workbench lists it, once, with both attempts.
    let addr = running.addr;
    let uri = format!("/arbitrate/{parked}");
    let (status, bench) = http(addr, "GET", &uri, "", "").await?;
    assert_eq!(status, 200, "{bench}");
    has(&bench, "Candidates")?;
    has(&bench, "#1, #2")?;
    assert_eq!(bench.matches("pick this candidate").count(), 1, "{bench}");
    has(&bench, &format!("name=\"resolved\" value=\"{first}\""))?;
    assert!(!bench.contains(&format!("value=\"{second}\"")), "{bench}");

    // Picking it lands it: the resolution has the candidate's result and
    // both parents, and the arbiter's signature is on the event.
    let (status, acted) = http(
        addr,
        "POST",
        &uri,
        "",
        &format!("action=resolved&resolved={first}"),
    )
    .await?;
    assert_eq!(status, 200, "{acted}");
    has(&acted, "lands with both as parents")?;
    let entry = wait_for(&remote, &parked, |e| {
        e.status() == proto::QueueStatus::Arbitrated
    })
    .await?;
    let resolution = entry.landed.clone().ok_or("the resolution landed")?;
    let view = remote
        .changes()
        .get_change(proto::GetChangeRequest {
            change: resolution.clone(),
        })
        .await?;
    assert_eq!(view.result, candidate.result, "{view:#?}");
    assert!(view.parents.contains(&parked), "{:?}", view.parents);
    assert!(view.parents.contains(&landed), "{:?}", view.parents);
    let event = arbitrated_event(&remote, &parked).await?;
    verify_arbitration(
        wire::object_id("parked", &parked)?,
        &Arbitration::Resolved(wire::object_id("candidate", &first)?),
        &event_signature(&event)?,
        &key.public(),
    )?;
    let head = remote.head(proto::HeadRequest {}).await?;
    assert_eq!(head.change.as_deref(), Some(resolution.as_str()));
    running.stop().await;
    Ok(())
}
