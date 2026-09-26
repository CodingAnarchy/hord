//! `RemoteRepo` against an in-process `hord serve` that requires tokens
//! (spec §10.5.4): scopes per RPC, provenance from the token, and
//! signatures by keys bound to the token's actor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hord_api::auth::Scope;
use hord_api::{ApiError, RepoBackend, proto, wire};
use hord_core::sign::{self, SigningKey};
use hord_core::{
    Actor, Bytes, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, Intent, RepoPath, Timestamp,
};
use hord_remote::RemoteRepo;
use hord_server::{AuthStore, Hosts, ServeOptions, Server, ServerConfig};
use hord_txn::{Repo, RepoOptions};

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
        "hord-remote-auth-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    Ok(Dir(path))
}

/// A server requiring tokens from `auth`; stopped when the sender drops.
async fn serve(repo: &Path, auth: &Path) -> TestResult<(String, tokio::sync::oneshot::Sender<()>)> {
    let hosts = Hosts::open_repo(repo, RepoOptions::default()).await?;
    let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = Server::new(hosts, ServerConfig::default()).with_auth(AuthStore::open(auth)?);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .expect("serve the test server");
    });
    Ok((url, stop))
}

fn bot(model: &str) -> Actor {
    Actor::Agent {
        id: "bot-1".into(),
        model: model.into(),
        model_hash: Bytes::default(),
        harness: "h1".into(),
    }
}

fn denied(result: Result<impl std::fmt::Debug, ApiError>, needle: &str) -> TestResult {
    match result {
        Err(ApiError::PermissionDenied(message)) if message.contains(needle) => Ok(()),
        other => Err(format!("expected permission denied ({needle}), got {other:?}").into()),
    }
}

/// Store `record` on the server and submit it.
async fn put_and_submit(
    remote: &RemoteRepo,
    record: &ChangeRecord,
) -> Result<proto::SubmitResponse, ApiError> {
    let bytes = hord_encoding::encode(record).map_err(|e| ApiError::Internal(e.to_string()))?;
    let id = wire::verified_object(&wire::object(bytes.clone()))?;
    remote
        .put_objects(proto::PutObjectsRequest {
            objects: vec![wire::object(bytes)],
        })
        .await?;
    remote
        .submit(proto::SubmitRequest {
            change: wire::id(id),
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_scopes_provenance_and_signatures_are_enforced() -> TestResult {
    let dir = temp("enforced")?;
    let repo_dir = dir.0.join("repo");
    let repo = Repo::create(&repo_dir).await?;
    let seed = repo
        .bootstrap(
            vec![(
                "src/lib.rs".parse::<RepoPath>()?,
                b"pub fn a() {}\n".to_vec(),
            )],
            Intent::from_summary("seed"),
            Actor::Human { id: "seed".into() },
        )
        .await?;
    drop(repo);
    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "root", "pw", &[Scope::Admin])?;
    AuthStore::add_user(
        &auth,
        "rita",
        "pw",
        &[Scope::Read, Scope::Review("human".into())],
    )?;
    let (url, _stop) = serve(&repo_dir, &auth).await?;

    // No token, or an unknown one: refused. The schema stays public.
    let anonymous = RemoteRepo::connect(&url).await?;
    let err = anonymous.head(proto::HeadRequest {}).await;
    assert!(matches!(err, Err(ApiError::Unauthenticated(_))), "{err:?}");
    let forged = RemoteRepo::connect_with_token(&url, "hord_nope").await?;
    let err = forged.head(proto::HeadRequest {}).await;
    assert!(matches!(err, Err(ApiError::Unauthenticated(_))), "{err:?}");
    let me = anonymous.auth().who_am_i().await;
    assert!(matches!(me, Err(ApiError::Unauthenticated(_))), "{me:?}");

    // Logins: a wrong password is refused.
    let rita_key = SigningKey::generate()?;
    let bad = anonymous
        .auth()
        .login(proto::LoginRequest {
            user: "rita".into(),
            password: "nope".into(),
            key_id: rita_key.public().key_id(),
        })
        .await;
    assert!(matches!(bad, Err(ApiError::Unauthenticated(_))), "{bad:?}");
    let login = |user: &str, key: &SigningKey| proto::LoginRequest {
        user: user.into(),
        password: "pw".into(),
        key_id: key.public().key_id(),
    };
    let rita_token = anonymous
        .auth()
        .login(login("rita", &rita_key))
        .await?
        .token;
    let rita = RemoteRepo::connect_with_token(&url, &rita_token).await?;
    let root_token = anonymous
        .auth()
        .login(login("root", &SigningKey::generate()?))
        .await?
        .token;
    let root = RemoteRepo::connect_with_token(&url, &root_token).await?;

    // Scopes per RPC: a reviewer reads but cannot propose or mint; admin
    // grants nothing but minting.
    rita.head(proto::HeadRequest {}).await?;
    denied(
        rita.put_objects(proto::PutObjectsRequest { objects: vec![] })
            .await,
        "requires scope propose",
    )?;
    let mint = proto::MintTokenRequest {
        agent_id: "bot-1".into(),
        model: "m1".into(),
        harness: "h1".into(),
        scopes: vec!["read".into(), "propose".into()],
    };
    denied(
        rita.auth().mint_token(mint.clone()).await,
        "requires scope admin",
    )?;
    denied(
        root.head(proto::HeadRequest {}).await,
        "requires scope read",
    )?;
    let minted = root.auth().mint_token(mint).await?;
    let bot_key = SigningKey::from_pem(&minted.private_key_pem)?;
    assert_eq!(minted.key_id, bot_key.public().key_id());
    let agent = RemoteRepo::connect_with_token(&url, &minted.token).await?;
    let me = agent.auth().who_am_i().await?;
    assert!(me.auth_required);
    assert_eq!(me.actor, Some(wire::actor(&bot("m1"))));
    let key = agent.auth().get_key(&rita_key.public().key_id()).await?;
    assert_eq!(
        key.actor,
        Some(wire::actor(&Actor::Human { id: "rita".into() }))
    );

    // A change the agent submits must be its own and signed by its key.
    let head = agent
        .head(proto::HeadRequest {})
        .await?
        .change
        .ok_or("head")?;
    let reply = agent
        .get_objects(proto::GetObjectsRequest { ids: vec![head] })
        .await?;
    let seed_record: ChangeRecord = hord_encoding::decode(&reply.objects[0].cbor)?;
    let claim = |actor: Actor, key: Option<&SigningKey>| -> TestResult<ChangeRecord> {
        let mut record = seed_record.clone();
        record.provenance.actor = actor;
        record.intent = Intent::from_summary("claimed");
        if let Some(key) = key {
            sign::sign_change(&mut record, key)?;
        }
        Ok(record)
    };
    let human = Actor::Human { id: "rita".into() };
    denied(
        put_and_submit(&agent, &claim(human, Some(&bot_key))?).await,
        "cannot submit a change authored by human rita",
    )?;
    denied(
        put_and_submit(&agent, &claim(bot("other-model"), Some(&bot_key))?).await,
        "cannot submit a change authored by agent bot-1 (other-model",
    )?;
    denied(
        put_and_submit(&agent, &claim(bot("m1"), None)?).await,
        "unsigned",
    )?;
    denied(
        put_and_submit(&agent, &claim(bot("m1"), Some(&SigningKey::generate()?))?).await,
        "not bound to any actor",
    )?;
    denied(
        put_and_submit(&agent, &claim(bot("m1"), Some(&rita_key))?).await,
        "bound to human rita",
    )?;
    // A signature that does not match the record is invalid.
    let mut tampered = claim(bot("m1"), Some(&bot_key))?;
    tampered.intent = Intent::from_summary("changed after signing");
    let err = put_and_submit(&agent, &tampered).await;
    assert!(
        matches!(err, Err(ApiError::InvalidArgument(ref m)) if m.contains("bad signature")),
        "{err:?}"
    );
    put_and_submit(&agent, &claim(bot("m1"), Some(&bot_key))?).await?;

    // Evidence: the agent attaches its own checks, not reviews or others'.
    let evidence = |kind: EvidenceKind, qualifier: Option<&str>, by: Actor, key: &SigningKey| {
        let mut ev = Evidence {
            kind,
            qualifier: qualifier.map(str::to_owned),
            snapshot: seed_record.result,
            toolchain: seed_record.provenance.toolchain,
            command: "test".into(),
            scope: None,
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 0,
            produced_by: by,
            produced_at: Timestamp::from_millis(1),
            signature: None,
        };
        sign::sign_evidence(&mut ev, key).map(|()| ev)
    };
    let attach = |remote: &RemoteRepo, ev: &Evidence| {
        let remote = remote.clone();
        let bytes = hord_encoding::encode(ev);
        async move {
            let bytes = bytes.map_err(|e| ApiError::Internal(e.to_string()))?;
            remote
                .attach_evidence(proto::AttachEvidenceRequest {
                    change: wire::id(seed),
                    evidence: bytes,
                })
                .await
        }
    };
    let check = evidence(EvidenceKind::Check, None, bot("m1"), &bot_key)?;
    attach(&agent, &check).await?;
    let review = evidence(EvidenceKind::Review, Some("human"), bot("m1"), &bot_key)?;
    denied(attach(&agent, &review).await, "requires scope review:human")?;
    // The reviewer signs a review as themself, not as someone else.
    let as_bot = evidence(EvidenceKind::Review, Some("human"), bot("m1"), &rita_key)?;
    denied(
        attach(&rita, &as_bot).await,
        "evidence produced by agent bot-1",
    )?;
    let other_kind = evidence(
        EvidenceKind::Review,
        Some("agent-reviewer"),
        Actor::Human { id: "rita".into() },
        &rita_key,
    )?;
    denied(attach(&rita, &other_kind).await, "review:agent-reviewer")?;
    let signed = evidence(
        EvidenceKind::Review,
        Some("human"),
        Actor::Human { id: "rita".into() },
        &rita_key,
    )?;
    let attached = attach(&rita, &signed).await?;
    let stored = agent
        .get_objects(proto::GetObjectsRequest {
            ids: vec![attached.evidence],
        })
        .await?;
    let stored: Evidence = hord_encoding::decode(&stored.objects[0].cbor)?;
    sign::verify_evidence(&stored, &rita_key.public())?;
    assert!(sign::verify_evidence(&stored, &bot_key.public()).is_err());
    Ok(())
}

/// Spec §6.4 rung 3 over a server that requires tokens: arbitrating needs
/// the `arbitrate` scope, the arbiter is the token's actor, and the
/// decision is signed with a key bound to it; the `Arbitrated` event
/// carries that signature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arbitration_is_scoped_and_signed_by_the_arbiter() -> TestResult {
    use hord_txn::{Arbitration, BeginOptions};
    let dir = temp("arbitrate")?;
    let repo_dir = dir.0.join("repo");
    let repo = Repo::create(&repo_dir).await?;
    let lib: RepoPath = "src/lib.rs".parse()?;
    let seed_actor = Actor::Human { id: "seed".into() };
    repo.bootstrap(
        vec![(lib.clone(), b"pub fn a() -> u32 {\n    1\n}\n".to_vec())],
        Intent::from_summary("seed"),
        seed_actor.clone(),
    )
    .await?;
    let mut changes = Vec::new();
    for n in [2, 3] {
        let mut ws = repo
            .begin(BeginOptions::at_head(seed_actor.clone()))
            .await?;
        ws.write_file(&lib, format!("pub fn a() -> u32 {{\n    {n}\n}}\n"))
            .await?;
        let proposal = ws
            .propose(Intent::from_summary(format!("a is {n}")))
            .await?;
        repo.submit(proposal.change).await?;
        changes.push(proposal.change);
    }
    repo.land_local().await?;
    let parked = changes[1];
    assert_eq!(
        repo.status(parked).await?.status,
        hord_txn::QueueStatus::Conflicted
    );
    drop(repo);

    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "ann", "pw", &[Scope::Read, Scope::Arbitrate])?;
    AuthStore::add_user(&auth, "rita", "pw", &[Scope::Read])?;
    let (url, _stop) = serve(&repo_dir, &auth).await?;
    let anonymous = RemoteRepo::connect(&url).await?;
    let connect = |user: &'static str, key_id: String| {
        let anonymous = anonymous.clone();
        let url = url.clone();
        async move {
            let token = anonymous
                .auth()
                .login(proto::LoginRequest {
                    user: user.into(),
                    password: "pw".into(),
                    key_id,
                })
                .await?
                .token;
            Ok::<_, Box<dyn std::error::Error>>(RemoteRepo::connect_with_token(&url, &token).await?)
        }
    };
    let ann_key = SigningKey::generate()?;
    let ann = connect("ann", ann_key.public().key_id()).await?;
    let rita_key = SigningKey::generate()?;
    let rita = connect("rita", rita_key.public().key_id()).await?;

    let theirs = Arbitration::PickTheirs;
    let request =
        |signature: Option<hord_core::Signature>, arbiter: Option<Actor>| proto::ArbitrateRequest {
            change: wire::id(parked),
            action: Some(proto::Arbitration {
                action: Some(proto::arbitration::Action::PickTheirs(true)),
            }),
            arbiter: arbiter.as_ref().map(wire::actor),
            note: None,
            key_id: signature.as_ref().map(|s| s.key_id.clone()),
            signature: signature.map(|s| s.bytes.as_slice().to_vec()),
        };
    let signed = |key: &SigningKey| hord_txn::sign_arbitration(parked, &theirs, key);
    denied(
        rita.arbitrate(request(Some(signed(&rita_key)?), None))
            .await,
        "requires scope arbitrate",
    )?;
    denied(ann.arbitrate(request(None, None)).await, "unsigned")?;
    denied(
        ann.arbitrate(request(Some(signed(&SigningKey::generate()?)?), None))
            .await,
        "not bound to any actor",
    )?;
    denied(
        ann.arbitrate(request(
            Some(signed(&ann_key)?),
            Some(Actor::Human { id: "rita".into() }),
        ))
        .await,
        "cannot submit a decision by human rita",
    )?;
    // A signature over another decision does not verify.
    let ours = hord_txn::sign_arbitration(parked, &Arbitration::PickOurs, &ann_key)?;
    let err = ann.arbitrate(request(Some(ours), None)).await;
    assert!(
        matches!(err, Err(ApiError::InvalidArgument(ref m)) if m.contains("bad signature")),
        "{err:?}"
    );

    let mut events = ann.events(proto::EventsRequest { from: Some(0) }).await?;
    let reply = ann
        .arbitrate(request(Some(signed(&ann_key)?), None))
        .await?;
    let arbitrated = loop {
        use tokio_stream::StreamExt;
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(30), events.next())
            .await?
            .ok_or("event stream ended")??;
        if let Some(proto::event::Kind::Arbitrated(a)) = envelope.event.and_then(|e| e.kind) {
            break a;
        }
    };
    assert_eq!(arbitrated.change, wire::id(parked));
    assert_eq!(arbitrated.result, reply.change);
    assert_eq!(
        arbitrated.by,
        Some(wire::actor(&Actor::Human { id: "ann".into() }))
    );
    let signature = hord_core::Signature {
        key_id: arbitrated.key_id.ok_or("key id")?,
        bytes: Bytes::new(arbitrated.signature.ok_or("signature")?),
    };
    hord_txn::verify_arbitration(parked, &theirs, &signature, &ann_key.public())?;
    Ok(())
}

/// Revoking an agent's token takes effect on a running server: the operator
/// deletes it from the auth file, and within the server's reload check the
/// token is refused. Other tokens keep working, and no restart is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_token_is_refused_without_a_restart() -> TestResult {
    let dir = temp("revoke")?;
    let repo_dir = dir.0.join("repo");
    drop(Repo::create(&repo_dir).await?);
    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "root", "pw", &[Scope::Admin, Scope::Read])?;
    let (url, _stop) = serve(&repo_dir, &auth).await?;
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
    let minted = root
        .auth()
        .mint_token(proto::MintTokenRequest {
            agent_id: "bot-1".into(),
            model: "m1".into(),
            harness: "h1".into(),
            scopes: vec!["read".into()],
        })
        .await?;
    let agent = RemoteRepo::connect_with_token(&url, &minted.token).await?;
    agent.head(proto::HeadRequest {}).await?;

    // The operator deletes bot-1's token from the file.
    let mut file: toml::Table = std::fs::read_to_string(&auth)?.parse()?;
    let tokens = file
        .get_mut("token")
        .and_then(toml::Value::as_array_mut)
        .ok_or("the auth file lists tokens")?;
    let before = tokens.len();
    tokens.retain(|t| t.get("actor").and_then(|a| a.get("id")) != Some(&"bot-1".into()));
    assert_eq!(tokens.len(), before - 1);
    std::fs::write(&auth, toml::to_string(&file)?)?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match agent.head(proto::HeadRequest {}).await {
            Err(ApiError::Unauthenticated(_)) => break,
            Ok(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            other => return Err(format!("the revoked token still works: {other:?}").into()),
        }
    }
    root.head(proto::HeadRequest {}).await?;
    Ok(())
}
