//! The git bridge against a `hord serve` that requires tokens (ADR 0037):
//! a `bridge` token submits a pull request's change on behalf of its git
//! author, and may do nothing else a person or an agent does.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::{fs, io};

use gix::bstr::BString;
use gix::refs::transaction::PreviousValue;
use hord_api::auth::Scope;
use hord_api::proto::event::Kind;
use hord_api::proto::{self, QueueStatus};
use hord_api::{ApiError, RepoBackend, wire};
use hord_core::sign::{self, SigningKey};
use hord_core::{
    Actor, Bytes, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, Intent, IntentRef,
    RepoPath, Timestamp,
};
use hord_git::sync::{Bridge, BridgeOptions, PullRequest, PullRequests, ScriptedPulls};
use hord_remote::RemoteRepo;
use hord_server::{AuthStore, Hosts, ServeOptions, Server, ServerConfig};
use hord_txn::{Repo, RepoOptions, StubVerifier};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, timeout};
use tokio_stream::StreamExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hord-bridge-auth-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stub() -> RepoOptions {
    RepoOptions {
        verifier: Some(Arc::new(StubVerifier)),
        ..RepoOptions::default()
    }
}

/// A server requiring tokens from `auth`; stopped when the sender drops.
async fn serve(repo: &Path, auth: &Path) -> TestResult<(String, oneshot::Sender<()>)> {
    let hosts = Hosts::open_repo(repo, stub()).await?;
    let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = Server::new(hosts, ServerConfig::default()).with_auth(AuthStore::open(auth)?);
    let (stop, stopped) = oneshot::channel::<()>();
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

fn denied(result: Result<impl Debug, ApiError>, needle: &str) -> TestResult {
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

async fn settle(remote: &RemoteRepo, change: &str) -> TestResult<proto::QueueEntry> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let queue = remote
            .queue(proto::QueueQuery {
                change: Some(change.to_owned()),
                ..Default::default()
            })
            .await?;
        if let Some(entry) = queue.entries.last()
            && !matches!(entry.status(), QueueStatus::Queued | QueueStatus::Replaying)
        {
            return Ok(entry.clone());
        }
        if Instant::now() > deadline {
            return Err(format!("{change} did not settle").into());
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// A commit on the mirror: `parent` with `notes.txt` rewritten, by Ada.
fn pull_commit(mirror: &Path, parent: gix::ObjectId, git_ref: &str) -> TestResult<gix::ObjectId> {
    let repo = gix::open(mirror)?;
    let parent_tree = repo.find_commit(parent)?.tree_id()?.detach();
    let mut tree: gix::objs::Tree = repo.find_tree(parent_tree)?.decode()?.into();
    let blob = repo.write_blob(b"alpha\nbeta (by Ada)\n")?.detach();
    tree.entries
        .iter_mut()
        .find(|e| e.filename == "notes.txt")
        .ok_or("notes.txt is on main")?
        .oid = blob;
    let tree = repo.write_object(&tree)?.detach();
    let author = gix::actor::Signature {
        name: "Ada Lovelace".into(),
        email: "ada@example.com".into(),
        time: gix::date::Time::new(1_800_000_000, 0),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: [parent].into_iter().collect(),
        author: author.clone(),
        committer: author,
        encoding: None,
        message: BString::from("rewrite beta\n"),
        extra_headers: Vec::new(),
    };
    let id = repo.write_object(&commit)?.detach();
    repo.reference(git_ref, id, PreviousValue::Any, "test")?;
    Ok(id)
}

fn agent(id: &str) -> Actor {
    Actor::Agent {
        id: id.into(),
        model: "m1".into(),
        model_hash: Bytes::default(),
        harness: "h1".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bridge_token_lands_a_pull_request_as_its_git_author_and_nothing_else() -> TestResult {
    let dir = TempDir::new("vouch")?;
    let repo_dir = dir.0.join("repo");
    let repo = Repo::create_with(&repo_dir, stub()).await?;
    repo.bootstrap(
        vec![("notes.txt".parse::<RepoPath>()?, b"alpha\nbeta\n".to_vec())],
        Intent::from_summary("seed"),
        Actor::Human { id: "seed".into() },
    )
    .await?;
    drop(repo);
    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "root", "pw", &[Scope::Admin])?;
    let (url, _stop) = serve(&repo_dir, &auth).await?;

    let anonymous = RemoteRepo::connect(&url).await?;
    let root_token = anonymous
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
    let minted = root
        .auth()
        .mint_token(mint("git-bridge", &["read", "bridge"]))
        .await?;
    let bridge_key = minted.key_id.clone();
    let bridge_remote = RemoteRepo::connect_with_token(&url, &minted.token).await?;
    let proposer = root
        .auth()
        .mint_token(mint("bot-1", &["read", "propose"]))
        .await?;
    let proposer_key = SigningKey::from_pem(&proposer.private_key_pem)?;
    let proposer_remote = RemoteRepo::connect_with_token(&url, &proposer.token).await?;

    // The bridge exports main, then submits Ada's pull request as Ada.
    let mirror = dir.0.join("mirror.git");
    gix::init_bare(&mirror)?;
    let pulls = Arc::new(ScriptedPulls::new());
    let mut options = BridgeOptions::new(mirror.display().to_string(), dir.0.join("bridge"));
    options.voucher = Some(bridge_key.clone());
    let backend: Arc<dyn RepoBackend> = Arc::new(bridge_remote.clone());
    let source: Arc<dyn PullRequests> = pulls.clone();
    let mut bridge = Bridge::open(backend, Some(source), options).await?;
    let first = bridge.sync_once().await?;
    let check = first.checks.last().ok_or("a check after the push")?;
    assert!(check.recorded.is_some(), "RecordBridgeCheck takes bridge");

    let main = gix::open(&mirror)?
        .find_reference("refs/heads/main")?
        .id()
        .detach();
    let head = pull_commit(&mirror, main, "refs/pull/3/head")?;
    pulls.open(PullRequest {
        number: 3,
        title: "Rewrite beta".into(),
        body: String::new(),
        head_sha: head.to_string(),
        git_ref: "refs/pull/3/head".into(),
        url: String::new(),
    });
    let report = bridge.sync_once().await?;
    let (_, change) = *report.proposed.first().ok_or("the pull was proposed")?;
    let entry = settle(&bridge_remote, &wire::id(change)).await?;
    assert_eq!(entry.status(), QueueStatus::Landed, "{entry:?}");
    bridge.sync_once().await?;
    assert!(!pulls.is_open(3), "closed once on main");

    let reply = bridge_remote
        .get_objects(proto::GetObjectsRequest {
            ids: vec![wire::id(change)],
        })
        .await?;
    let vouched: ChangeRecord = hord_encoding::decode(&reply.objects[0].cbor)?;
    let ada = Actor::Human {
        id: "Ada Lovelace <ada@example.com>".into(),
    };
    assert_eq!(vouched.provenance.actor, ada);
    assert_eq!(
        vouched.provenance.voucher.as_deref(),
        Some(bridge_key.as_str())
    );
    assert!(vouched.signature.is_none());

    // The Submitted event names the voucher.
    let mut events = bridge_remote
        .events(proto::EventsRequest { from: Some(0) })
        .await?;
    let mut voucher = None;
    while let Ok(Some(item)) = timeout(Duration::from_millis(300), events.next()).await {
        if let Some(Kind::Submitted(s)) = item?.event.and_then(|e| e.kind)
            && s.change == wire::id(change)
        {
            voucher = s.voucher;
        }
    }
    assert_eq!(voucher.as_deref(), Some(bridge_key.as_str()));

    // Variations on the vouched record, each refused.
    let with = |edit: &dyn Fn(&mut ChangeRecord)| {
        let mut record = vouched.clone();
        record.intent.summary = "a variation".into();
        edit(&mut record);
        record
    };
    denied(
        put_and_submit(&bridge_remote, &with(&|r| r.intent.refs.clear())).await,
        "git commit ref",
    )?;
    denied(
        put_and_submit(
            &bridge_remote,
            &with(&|r| r.provenance.actor = agent("git-bridge")),
        )
        .await,
        "only a git author's change",
    )?;
    denied(
        put_and_submit(&bridge_remote, &with(&|r| r.provenance.voucher = None)).await,
        "voucher",
    )?;
    let other_key = proposer_key.public().key_id();
    denied(
        put_and_submit(
            &bridge_remote,
            &with(&|r| r.provenance.voucher = Some(other_key.clone())),
        )
        .await,
        "is bound to agent bot-1",
    )?;
    let mut signed = with(&|_| {});
    sign::sign_change(&mut signed, &proposer_key)?;
    denied(put_and_submit(&bridge_remote, &signed).await, "unsigned")?;
    // Only intent refs of the git kind count.
    denied(
        put_and_submit(
            &bridge_remote,
            &with(&|r| {
                r.intent.refs = vec![IntentRef::Url {
                    url: "https://example.com".into(),
                }]
            }),
        )
        .await,
        "git commit ref",
    )?;

    // A bridge token does not review or arbitrate.
    let mut review = Evidence {
        kind: EvidenceKind::Review,
        qualifier: Some("human".into()),
        snapshot: vouched.result,
        toolchain: vouched.provenance.toolchain,
        command: "review".into(),
        scope: None,
        result: EvidenceResult::Pass,
        log: None,
        cost_ms: 0,
        produced_by: ada.clone(),
        produced_at: Timestamp::from_millis(1),
        signature: None,
    };
    sign::sign_evidence(&mut review, &proposer_key)?;
    denied(
        bridge_remote
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(change),
                evidence: hord_encoding::encode(&review)?,
            })
            .await,
        "requires scope propose or review",
    )?;
    denied(
        bridge_remote
            .arbitrate(proto::ArbitrateRequest {
                change: wire::id(change),
                action: Some(proto::Arbitration {
                    action: Some(proto::arbitration::Action::PickOurs(true)),
                }),
                ..Default::default()
            })
            .await,
        "requires scope arbitrate",
    )?;

    // A token without `bridge` cannot claim a git author, vouched or not.
    denied(
        put_and_submit(&proposer_remote, &with(&|_| {})).await,
        "lacks scope bridge",
    )?;
    denied(
        put_and_submit(&proposer_remote, &with(&|r| r.provenance.voucher = None)).await,
        "cannot submit a change authored by human Ada Lovelace",
    )?;
    Ok(())
}
