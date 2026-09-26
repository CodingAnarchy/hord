//! `hord audit`'s `Audit` service on a real repository (spec §12 M6): a
//! signed change with a signed review lands through an authenticated
//! server, and the window that holds it audits clean, both through the
//! server and in process on the store once the server stops. The unsigned,
//! unevidenced seed before it does not.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hord_api::auth::Scope;
use hord_api::proto::{AuditCriterion, AuditOrigin};
use hord_api::{AuditBackend, RepoBackend, proto, wire};
use hord_core::sign::{self, SigningKey};
use hord_core::{Actor, Evidence, EvidenceKind, EvidenceResult, Intent, RepoPath, Timestamp};
use hord_remote::{RemoteRepo, open_cache, push_change};
use hord_server::{AuthStore, Hosts, LocalAudit, ServeOptions, Server, ServerConfig};
use hord_txn::{Base, BeginOptions, LocalRepo, Repo, RepoOptions};
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
