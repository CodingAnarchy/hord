//! The replay protocol (spec §6.6): the lander hands a conflicted change's
//! intent, provenance, a workspace on the new base, the conflict report and
//! summary, and a budget to a harness, which proposes a change or gives up.
//!
//! A [`ReplayHarness`] is anything that answers a
//! [`ReplayRequest`](proto::ReplayRequest) with a
//! [`ReplayResult`](proto::ReplayResult). The one Hord ships is
//! [`CommandHarness`]: a process that reads the request as one JSON line on
//! stdin and writes the result as one JSON line on stdout, both in the
//! canonical protobuf JSON mapping (ADR 0024). The reference harness
//! (`hord-replay-ref`) is such a process.
//!
//! The lander enforces the budget per attempt (ADR 0028, ADR 0029): a
//! harness that runs past `wall_time_ms` is killed (with its process
//! group, on Unix), and a result that reports more tokens or cost than the
//! budget allows is rejected, whatever it proposed.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use hord_api::{proto, wire};
use hord_core::{Acceptance, ChangeRecord, IntentRef, ReplayBudget};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Result;
use crate::escalation::{ReplayJob, lander_actor};
use crate::lander::QueueEntry;
use crate::local::conflict_report_message;
use crate::repo::{Base, BeginOptions, Inner, Repo, blocking};
use crate::summary::ConflictSummary;
use crate::workspace::Materialization;

/// What a harness returns: its result, or why it produced none.
pub type ReplayFuture =
    Pin<Box<dyn Future<Output = std::result::Result<proto::ReplayResult, String>> + Send>>;

/// A replay harness (spec §6.6). The lander runs it once per attempt and
/// drops the future when the attempt's wall-clock budget runs out, so an
/// implementation must stop its work when dropped.
pub trait ReplayHarness: Send + Sync {
    /// Its name, for `Replaying` events and attempt records.
    fn name(&self) -> String;

    /// Replay `request`. The workspace it names is a directory workspace of
    /// `repo`, on the request's base; the harness proposes from it (through
    /// `hord propose`, or [`Repo::open_workspace`] in process) and answers
    /// with the proposed change.
    fn replay(&self, request: proto::ReplayRequest, repo: Repo) -> ReplayFuture;
}

/// A harness process speaking the JSON-lines protocol on stdin and stdout.
///
/// Its stderr is inherited. It runs in the repository root, in a process
/// group of its own on Unix, and the group is killed when the attempt is
/// dropped (its budget ran out) before the process exits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandHarness {
    argv: Vec<String>,
}

impl CommandHarness {
    /// A harness that runs `argv[0]` with the rest as arguments. `None`
    /// when `argv` is empty.
    #[must_use]
    pub fn new(argv: Vec<String>) -> Option<Self> {
        (!argv.is_empty()).then_some(Self { argv })
    }

    /// The command line.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Run the process on `request`: one request line in, one result line
    /// out. Dropping the future kills the process (and its group).
    pub async fn run(
        &self,
        request: &proto::ReplayRequest,
    ) -> std::result::Result<proto::ReplayResult, String> {
        let line = serde_json::to_string(request)
            .map_err(|err| format!("encode the replay request: {err}"))?;
        let mut command = tokio::process::Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if !request.repo.is_empty() {
            command.current_dir(&request.repo);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|err| format!("start harness {:?}: {err}", self.argv[0]))?;
        let _group = ProcessGroup(child.id());
        let mut stdin = child
            .stdin
            .take()
            .ok_or("the harness's stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("the harness's stdout was not piped")?;
        // A harness may exit without reading its request; that is not a
        // protocol error by itself.
        let _ = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await
        }
        .await;
        drop(stdin);
        let mut lines = BufReader::new(stdout).lines();
        let mut answer = None;
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|err| format!("read the harness's stdout: {err}"))?
        {
            if !line.trim().is_empty() {
                answer = Some(line);
                break;
            }
        }
        let status = child
            .wait()
            .await
            .map_err(|err| format!("wait for the harness: {err}"))?;
        match answer {
            Some(line) => serde_json::from_str::<proto::ReplayResult>(&line).map_err(|err| {
                format!("the harness's answer is not a ReplayResult: {err}: {line}")
            }),
            None => Err(format!(
                "the harness exited ({status}) without writing a ReplayResult"
            )),
        }
    }
}

impl ReplayHarness for CommandHarness {
    fn name(&self) -> String {
        self.argv.join(" ")
    }

    fn replay(&self, request: proto::ReplayRequest, _repo: Repo) -> ReplayFuture {
        let harness = self.clone();
        Box::pin(async move { harness.run(&request).await })
    }
}

/// Kills a harness and everything it started when dropped: its budget ran
/// out, or it exited and left children behind. Its process group on Unix,
/// its process tree on Windows (`kill_on_drop` alone kills only the
/// harness, and a surviving child could outlive the budget).
struct ProcessGroup(Option<u32>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // The group id is the harness's pid (`process_group(0)`). A
            // group that is gone already is not an error.
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(windows)]
        if let Some(pid) = self.0 {
            // A tree that is gone already is not an error.
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// How an attempt ended, before the lander judged it.
#[derive(Debug)]
pub(crate) enum Ended {
    /// The harness answered.
    Returned {
        result: proto::ReplayResult,
        elapsed_ms: u64,
    },
    /// It ran past its budget and was killed.
    Killed { elapsed_ms: u64 },
    /// It failed, or the attempt could not run.
    Failed { reason: String, elapsed_ms: u64 },
}

impl Ended {
    pub fn elapsed_ms(&self) -> u64 {
        match self {
            Self::Returned { elapsed_ms, .. }
            | Self::Killed { elapsed_ms }
            | Self::Failed { elapsed_ms, .. } => *elapsed_ms,
        }
    }
}

/// Why `result` is over `budget`, if it is: reported tokens or cost above
/// their limits.
#[must_use]
pub fn over_budget(result: &proto::ReplayResult, budget: &ReplayBudget) -> Option<String> {
    if let (Some(used), Some(limit)) = (result.tokens, budget.tokens)
        && used > limit
    {
        return Some(format!("reported {used} tokens; the budget is {limit}"));
    }
    if let (Some(used), Some(limit)) = (result.cost_micros, budget.cost_micros)
        && used > limit
    {
        return Some(format!(
            "reported a cost of {used} micro-dollars; the budget is {limit}"
        ));
    }
    None
}

/// Wire form of a [`ReplayBudget`].
#[must_use]
pub fn budget_message(budget: &ReplayBudget) -> proto::Budget {
    proto::Budget {
        wall_time_ms: budget.wall_time_ms,
        tokens: budget.tokens,
        cost_micros: budget.cost_micros,
    }
}

/// Wire form of a record's intent.
#[must_use]
pub fn intent_message(record: &ChangeRecord) -> proto::ChangeIntent {
    let item = |kind: &str, value: String| proto::IntentItem {
        kind: kind.into(),
        value,
    };
    proto::ChangeIntent {
        summary: record.intent.summary.clone(),
        body: record.intent.body.clone(),
        refs: record
            .intent
            .refs
            .iter()
            .map(|r| match r {
                IntentRef::Issue { id } => item("issue", id.clone()),
                IntentRef::Url { url } => item("url", url.clone()),
                IntentRef::Change { change } => item("change", wire::id(*change)),
                IntentRef::GitCommit { sha } => item("git", sha.clone()),
            })
            .collect(),
        acceptance: record
            .intent
            .acceptance
            .iter()
            .map(|a| match a {
                Acceptance::Test { name } => item("test", name.clone()),
                Acceptance::Check { command } => item("check", command.clone()),
                Acceptance::Invariant { description } => item("invariant", description.clone()),
            })
            .collect(),
    }
}

/// Wire form of a record's provenance.
#[must_use]
pub fn provenance_message(record: &ChangeRecord) -> proto::ChangeProvenance {
    let p = &record.provenance;
    proto::ChangeProvenance {
        actor: Some(wire::actor(&p.actor)),
        toolchain: wire::id(p.toolchain),
        created_at_ms: p.created_at.as_millis(),
        session: p.session.clone(),
        parent_intent: p.parent_intent.map(wire::id),
    }
}

/// Wire form of a [`ConflictSummary`].
#[must_use]
pub fn summary_message(summary: &ConflictSummary) -> proto::ConflictSummary {
    proto::ConflictSummary {
        change: wire::id(summary.change),
        nodes: summary
            .nodes
            .iter()
            .map(|n| proto::NodeRef {
                name: n.name.clone(),
                path: n.path.clone(),
                ..wire::node_ref(n.node)
            })
            .collect(),
        paths: summary.paths.iter().map(ToString::to_string).collect(),
        sides: summary
            .sides
            .iter()
            .map(|s| proto::ConflictSide {
                change: wire::id(s.change),
                intent: s.intent.clone(),
                actor: Some(wire::actor(&s.actor)),
                landed: s.landed,
                changed: s.changed.clone(),
            })
            .collect(),
        reasons: summary.reasons.clone(),
        text: summary.text.clone(),
    }
}

/// What an attempt needs from the store: the change, its entry, and head's
/// budget.
struct Inputs {
    record: ChangeRecord,
    entry: QueueEntry,
    budget: ReplayBudget,
    /// Acceptance tests the replay must not change (ADR 0034).
    protected: Vec<String>,
}

impl Inner {
    fn replay_inputs(&self, job: &ReplayJob) -> Result<Inputs> {
        Ok(Inputs {
            record: self.change_record(job.change)?,
            entry: self.submitted_entry(job.change)?,
            budget: self.replay_limits()?.1,
            protected: self.protected_test_names(job.change)?,
        })
    }
}

/// Start `job` (and the attempts after it) on the runtime, unless the
/// change is already replaying here. The lander waits for running replays
/// before it goes idle, and is woken when one finishes.
pub(crate) fn spawn_replay(repo: &Repo, job: ReplayJob) {
    let Some(harness) = repo.inner.harness.clone() else {
        return;
    };
    let change = job.change;
    if !repo.inner.replays.start(job) {
        // Its task runs it after the current attempt.
        return;
    }
    let repo = repo.clone();
    let tasks = repo.inner.tasks.clone();
    tasks.spawn(async move {
        while let Some(job) = repo.inner.replays.next(change) {
            // Closing the repository stops the attempt: dropping it kills
            // the harness, and the entry stays replaying for the next start
            // to resume (`resume_ladders`).
            let attempt = tokio::select! {
                () = repo.inner.closing.cancelled() => {
                    while repo.inner.replays.next(change).is_some() {}
                    break;
                }
                attempt = run_attempt(&repo, harness.as_ref(), &job) => attempt,
            };
            match attempt {
                Ok(more) => {
                    for job in more {
                        repo.inner.replays.start(job);
                    }
                }
                // A store failure: the entry stays replaying, and the next
                // lander start resumes it (`resume_ladders`).
                Err(_) => {
                    while repo.inner.replays.next(change).is_some() {}
                    break;
                }
            }
        }
        repo.inner.wake.notify_one();
    });
}

/// Run one attempt: a workspace on head, the harness under the budget, and
/// the outcome recorded. Returns the next attempt, if any.
async fn run_attempt(
    repo: &Repo,
    harness: &dyn ReplayHarness,
    job: &ReplayJob,
) -> Result<Vec<ReplayJob>> {
    let lookup = job.clone();
    let inputs = blocking(&repo.inner, move |inner| inner.replay_inputs(&lookup)).await?;
    let budget = inputs.budget.clone();
    let ws = repo
        .begin_directory(BeginOptions {
            base: Base::Head,
            actor: lander_actor(),
            session: None,
        })
        .await?;
    let base = ws.base();
    let request = request_message(repo, job, &inputs, &ws);
    let started = Instant::now();
    let answer = tokio::time::timeout(
        Duration::from_millis(budget.wall_time_ms),
        harness.replay(request, repo.clone()),
    )
    .await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ended = match answer {
        Err(_) => Ended::Killed { elapsed_ms },
        Ok(Err(reason)) => Ended::Failed { reason, elapsed_ms },
        Ok(Ok(result)) => Ended::Returned { result, elapsed_ms },
    };
    // The proposal (if any) is stored; the checkout is not needed.
    let _ = repo.remove_workspace(ws.id()).await;
    let job = job.clone();
    blocking(&repo.inner, move |inner| {
        inner.finish_attempt(&job, base, &budget, ended)
    })
    .await
}

fn request_message(
    repo: &Repo,
    job: &ReplayJob,
    inputs: &Inputs,
    ws: &crate::Workspace,
) -> proto::ReplayRequest {
    let workspace_path = match ws.materialization() {
        Materialization::Directory { path } => path.display().to_string(),
        Materialization::InMemory => String::new(),
    };
    proto::ReplayRequest {
        change: wire::id(job.change),
        attempt: job.attempt,
        intent: Some(intent_message(&inputs.record)),
        provenance: Some(provenance_message(&inputs.record)),
        base: wire::id(ws.base()),
        workspace: ws.id().to_string(),
        workspace_path,
        repo: repo.store().repo_root().display().to_string(),
        conflict: inputs.entry.report.as_ref().map(conflict_report_message),
        summary: inputs
            .entry
            .escalation
            .as_ref()
            .and_then(|e| e.summary.as_ref())
            .map(summary_message),
        budget: Some(budget_message(&inputs.budget)),
        note: job.note.clone().filter(|n| !n.is_empty()),
        protected_tests: inputs.protected.clone(),
    }
}
