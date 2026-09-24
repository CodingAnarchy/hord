//! `--server`: the concurrency simulation against a spawned `hord serve`
//! (spec §12 M4 server foundation, ADR 0024).
//!
//! The cargo base is bootstrapped and the plan made locally, as in the
//! in-process run. Then this binary starts itself as the server in a child
//! process ([`serve_child`]: `hord_server` over the repository, with the
//! stub verifier, on a loopback port). 100 agents, each a tokio task with
//! its own [`RemoteRepo`] connection, work in InMemory workspaces of one
//! client-side cache repository that reads the server's objects lazily
//! ([`hord_remote::open_cache`]). Each agent pushes its proposal's objects
//! over its own connection and submits in the seeded order (or as soon as
//! it is done with `--racy-submit`). The server's lander task lands them.
//!
//! A flight recorder follows the event stream from cursor 0 until every
//! change has settled and writes it as a recording
//! ([`hord_api::recording`]). After the server stops, the store is opened
//! here: the recording is stored as a `Blob` object (its id is in the
//! report) and read back, and the queue goes through the M3 oracle
//! ([`sim::analyze`]). Throughput is changes over the time from the first
//! agent's submission to the last settlement of an agent's change. Gates
//! are M3's.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hord_api::proto::event::Kind;
use hord_api::{RepoBackend, proto, wire};
use hord_core::{ChangeId, RepoPath};
use hord_remote::{RemoteRepo, open_cache, push_change};
use hord_txn::Repo;
use serde::Serialize;
use tokio_stream::StreamExt;

use crate::sim;

/// What `--server` measured, besides the oracle's [`sim::SimReport`].
#[derive(Debug, Serialize)]
pub(crate) struct ServerReport {
    pub url: String,
    pub client_connections: usize,
    pub objects_pushed: usize,
    pub push_secs: f64,
    /// Events in the flight recording.
    pub recorded_events: usize,
    /// ObjectId of the recording (a `Blob`) in the repository's store.
    pub recording: String,
    /// The recording read back: its landed events match the queue.
    pub recording_replays: bool,
    pub sim: sim::SimReport,
}

/// The child process: serve `dir` on a loopback port, print
/// `LISTENING <addr>`, and stop when stdin closes.
pub(crate) fn serve_child(dir: &Path, strict_reads: bool, policy: bool) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let hosts =
            hord_server::Hosts::open_repo(dir, sim::repo_options(strict_reads, policy)).await?;
        let listener = hord_server::Server::bind(
            "127.0.0.1:0".parse()?,
            &hord_server::ServeOptions::default(),
        )
        .await?;
        println!("LISTENING {}", listener.local_addr()?);
        std::io::stdout().flush()?;
        let server = hord_server::Server::new(hosts, hord_server::ServerConfig::default());
        let stdin_closed = async {
            let _ = tokio::task::spawn_blocking(|| {
                let mut sink = String::new();
                while std::io::stdin().read_line(&mut sink).is_ok_and(|n| n > 0) {
                    sink.clear();
                }
            })
            .await;
        };
        server.serve(listener, stdin_closed).await?;
        anyhow::Ok(())
    })
}

/// A spawned server; dropping it closes its stdin, which stops it.
struct ServerProcess {
    child: Child,
    url: String,
}

impl ServerProcess {
    fn spawn(dir: &Path, strict_reads: bool, policy: bool) -> Result<Self> {
        let exe = std::env::current_exe().context("current exe")?;
        let mut command = Command::new(exe);
        command
            .arg("--internal-serve")
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if strict_reads {
            command.arg("--strict-reads");
        }
        if policy {
            command.arg("--policy");
        }
        let mut child = command.spawn().context("spawn the server")?;
        let stdout = child.stdout.take().context("server stdout")?;
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .context("read the server's address")?;
        let addr = line
            .trim()
            .strip_prefix("LISTENING ")
            .with_context(|| format!("unexpected server output {line:?}"))?;
        Ok(Self {
            url: format!("http://{addr}"),
            child,
        })
    }

    fn stop(mut self) -> Result<()> {
        drop(self.child.stdin.take());
        let status = self.child.wait()?;
        if !status.success() {
            bail!("the server exited with {status}");
        }
        Ok(())
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

pub(crate) async fn run(
    scratch: &Path,
    files: Vec<(RepoPath, Vec<u8>)>,
    corpus_files: &[(String, Vec<u8>)],
    config: &sim::SimConfig,
) -> Result<ServerReport> {
    let dir = scratch.join("server");
    let (base, snapshot, plan, public_dir) = {
        let store = hord_store::Store::create(&dir).context("create store")?;
        let repo =
            Repo::from_store(store, sim::repo_options(config.strict_reads, config.policy)).await?;
        let base = repo
            .bootstrap(files, sim::intent("import cargo"), sim::actor("m3-seed"))
            .await?;
        let snapshot = sim::load_snapshot(&repo, corpus_files).await?;
        let mut plan = sim::plan(
            &snapshot,
            config.agents,
            config.overlap_percent,
            config.seed,
        )?;
        let (base, public_dir) = if config.policy {
            let (base, dir) = crate::policy::install(&repo, base, &mut plan, &snapshot).await?;
            (base, Some(dir))
        } else {
            (base, None)
        };
        (base, snapshot, plan, public_dir)
    };
    let server = ServerProcess::spawn(&dir, config.strict_reads, config.policy)?;
    let url = server.url.clone();
    eprintln!(
        "[server] {} agents, one connection each, against {url}",
        config.agents
    );

    // Flight recorder: everything from cursor 0, until every change settles.
    let recorder_remote = RemoteRepo::connect(&url).await?;
    let mut stream = recorder_remote
        .events(proto::EventsRequest { from: Some(0) })
        .await?;
    let agents = config.agents;
    let (proposed_tx, mut proposed_rx) = tokio::sync::mpsc::unbounded_channel::<ChangeId>();
    // When the first agent submits: the clock starts here, not at a
    // `Submitted` event of the history the recording replays first.
    let submitting: Arc<std::sync::OnceLock<Instant>> = Arc::default();
    let started_submitting = Arc::clone(&submitting);
    let recorder = tokio::spawn(async move {
        let header = proto::RecordingHeader {
            repo: "m3-server".into(),
            started_at_ms: now_ms(),
            from_cursor: 0,
            description: format!("hord-eval-m3 --server: {agents} agents"),
            ..Default::default()
        };
        let mut recording = hord_api::recording::Recorder::new(Vec::new(), header)?;
        let mut expected: HashSet<String> = HashSet::new();
        let mut settled: HashSet<String> = HashSet::new();
        let mut settled_at: std::collections::HashMap<String, Instant> =
            std::collections::HashMap::new();
        let mut count = 0;
        // Settled ids include the bootstrap's own landing, so only the
        // subset test below ends the loop.
        loop {
            while let Ok(change) = proposed_rx.try_recv() {
                expected.insert(wire::id(change));
            }
            let next = tokio::time::timeout(Duration::from_secs(300), stream.next())
                .await
                .context("no event for 300s")?
                .context("event stream ended")??;
            recording.record(&next)?;
            count += 1;
            let settled_change = match next.event.as_ref().and_then(|e| e.kind.as_ref()) {
                Some(Kind::Landed(l)) => {
                    Some(l.submitted.clone().unwrap_or_else(|| l.change.clone()))
                }
                Some(Kind::Parked(p)) => Some(p.change.clone()),
                Some(Kind::Rejected(r)) => Some(r.change.clone()),
                _ => None,
            };
            if let Some(change) = settled_change
                && settled.insert(change.clone())
            {
                settled_at.insert(change, Instant::now());
            }
            while let Ok(change) = proposed_rx.try_recv() {
                expected.insert(wire::id(change));
            }
            if expected.len() == agents && expected.is_subset(&settled) {
                break;
            }
        }
        let last_settle = expected
            .iter()
            .filter_map(|c| settled_at.get(c))
            .max()
            .copied()
            .unwrap_or_else(Instant::now);
        let first_submit = started_submitting.get().copied().unwrap_or(last_settle);
        let land = last_settle.duration_since(first_submit);
        anyhow::Ok((recording.finish()?, count, land))
    });

    // The shared client-side cache, reading the server's objects lazily.
    let cache_remote = RemoteRepo::connect(&url).await?;
    let cache = open_cache(
        &scratch.join("client"),
        cache_remote,
        sim::repo_options(config.strict_reads, config.policy),
    )
    .await?;
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for agent in plan.agents.iter().cloned() {
        let remote = RemoteRepo::connect(&url).await?;
        let cache = cache.clone();
        let seed = config.seed;
        tasks.spawn(async move {
            let index = agent.index;
            let (run, change) = sim::agent_work(&cache, base, agent, seed).await?;
            let push_started = Instant::now();
            let pushed = push_change(&remote, &cache, change).await?;
            anyhow::Ok((index, run, change, remote, pushed, push_started.elapsed()))
        });
    }
    // Everything is proposed and pushed first, as the in-process run
    // proposes before `land_local`; then each agent submits over its own
    // connection, in the seeded order or, racy, in completion order.
    let mut done_agents = Vec::with_capacity(config.agents);
    while let Some(joined) = tasks.join_next().await {
        done_agents.push(joined.context("agent task")??);
    }
    let agents_secs = started.elapsed().as_secs_f64();
    if !config.racy_submit {
        let positions = sim::submit_positions(config);
        done_agents.sort_by_key(|a| positions[a.0]);
    }
    for (_, _, change, _, _, _) in &done_agents {
        proposed_tx.send(*change).ok();
    }
    drop(proposed_tx);
    let mut runs = Vec::with_capacity(config.agents);
    let mut objects_pushed = 0;
    let mut push_secs = 0.0;
    submitting.get_or_init(Instant::now);
    for (_, run, change, remote, pushed, push) in done_agents {
        remote
            .submit(proto::SubmitRequest {
                change: wire::id(change),
            })
            .await?;
        objects_pushed += pushed;
        push_secs += push.as_secs_f64();
        runs.push(run);
    }
    let (recording, recorded_events, land) = recorder.await.context("recorder task")??;
    drop(cache);
    server.stop()?;

    // The server is gone: open its store here for the oracle.
    let repo = Repo::open_with(&dir, sim::repo_options(config.strict_reads, config.policy)).await?;
    let mut done = repo.queue().await?;
    done.retain(|e| runs.iter().any(|r| r.change == e.change));
    if done.len() != config.agents {
        bail!("the queue has {} of {} changes", done.len(), config.agents);
    }
    let recording_id = repo.store().put_object(&hord_core::Blob::new(recording))?;
    repo.store().flush()?;
    let stored: hord_core::Blob = repo.store().get_object(recording_id)?;
    let (_, events) = hord_api::recording::parse(stored.bytes.as_slice())?;
    // The recording replays: it names every landed change of the queue,
    // and every parked or rejected one.
    let mut recorded_landed = HashSet::new();
    let mut recorded_stopped = HashSet::new();
    for event in &events {
        match event.event.as_ref().and_then(|e| e.kind.as_ref()) {
            Some(Kind::Landed(l)) => {
                recorded_landed.insert(l.change.clone());
            }
            Some(Kind::Parked(p)) => {
                recorded_stopped.insert(p.change.clone());
            }
            Some(Kind::Rejected(r)) => {
                recorded_stopped.insert(r.change.clone());
            }
            _ => {}
        }
    }
    let mut missing = Vec::new();
    for entry in &done {
        let found = match &entry.status {
            hord_txn::QueueStatus::Landed { landed } => {
                recorded_landed.contains(&wire::id(*landed))
            }
            _ => recorded_stopped.contains(&wire::id(entry.change)),
        };
        if !found {
            missing.push(wire::id(entry.change));
        }
    }
    if !missing.is_empty() {
        eprintln!("[server] recording lacks the outcome of {missing:?}");
    }
    let recording_replays = events.len() == recorded_events && missing.is_empty();
    let sim = sim::analyze(
        &repo,
        &snapshot,
        &plan,
        config,
        runs,
        done,
        agents_secs,
        land,
        public_dir.as_deref(),
    )
    .await?;
    Ok(ServerReport {
        url,
        client_connections: config.agents,
        objects_pushed,
        push_secs,
        recorded_events,
        recording: recording_id.to_hex(),
        recording_replays,
        sim,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
