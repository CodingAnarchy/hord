//! `hord audit`'s `Audit` service on a real repository (spec §12 M6): a
//! signed change with a signed review lands through an authenticated
//! server, and the window that holds it audits clean, both through the
//! server and in process on the store once the server stops. The unsigned,
//! unevidenced seed before it does not. The git bridge's recorded checks
//! and a pull request it vouched for are audited too.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hord_api::auth::Scope;
use hord_api::proto::{AuditCriterion, AuditOrigin};
use hord_api::{AuditBackend, RepoBackend, proto, wire};
use hord_core::sign::{self, SigningKey};
use hord_core::{
    Actor, Bytes, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, Intent, IntentRef,
    RepoPath, Timestamp,
};
use hord_remote::{RemoteRepo, open_cache, push_change};
use hord_server::{AuthStore, Hosts, LocalAudit, ServeOptions, Server, ServerConfig};
use hord_txn::{Base, BeginOptions, LocalRepo, Repo, RepoOptions, StubVerifier};
use tokio_stream::StreamExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("remove temp dir {}: {err}", self.0.display());
        }
    }
}

fn temp(tag: &str) -> std::io::Result<Dir> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-remote-audit-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    Ok(Dir(path))
}

fn now_ms() -> TestResult<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn window(since_ms: u64) -> TestResult<proto::AuditRequest> {
    Ok(proto::AuditRequest {
        since_ms,
        until_ms: Some(now_ms()? + 1),
        max_bridge_gap_ms: 0,
        require_bridge: false,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_reviewed_landing_audits_clean_remotely_and_locally() -> TestResult {
    let dir = temp("clean")?;
    let repo_dir = dir.0.join("repo");
    let lib: RepoPath = "src/lib.rs".parse()?;
    let repo = Repo::create(&repo_dir).await?;
    repo.bootstrap(
        vec![(lib.clone(), b"pub fn a() -> u32 {\n    1\n}\n".to_vec())],
        Intent::from_summary("seed"),
        Actor::Human { id: "seed".into() },
    )
    .await?;
    drop(repo);
    // The seed was bootstrapped, not landed: the window starts after it.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let since = now_ms()?;

    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(
        &auth,
        "ada",
        "pw",
        &[Scope::Read, Scope::Propose, Scope::Review("human".into())],
    )?;
    let hosts = Hosts::open_repo(&repo_dir, RepoOptions::default()).await?;
    let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = Server::new(hosts, ServerConfig::default()).with_auth(AuthStore::open(&auth)?);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
    });

    let ada_key = SigningKey::generate()?;
    let token = RemoteRepo::connect(&url)
        .await?
        .auth()
        .login(proto::LoginRequest {
            user: "ada".into(),
            password: "pw".into(),
            key_id: ada_key.public().key_id(),
        })
        .await?
        .token;
    let remote = RemoteRepo::connect_with_token(&url, &token).await?;
    let ada = Actor::Human { id: "ada".into() };

    // Ada proposes against head in a client-side cache, signs, and pushes.
    let head = remote.head(proto::HeadRequest {}).await?;
    let head = wire::object_id("head", head.change.as_deref().ok_or("head")?)?;
    let client = temp("client")?;
    let cache = open_cache(&client.0, remote.clone(), RepoOptions::default()).await?;
    let mut ws = cache
        .begin(BeginOptions {
            base: Base::Change(head),
            ..BeginOptions::at_head(ada.clone())
        })
        .await?;
    ws.write_file(&lib, "pub fn a() -> u32 {\n    2\n}\n")
        .await?;
    let proposal = ws.propose(Intent::from_summary("a is 2")).await?;
    push_change(&remote, &cache, proposal.change).await?;
    let mut record = proposal.record;
    sign::sign_change(&mut record, &ada_key)?;
    let bytes = hord_encoding::encode(&record)?;
    let change = wire::verified_object(&wire::object(bytes.clone()))?;
    remote
        .put_objects(proto::PutObjectsRequest {
            objects: vec![wire::object(bytes)],
        })
        .await?;

    // Her passing check and her signed review of its result.
    for (kind, qualifier) in [
        (EvidenceKind::Check, None),
        (EvidenceKind::Review, Some("human")),
    ] {
        let mut evidence = Evidence {
            kind,
            qualifier: qualifier.map(str::to_owned),
            snapshot: record.result,
            toolchain: record.provenance.toolchain,
            command: "cargo check".into(),
            scope: None,
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 1,
            produced_by: ada.clone(),
            produced_at: Timestamp::from_millis(now_ms()?),
            signature: None,
        };
        sign::sign_evidence(&mut evidence, &ada_key)?;
        remote
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(change),
                evidence: hord_encoding::encode(&evidence)?,
            })
            .await?;
    }

    let mut events = remote
        .events(proto::EventsRequest { from: Some(0) })
        .await?;
    remote
        .submit(proto::SubmitRequest {
            change: wire::id(change),
        })
        .await?;
    let landed = loop {
        let envelope = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await?
            .ok_or("event stream ended")??;
        match envelope.event.and_then(|e| e.kind) {
            Some(proto::event::Kind::Landed(landed))
                if landed.change == wire::id(change)
                    || landed.submitted == Some(wire::id(change)) =>
            {
                break landed;
            }
            Some(proto::event::Kind::Rejected(r)) => return Err(r.reason.into()),
            Some(proto::event::Kind::Parked(p)) => return Err(p.detail.into()),
            _ => {}
        }
    };
    drop(events);

    // Through the server: one signed change, one review, keys bound.
    let report = remote.audit().audit_log(window(since)?).await?;
    assert!(report.ok, "{:#?}", report.violations);
    assert!(report.bindings_checked);
    assert_eq!(report.changes.len(), 1, "{report:#?}");
    let audited = &report.changes[0];
    assert_eq!(audited.change, landed.change);
    assert_eq!(audited.origin(), AuditOrigin::Signed);
    assert_eq!(audited.passing_evidence, 2);
    assert_eq!(audited.summary, "a is 2");
    assert_eq!(report.reviews, 1);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.detail == "no bridge checks recorded"),
        "{:#?}",
        report.notes
    );
    // Requiring the bridge makes its absence a violation.
    let mut strict = window(since)?;
    strict.require_bridge = true;
    let report = remote.audit().audit_log(strict).await?;
    assert!(!report.ok);
    assert_eq!(report.violations[0].criterion(), AuditCriterion::BridgeGap);
    // A window from before the bootstrap sees the seed: an unsigned
    // import with no evidence.
    let report = remote.audit().audit_log(window(0)?).await?;
    assert_eq!(report.changes.len(), 2);
    assert_eq!(
        report
            .violations
            .iter()
            .map(|v| v.criterion())
            .collect::<Vec<_>>(),
        [AuditCriterion::Provenance, AuditCriterion::Evidence]
    );
    drop((remote, cache));

    // In process on the store, with the same bindings: the same verdict.
    let _ = stop.send(());
    serving.await??;
    let repo = Repo::open(&repo_dir).await?;
    let local = LocalAudit::new(
        Arc::new(LocalRepo::without_lander(repo)),
        Some(Arc::new(AuthStore::open(&auth)?)),
    );
    let report = local.audit_log(window(since)?).await?;
    assert!(report.ok, "{:#?}", report.violations);
    assert_eq!(report.changes.len(), 1);
    assert_eq!(report.changes[0].origin(), AuditOrigin::Signed);
    // Without the bindings, signatures still verify, and the report says
    // the bindings were not checked.
    drop(local);
    let unbound = LocalAudit::new(
        Arc::new(LocalRepo::without_lander(Repo::open(&repo_dir).await?)),
        None,
    );
    let report = unbound.audit_log(window(since)?).await?;
    assert!(report.ok, "{:#?}", report.violations);
    assert!(!report.bindings_checked);
    assert_eq!(report.changes[0].origin(), AuditOrigin::SignedUnbound);
    Ok(())
}

/// A CI agent that attaches signed, passing `check` evidence.
struct Ci {
    remote: RemoteRepo,
    actor: Actor,
    key: SigningKey,
}

/// Land `record` (already pushed with its objects) through `remote`, with
/// `ci`'s passing check for its result, and wait for its `Landed` event.
async fn land(remote: &RemoteRepo, ci: &Ci, record: &ChangeRecord) -> TestResult<proto::Landed> {
    let bytes = hord_encoding::encode(record)?;
    let change = wire::id(wire::verified_object(&wire::object(bytes.clone()))?);
    remote
        .put_objects(proto::PutObjectsRequest {
            objects: vec![wire::object(bytes)],
        })
        .await?;
    let mut evidence = Evidence {
        kind: EvidenceKind::Check,
        qualifier: None,
        snapshot: record.result,
        toolchain: record.provenance.toolchain,
        command: "cargo check".into(),
        scope: None,
        result: EvidenceResult::Pass,
        log: None,
        cost_ms: 1,
        produced_by: ci.actor.clone(),
        produced_at: Timestamp::from_millis(now_ms()?),
        signature: None,
    };
    sign::sign_evidence(&mut evidence, &ci.key)?;
    ci.remote
        .attach_evidence(proto::AttachEvidenceRequest {
            change: change.clone(),
            evidence: hord_encoding::encode(&evidence)?,
        })
        .await?;
    let mut events = remote.events(proto::EventsRequest { from: None }).await?;
    remote
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await?;
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await?
            .ok_or("event stream ended")??;
        match envelope.event.and_then(|e| e.kind) {
            Some(proto::event::Kind::Landed(landed))
                if landed.change == change || landed.submitted.as_ref() == Some(&change) =>
            {
                return Ok(landed);
            }
            Some(proto::event::Kind::Rejected(r)) if r.change == change => {
                return Err(r.reason.into());
            }
            Some(proto::event::Kind::Parked(p)) if p.change == change => {
                return Err(p.detail.into());
            }
            _ => {}
        }
    }
}

/// A change to `src/lib.rs` by `actor`, proposed in a client-side cache of
/// `remote` at its head, with its objects pushed; the record, unsigned.
async fn propose(
    remote: &RemoteRepo,
    cache_dir: &std::path::Path,
    actor: Actor,
    body: &str,
    summary: &str,
) -> TestResult<ChangeRecord> {
    let head = remote.head(proto::HeadRequest {}).await?;
    let head = wire::object_id("head", head.change.as_deref().ok_or("head")?)?;
    let cache = open_cache(cache_dir, remote.clone(), RepoOptions::default()).await?;
    let mut ws = cache
        .begin(BeginOptions {
            base: Base::Change(head),
            ..BeginOptions::at_head(actor)
        })
        .await?;
    ws.write_file(&"src/lib.rs".parse()?, body).await?;
    let proposal = ws.propose(Intent::from_summary(summary)).await?;
    push_change(remote, &cache, proposal.change).await?;
    Ok(proposal.record)
}

/// The git bridge's real `BridgeChecked` events (ADR 0036), recorded with
/// its `bridge` token, and a pull request it submitted on its author's
/// behalf (ADR 0037). Time is scaled down: the allowed gap is 1.5 s, not an
/// hour. Checks at a steady pace audit clean; a pause longer than the gap
/// and a diverged check fail. The vouched change is counted apart from the
/// signed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_checks_and_a_vouched_pull_request_are_audited() -> TestResult {
    const GAP_MS: u64 = 1_500;
    let dir = temp("bridge")?;
    let repo_dir = dir.0.join("repo");
    let stub = || RepoOptions {
        verifier: Some(Arc::new(StubVerifier)),
        ..RepoOptions::default()
    };
    let repo = Repo::create_with(&repo_dir, stub()).await?;
    // Head's policy requires a check: CI attaches one to each change.
    repo.bootstrap(
        vec![
            (
                "src/lib.rs".parse::<RepoPath>()?,
                b"pub fn a() -> u32 {\n    1\n}\n".to_vec(),
            ),
            (
                ".hord-policy.toml".parse::<RepoPath>()?,
                b"[land]\nrequire = [\"check\"]\n".to_vec(),
            ),
        ],
        Intent::from_summary("seed"),
        Actor::Human { id: "seed".into() },
    )
    .await?;
    drop(repo);
    tokio::time::sleep(Duration::from_millis(5)).await;
    let since = now_ms()?;

    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "root", "pw", &[Scope::Admin, Scope::Read])?;
    let hosts = Hosts::open_repo(&repo_dir, stub()).await?;
    let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = Server::new(hosts, ServerConfig::default()).with_auth(AuthStore::open(&auth)?);
    let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .expect("serve the test server");
    });
    let root_token = RemoteRepo::connect(&url)
        .await?
        .auth()
        .login(proto::LoginRequest {
            user: "root".into(),
            password: "pw".into(),
            key_id: SigningKey::generate()?.public().key_id(),
        })
        .await?
        .token;
    let root = RemoteRepo::connect_with_token(&url, &root_token).await?;
    let mint = |id: &str, scopes: &[&str]| proto::MintTokenRequest {
        agent_id: id.into(),
        model: "m1".into(),
        harness: "h1".into(),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
    };
    let bridge = root
        .auth()
        .mint_token(mint("git-bridge", &["read", "bridge"]))
        .await?;
    let bridge_remote = RemoteRepo::connect_with_token(&url, &bridge.token).await?;
    let bot = root
        .auth()
        .mint_token(mint("bot-1", &["read", "propose"]))
        .await?;
    let bot_key = SigningKey::from_pem(&bot.private_key_pem)?;
    let bot_remote = RemoteRepo::connect_with_token(&url, &bot.token).await?;
    let ci = root
        .auth()
        .mint_token(mint("ci", &["read", "propose"]))
        .await?;
    let ci = Ci {
        remote: RemoteRepo::connect_with_token(&url, &ci.token).await?,
        actor: Actor::Agent {
            id: "ci".into(),
            model: "m1".into(),
            model_hash: Bytes::default(),
            harness: "h1".into(),
        },
        key: SigningKey::from_pem(&ci.private_key_pem)?,
    };

    let check = |diverged: bool| proto::BridgeChecked {
        remote: "file:///mirror.git".into(),
        diverged,
        expected: Some("e".repeat(40)),
        actual: Some(if diverged { "d" } else { "e" }.repeat(40)),
        trigger: proto::BridgeCheckTrigger::Hourly.into(),
        detail: if diverged { "diverged" } else { "in sync" }.into(),
        head: None,
    };

    // A signed agent change, and a pull request the bridge vouches for.
    let bot_actor = Actor::Agent {
        id: "bot-1".into(),
        model: "m1".into(),
        model_hash: Bytes::default(),
        harness: "h1".into(),
    };
    let mut signed = propose(
        &bot_remote,
        &dir.0.join("bot-cache"),
        bot_actor,
        "pub fn a() -> u32 {\n    2\n}\n",
        "a is 2",
    )
    .await?;
    sign::sign_change(&mut signed, &bot_key)?;
    let signed = land(&bot_remote, &ci, &signed).await?;
    bridge_remote.record_bridge_check(check(false)).await?;

    let mut vouched = propose(
        &bridge_remote,
        &dir.0.join("bridge-cache"),
        Actor::Human {
            id: "Ada Lovelace <ada@example.com>".into(),
        },
        "pub fn a() -> u32 {\n    3\n}\n",
        "Rewrite a",
    )
    .await?;
    vouched.intent.refs = vec![IntentRef::GitCommit {
        sha: "a".repeat(40),
    }];
    vouched.provenance.voucher = Some(bridge.key_id.clone());
    let vouched = land(&bridge_remote, &ci, &vouched).await?;

    // Checks at a steady pace, well inside the gap.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(GAP_MS / 5)).await;
        bridge_remote.record_bridge_check(check(false)).await?;
    }
    let steady_until = now_ms()? + 1;
    let audit = |until_ms: u64| proto::AuditRequest {
        since_ms: since,
        until_ms: Some(until_ms),
        max_bridge_gap_ms: GAP_MS,
        require_bridge: true,
    };
    let report = root.audit().audit_log(audit(steady_until)).await?;
    assert!(report.ok, "{:#?}", report.violations);
    let bridge_report = report.bridge.unwrap_or_default();
    assert_eq!((bridge_report.checks, bridge_report.diverged), (4, 0));
    let origins: Vec<(String, AuditOrigin)> = report
        .changes
        .iter()
        .map(|c| (c.change.clone(), c.origin()))
        .collect();
    assert_eq!(
        origins,
        [
            (signed.change.clone(), AuditOrigin::Signed),
            (vouched.change.clone(), AuditOrigin::BridgeVouched),
        ]
    );

    // A pause longer than the gap, then a diverged check.
    tokio::time::sleep(Duration::from_millis(GAP_MS * 2)).await;
    bridge_remote.record_bridge_check(check(true)).await?;
    let report = root.audit().audit_log(audit(now_ms()? + 1)).await?;
    let criteria: Vec<AuditCriterion> = report.violations.iter().map(|v| v.criterion()).collect();
    assert_eq!(
        criteria,
        [AuditCriterion::BridgeDiverged, AuditCriterion::BridgeGap],
        "{:#?}",
        report.violations
    );
    assert!(report.violations[0].detail.contains(&"d".repeat(40)));
    assert_eq!(report.bridge.unwrap_or_default().diverged, 1);
    Ok(())
}
