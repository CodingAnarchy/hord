//! Stopping `hord serve` during a long verification: the verification is
//! cancelled (its process group killed, nothing recorded), `serve` returns
//! within seconds, the repository reopens at once, and the next start
//! verifies the change again.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hord_api::proto;
use hord_api::proto::repo_backend_client::RepoBackendClient;
use hord_core::{Actor, EvidenceKind, Intent, RepoPath};
use hord_server::{Hosts, ServeOptions, Server, ServerConfig};
use hord_txn::{
    BeginOptions, QueueStatus, Repo, RepoOptions, StubVerifier, Verdict, Verifier, VerifyFuture,
    VerifyRequest,
};
use hord_verify::Check;
use hord_verify_rust::CargoRunner;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir_all(&self.0) {
            eprintln!("remove temp dir {}: {err}", self.0.display());
        }
    }
}

fn temp() -> std::io::Result<Dir> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-shutdown-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    if path.exists() {
        fs::remove_dir_all(&path)?;
    }
    fs::create_dir_all(&path)?;
    Ok(Dir(path))
}

/// A verification that runs `sleep 120` through the cargo runner, under
/// the context's cancel flag, after writing its shell's pid to `pidfile`.
struct Sleeper {
    pidfile: PathBuf,
}

impl Verifier for Sleeper {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        let pidfile = self.pidfile.clone();
        Box::pin(async move {
            let runner = CargoRunner {
                timeout: None,
                idle_timeout: None,
                cancel: request.context.cancel(),
                ..CargoRunner::default()
            };
            let check = Check {
                requirement: "test".into(),
                kind: EvidenceKind::Test,
                qualifier: None,
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    format!("echo $$ > {}; sleep 120", pidfile.display()),
                ],
                env: BTreeMap::new(),
                dir: RepoPath::default(),
                scope: None,
            };
            let run = move || runner.run(Path::new("/"), &check);
            let task = match request.context.tasks() {
                Some(tasks) => tasks.spawn_blocking(run),
                None => tokio::task::spawn_blocking(run),
            };
            match task.await {
                Ok(Ok(out)) if out.success() => Verdict::Pass {
                    evidence: Vec::new(),
                },
                other => Verdict::Fail {
                    evidence: Vec::new(),
                    reason: format!("{other:?}"),
                },
            }
        })
    }
}

struct Running {
    addr: std::net::SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

async fn serve(root: &Path, verifier: Arc<dyn Verifier>) -> TestResult<Running> {
    let hosts = Hosts::open_repo(
        root,
        RepoOptions {
            verifier: Some(verifier),
            ..RepoOptions::default()
        },
    )
    .await?;
    let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
    let addr = listener.local_addr()?;
    let (stop, stopped) = oneshot::channel::<()>();
    let server = Server::new(hosts, ServerConfig::default());
    let task = tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .expect("serve the test server");
    });
    Ok(Running { addr, stop, task })
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_during_a_long_verification_cancels_it_and_releases_the_repository() -> TestResult
{
    let dir = temp()?;
    let change = {
        let repo = Repo::create(&dir.0).await?;
        let lib: RepoPath = "src/lib.rs".parse()?;
        let actor = Actor::Human { id: "ada".into() };
        repo.bootstrap(
            vec![(lib.clone(), b"pub fn one() -> u32 {\n    1\n}\n".to_vec())],
            Intent::from_summary("seed"),
            actor.clone(),
        )
        .await?;
        let mut ws = repo.begin(BeginOptions::at_head(actor)).await?;
        ws.write_file(&lib, b"pub fn one() -> u32 {\n    11\n}\n".to_vec())
            .await?;
        ws.propose(Intent::from_summary("eleven")).await?.change
    };

    let pidfile = dir.0.join("verifier.pid");
    let running = serve(
        &dir.0,
        Arc::new(Sleeper {
            pidfile: pidfile.clone(),
        }),
    )
    .await?;
    let mut client = RepoBackendClient::connect(format!("http://{}", running.addr)).await?;
    client
        .submit(proto::SubmitRequest {
            change: change.to_hex(),
        })
        .await?;
    // Wait until the verification's command is running.
    let started = Instant::now();
    let pid = loop {
        if let Ok(pid) = fs::read_to_string(&pidfile)
            && !pid.trim().is_empty()
        {
            break pid.trim().to_owned();
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err("the verification never started".into());
        }
        sleep(Duration::from_millis(20)).await;
    };
    assert!(alive(&pid), "the verification command runs");
    drop(client);

    let stopping = Instant::now();
    let _ = running.stop.send(());
    timeout(Duration::from_secs(8), running.task).await??;
    let took = stopping.elapsed();
    assert!(took < Duration::from_secs(5), "serve took {took:?} to stop");
    // Its process group was killed, not left to run out.
    let gone = Instant::now();
    while alive(&pid) && gone.elapsed() < Duration::from_secs(2) {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(&pid), "the verification command was killed");

    // The repository reopens at once, with nothing recorded: still queued.
    {
        let repo = Repo::open(&dir.0).await?;
        let entry = repo.status(change).await?;
        assert!(
            matches!(entry.status, QueueStatus::Queued),
            "{:?}",
            entry.status
        );
    }

    // The next start verifies it again, and it lands.
    let running = serve(&dir.0, Arc::new(StubVerifier)).await?;
    let mut client = RepoBackendClient::connect(format!("http://{}", running.addr)).await?;
    let started = Instant::now();
    loop {
        let queue = client
            .queue(proto::QueueQuery {
                change: Some(change.to_hex()),
                ..Default::default()
            })
            .await?
            .into_inner();
        if queue
            .entries
            .last()
            .is_some_and(|e| e.status() == proto::QueueStatus::Landed)
        {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err(format!("never landed: {:?}", queue.entries).into());
        }
        sleep(Duration::from_millis(50)).await;
    }
    drop(client);
    let _ = running.stop.send(());
    running.task.await?;
    Ok(())
}
