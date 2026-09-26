//! Running one case end to end, as agents and an arbiter would: the case's
//! repository runs under its own `hord serve`, whose lander runs the replay
//! harness from `.hord/replay.toml`. Tasks are proposed through its
//! `Workspaces` service on the repository's local endpoint, and a parked
//! case is resolved from the web workbench (spec §12 M5): the runner posts
//! the workbench's form, and the UI signs the decision with the server's
//! key. The direct `Arbitrate` API is only a fallback, reported as a
//! failure.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hord_api::proto::event::Kind;
use hord_api::{RepoBackend, WorkspacesBackend, proto, wire};
use hord_core::sign::SigningKey;
use hord_core::{Actor, Bytes, ChangeId, ChangeRecord, Signature};
use hord_remote::RemoteRepo;
use hord_txn::Arbitration;
use serde::Serialize;
use tokio::process::Command;
use tokio_stream::StreamExt;

use crate::corpus::Case;

/// Slack on the wall-clock check: an attempt that is not killed must end
/// within its budget, give or take timer and scheduling noise.
const WALL_GRACE_MS: u64 = 500;
/// How often the queue is polled.
const POLL: Duration = Duration::from_millis(200);

/// Which harness the lander runs.
#[derive(Clone, Debug)]
pub enum Harness {
    /// `hord-eval-m5 scripted`: the case's script (CI).
    Scripted,
    /// A model command for `hord-replay-ref --cmd`, such as `claude -p`.
    Command(String),
    /// No harness: every conflict waits for an arbiter.
    None,
}

/// How to run the corpus.
#[derive(Clone, Debug)]
pub struct Config {
    /// The `hord` binary.
    pub hord: PathBuf,
    /// The `hord-replay-ref` binary.
    pub replay_ref: PathBuf,
    /// This binary (for `scripted`).
    pub eval: PathBuf,
    /// The harness.
    pub harness: Harness,
    /// A model name for `hord-replay-ref --model`.
    pub model: Option<String>,
    /// `[replay] budget` wall-clock time per attempt.
    pub wall_time_secs: u64,
    /// `[replay] budget` tokens per attempt.
    pub tokens: u64,
    /// `[replay] budget` cost per attempt in US dollars; `None`: no limit.
    pub cost_usd: Option<f64>,
    /// `[land] max_replay_attempts`.
    pub max_attempts: u64,
    /// Where case repositories are built.
    pub work: PathBuf,
    /// Keep each case's repository.
    pub keep: bool,
    /// `HORD_HOME` for each case's `hord serve`: its `keys/` holds the key
    /// the web UI signs arbitration decisions with.
    pub home: PathBuf,
    /// The actor that key belongs to (`HORD_ACTOR`), the UI's arbiter.
    pub arbiter: String,
    /// That key's public half, to verify `Arbitrated` signatures.
    pub arbiter_key: hord_core::sign::PublicKey,
}

/// Write a fresh signing key for `actor` under `home/keys/` (the file
/// `hord serve` gives the web UI) and return its public half.
pub fn ui_key(home: &Path, actor: &str) -> Result<hord_core::sign::PublicKey> {
    let key = SigningKey::generate()?;
    let dir = home.join("keys");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{actor}.pem")), key.to_pem()?)?;
    Ok(key.public())
}

/// How a case ended.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// A replay landed and both acceptance tests pass on the landed head.
    ResolvedByReplay,
    /// A replay landed, but an acceptance test fails on the landed head.
    ReplayFailedAcceptance {
        /// The test output.
        why: String,
    },
    /// A replay landed on a contradiction case, whose intents cannot both
    /// hold: it cheated in a way the lander could not see (ADR 0034
    /// amendment). A failure, never resolved.
    Gamed {
        /// What landed, and how the acceptance tests fared on it.
        why: String,
    },
    /// Parked for arbitration (or conflicted with no harness).
    Parked,
    /// The case does not do what the corpus needs (the first task did not
    /// land, or the second did not collide).
    Invalid {
        /// What happened instead.
        why: String,
    },
    /// The runner failed.
    Error {
        /// The error.
        why: String,
    },
}

/// One replay attempt, as the lander recorded it.
#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    /// Attempt number.
    pub attempt: u32,
    /// `proposed`, `gave_up`, `killed`, `over_budget`, `tampered`,
    /// `failed`, or `running`.
    pub outcome: String,
    /// Wall-clock time.
    pub elapsed_ms: u64,
    /// Reported tokens.
    pub tokens: Option<u64>,
    /// Reported cost in US dollars (ADR 0028).
    pub cost_usd: Option<f64>,
    /// The model that ran it, as the harness reported it (ADR 0029).
    pub model: Option<String>,
    /// For a tampered attempt, the protected tests it changed (ADR 0034).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tampered: Vec<String>,
    /// The lander's note.
    pub detail: Option<String>,
}

/// The arbitration round-trip of a parked case.
#[derive(Clone, Debug, Serialize)]
pub struct RoundTrip {
    /// It landed with both parents and a verified signed `Arbitrated`.
    pub ok: bool,
    /// What went wrong, or what landed.
    pub detail: String,
}

/// Everything about one case's run.
#[derive(Clone, Debug, Serialize)]
pub struct CaseResult {
    /// Case id.
    pub id: String,
    /// Template.
    pub kind: String,
    /// `hard` or `semantic`, as designed.
    pub designed_conflict: String,
    /// What the lander saw: `hard` (a hard merge conflict) or `semantic`.
    pub observed_conflict: Option<String>,
    /// Intents contradict.
    pub ambiguous: bool,
    /// The two intents.
    pub intents: [String; 2],
    /// How it ended.
    #[serde(flatten)]
    pub outcome: Outcome,
    /// Replay attempts.
    pub attempts: Vec<Attempt>,
    /// Attempts that ran past their budget without being killed, or whose
    /// over-budget result was accepted: hard failures.
    pub budget_violations: Vec<String>,
    /// For a parked case: its conflict summary.
    pub summary: Option<String>,
    /// For a parked case: the arbitration round-trip.
    pub arbitration: Option<RoundTrip>,
    /// With the scripted harness: whether the script resolves it.
    pub expected_resolved: Option<bool>,
    /// Wall-clock seconds for the case.
    pub seconds: f64,
}

async fn run_tool(program: &Path, args: &[&str], cwd: &Path) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "m5-eval")
        .env("GIT_AUTHOR_EMAIL", "m5-eval@example.com")
        .env("GIT_COMMITTER_NAME", "m5-eval")
        .env("GIT_COMMITTER_EMAIL", "m5-eval@example.com")
        .env("HORD_ACTOR", "m5-eval")
        .env("HORD_NO_DAEMON", "1")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {}", program.display()))?;
    if !out.status.success() {
        bail!(
            "{} {args:?}: {}{}",
            program.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn write_files<'a>(
    dir: &Path,
    files: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Result<()> {
    for (path, text) in files {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, text).with_context(|| format!("write {}", target.display()))?;
    }
    Ok(())
}

/// Remove `dir`, making read-only pristine checkouts writable first.
fn remove_tree(dir: &Path) {
    fn writable(dir: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(dir) {
                let mode = meta.permissions().mode() | 0o700;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode));
            }
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    writable(&entry.path());
                }
            }
        }
    }
    if dir.exists() {
        writable(dir);
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// A case's repository under its own `hord serve`, which serves the gRPC
/// API on the repository's local endpoint and the web UI on a loopback
/// port; the server stops when dropped.
struct CaseRepo {
    dir: PathBuf,
    remote: RemoteRepo,
    /// The web UI's address, `http://127.0.0.1:<port>`.
    ui: String,
    server: tokio::process::Child,
}

impl CaseRepo {
    async fn stop(mut self) {
        // `hord serve` stops on Ctrl-C; SIGINT lets it close its store.
        #[cfg(unix)]
        if let Some(pid) = self.server.id() {
            let _ = Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .status()
                .await;
        }
        if tokio::time::timeout(Duration::from_secs(10), self.server.wait())
            .await
            .is_err()
        {
            let _ = self.server.kill().await;
        }
    }
}

fn caller(id: &str) -> proto::Caller {
    proto::Caller {
        actor: Some(wire::actor(&Actor::Agent {
            id: id.into(),
            model: "m5-eval".into(),
            model_hash: Bytes::default(),
            harness: "hord-eval-m5".into(),
        })),
        session: None,
    }
}

/// Build the case's repository, import it, configure the harness, and
/// start its `hord serve`.
async fn build(cfg: &Config, path: &Path, case: &Case) -> Result<CaseRepo> {
    let dir = cfg.work.join(&case.id);
    remove_tree(&dir);
    std::fs::create_dir_all(&dir)?;
    write_files(&dir, &case.base)?;
    let cost = cfg
        .cost_usd
        .map(|usd| format!(", cost_usd = {usd}"))
        .unwrap_or_default();
    let policy = format!(
        "[land]\nrequire = [\"check\", \"test:full\"]\nmax_replay_attempts = {}\n\n[replay]\nbudget = {{ wall_time_secs = {}, tokens = {}{cost} }}\n",
        cfg.max_attempts, cfg.wall_time_secs, cfg.tokens
    );
    std::fs::write(dir.join(".hord-policy.toml"), policy)?;
    let git = Path::new("git");
    run_tool(git, &["init", "-q", "-b", "main"], &dir).await?;
    run_tool(git, &["add", "."], &dir).await?;
    run_tool(git, &["commit", "-q", "-m", "base"], &dir).await?;
    let dir_text = dir.to_str().ok_or_else(|| anyhow!("non-UTF-8 work path"))?;
    run_tool(&cfg.hord, &["init", "--from-git", dir_text], &dir).await?;

    let command = match &cfg.harness {
        Harness::None => None,
        Harness::Scripted => Some(format!(
            "{} scripted --case {}",
            shell_quote(&cfg.eval),
            shell_quote(&path.canonicalize()?)
        )),
        Harness::Command(cmd) => Some(cmd.clone()),
    };
    if let Some(command) = command {
        let mut harness = vec![
            cfg.replay_ref.display().to_string(),
            "--cmd".into(),
            command,
            "--hord".into(),
            cfg.hord.display().to_string(),
        ];
        let model = match &cfg.harness {
            Harness::Scripted => Some("scripted".to_owned()),
            _ => cfg.model.clone(),
        };
        if let Some(model) = model {
            harness.extend(["--model".into(), model]);
        }
        #[derive(Serialize)]
        struct ReplayToml {
            harness: Vec<String>,
        }
        std::fs::write(
            dir.join(".hord").join("replay.toml"),
            toml::to_string(&ReplayToml { harness })?,
        )?;
    }

    let log_path = cfg.work.join(format!("{}.serve.log", case.id));
    let log = std::fs::File::create(&log_path)?;
    let server = Command::new(&cfg.hord)
        .args(["serve", "--repo", dir_text, "--bind", "127.0.0.1:0"])
        .current_dir(&dir)
        .env("HORD_HOME", &cfg.home)
        .env("HORD_ACTOR", &cfg.arbiter)
        .env_remove("HORD_AGENT_MODEL")
        .env_remove("HORD_NO_DAEMON")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .context("start the case's hord serve")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let (remote, ui) = loop {
        // Its first line names the address: `hord serve: http://<addr> (...)`.
        let ui = std::fs::read_to_string(&log_path).ok().and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("hord serve: "))
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_owned)
        });
        if let Some(ui) = ui
            && let Ok(remote) = RemoteRepo::connect_local(&dir).await
            && remote.head(proto::HeadRequest {}).await.is_ok()
        {
            break (remote, ui);
        }
        if Instant::now() > deadline {
            bail!(
                "hord serve for {} did not start (see {})",
                case.id,
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    Ok(CaseRepo {
        dir,
        remote,
        ui,
        server,
    })
}

/// The latest queue entry submitted as `change`.
async fn entry(remote: &RemoteRepo, change: &str) -> Result<Option<proto::QueueEntry>> {
    Ok(remote
        .queue(proto::QueueQuery {
            change: Some(change.into()),
            ..Default::default()
        })
        .await?
        .entries
        .into_iter()
        .rev()
        .find(|e| e.change == change))
}

/// Poll `change`'s entry until `done` holds, for up to `limit`.
async fn wait_for(
    remote: &RemoteRepo,
    change: &str,
    limit: Duration,
    done: impl Fn(&proto::QueueEntry) -> bool,
) -> Result<proto::QueueEntry> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(entry) = entry(remote, change).await?
            && done(&entry)
        {
            return Ok(entry);
        }
        if Instant::now() > deadline {
            let last = entry(remote, change).await?;
            bail!("{change} did not settle in {limit:?}: {last:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Propose `task` from a new workspace (all on the same base).
async fn workspace(remote: &RemoteRepo, who: &str) -> Result<proto::WsNewResponse> {
    Ok(remote
        .workspaces()
        .ws_new(proto::WsNewRequest {
            caller: Some(caller(who)),
            base: None,
            materialize: proto::Materialize::Clone.into(),
        })
        .await?)
}

async fn propose(
    remote: &RemoteRepo,
    ws: &proto::WsNewResponse,
    task: &crate::corpus::Task,
    who: &str,
) -> Result<String> {
    write_files(Path::new(&ws.materialization), &task.writes())?;
    Ok(remote
        .workspaces()
        .propose(proto::ProposeRequest {
            caller: Some(caller(who)),
            workspace: Some(ws.id.clone()),
            intent: task.intent_text()?,
            intent_path: format!("{}.md", task.name),
        })
        .await?
        .change)
}

fn attempt_outcome(outcome: proto::ReplayOutcome) -> &'static str {
    match outcome {
        proto::ReplayOutcome::Running => "running",
        proto::ReplayOutcome::Proposed => "proposed",
        proto::ReplayOutcome::GaveUp => "gave_up",
        proto::ReplayOutcome::Killed => "killed",
        proto::ReplayOutcome::OverBudget => "over_budget",
        proto::ReplayOutcome::Failed => "failed",
        proto::ReplayOutcome::Tampered => "tampered",
        proto::ReplayOutcome::Unspecified => "unknown",
    }
}

/// Budget enforcement (ADR 0028): an attempt that was not killed ended
/// within its wall-clock budget, and no over-budget result was accepted.
fn budget_violations(cfg: &Config, attempts: &[proto::ReplayAttempt]) -> Vec<String> {
    let wall = cfg.wall_time_secs * 1_000;
    let mut out = Vec::new();
    for a in attempts {
        let killed = a.outcome() == proto::ReplayOutcome::Killed;
        if !killed && a.elapsed_ms > wall + WALL_GRACE_MS {
            out.push(format!(
                "attempt {} ran {} ms, past its {wall} ms budget, and was not killed",
                a.attempt, a.elapsed_ms
            ));
        }
        if a.outcome() == proto::ReplayOutcome::Proposed && a.tokens.is_some_and(|t| t > cfg.tokens)
        {
            out.push(format!(
                "attempt {} reported {:?} tokens over the {} budget and was accepted",
                a.attempt, a.tokens, cfg.tokens
            ));
        }
        let over_cost = cfg.cost_usd.is_some_and(|usd| {
            a.cost_micros
                .is_some_and(|c| c as f64 > (usd * 1_000_000.0).round())
        });
        if a.outcome() == proto::ReplayOutcome::Proposed && over_cost {
            out.push(format!(
                "attempt {} reported a cost of {:?} micro-dollars over the ${:?} budget and was \
                 accepted",
                a.attempt, a.cost_micros, cfg.cost_usd
            ));
        }
    }
    out
}

/// Both tasks' acceptance tests, as the case defines them, on the landed
/// head (ADR 0029 grading). `Ok(None)` when they pass.
async fn acceptance(remote: &RemoteRepo, case: &Case) -> Result<Option<String>> {
    let ws = workspace(remote, "m5-grader").await?;
    let checkout = PathBuf::from(&ws.materialization);
    let mut args = vec!["test".to_owned(), "--quiet".to_owned()];
    for task in &case.tasks {
        write_files(&checkout, [(&task.test_path, &task.test_source)])?;
        let target = Path::new(&task.test_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("test path {}", task.test_path))?;
        args.extend(["--test".to_owned(), target.to_owned()]);
    }
    let out = Command::new("cargo")
        .args(&args)
        .current_dir(&checkout)
        .stdin(Stdio::null())
        .output()
        .await
        .context("run cargo test")?;
    let _ = remote
        .workspaces()
        .ws_rm(proto::WsRmRequest { id: ws.id })
        .await;
    if out.status.success() {
        return Ok(None);
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let tail: Vec<&str> = text.lines().rev().take(15).collect();
    Ok(Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n")))
}

/// Resolve the parked `change` as the workbench does: a signed `Arbitrate`
/// (keep ours, which always lands), then check that the resolution landed
/// with both colliding changes as parents and that the `Arbitrated` event
/// carries a signature that verifies.
async fn round_trip(
    cfg: &Config,
    remote: &RemoteRepo,
    ui: &str,
    change: &str,
    first_landed: &str,
) -> Result<String> {
    let id: ChangeId = wire::object_id("change", change)?;
    let decision = Arbitration::PickOurs;
    // The workbench's own form, as its "pick ours" button posts it; the UI
    // signs the decision with the server's key for `cfg.arbiter`.
    let (arbiter, key, fallback) = match workbench_pick_ours(ui, change).await {
        Ok(()) => (
            Actor::Human {
                id: cfg.arbiter.clone(),
            },
            cfg.arbiter_key,
            None,
        ),
        Err(err) => {
            // Resolve it anyway, so the case's result is complete, but
            // report the missing workbench as a failure.
            let (arbiter, key) = direct_pick_ours(remote, id, change).await?;
            (arbiter, key, Some(err))
        }
    };
    // Until it is arbitrated, or back in the arbitration queue with no
    // resolution pending (the resolution did not land).
    let entry = wait_for(remote, change, Duration::from_secs(300), |e| {
        e.status() == proto::QueueStatus::Arbitrated
            || e.escalation
                .as_ref()
                .is_some_and(|x| x.resolution.is_none())
    })
    .await?;
    if entry.status() != proto::QueueStatus::Arbitrated {
        bail!(
            "the resolution did not land: {:?}",
            entry.escalation.and_then(|e| e.note)
        );
    }
    let landed = entry.landed.ok_or_else(|| anyhow!("no landed id"))?;
    let objects = remote
        .get_objects(proto::GetObjectsRequest {
            ids: vec![landed.clone()],
        })
        .await?;
    let record: ChangeRecord = hord_encoding::decode(
        &objects
            .objects
            .first()
            .ok_or_else(|| anyhow!("resolution {landed} is not stored"))?
            .cbor,
    )?;
    let parents: Vec<String> = record.parents.iter().copied().map(wire::id).collect();
    if !parents.iter().any(|p| p == change) || !parents.iter().any(|p| p == first_landed) {
        bail!("resolution {landed} has parents {parents:?}, not both colliding changes");
    }
    let mut events = remote
        .events(proto::EventsRequest { from: Some(0) })
        .await?;
    let arbitrated = loop {
        let next = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .map_err(|_| anyhow!("no Arbitrated event for {change}"))?
            .ok_or_else(|| anyhow!("the event stream ended"))??;
        if let Some(Kind::Arbitrated(a)) = next.event.and_then(|e| e.kind)
            && a.change == change
        {
            break a;
        }
    };
    if arbitrated.result != landed {
        bail!("Arbitrated names {}, not {landed}", arbitrated.result);
    }
    if arbitrated.by != Some(wire::actor(&arbiter)) {
        bail!("Arbitrated names {:?} as the arbiter", arbitrated.by);
    }
    let stored = Signature {
        key_id: arbitrated
            .key_id
            .ok_or_else(|| anyhow!("Arbitrated is not signed"))?,
        bytes: Bytes::new(
            arbitrated
                .signature
                .ok_or_else(|| anyhow!("Arbitrated has no signature"))?,
        ),
    };
    hord_txn::verify_arbitration(id, &decision, &stored, &key)
        .context("the Arbitrated signature does not verify")?;
    if let Some(err) = fallback {
        bail!(
            "the workbench route is unavailable ({err:#}); resolved through the direct \
             Arbitrate API instead, which does not count"
        );
    }
    Ok(format!(
        "resolved from the workbench; landed {landed} with parents {parents:?}"
    ))
}

/// Post the workbench's "pick ours" form for `change` to the web UI at
/// `ui`, and check the page it returns says the resolution was submitted.
async fn workbench_pick_ours(ui: &str, change: &str) -> Result<()> {
    use http_body_util::{BodyExt, Full};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<bytes::Bytes>>();
    let request = http::Request::post(format!("{ui}/arbitrate/{change}"))
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Full::new(bytes::Bytes::from_static(
            b"action=pick_ours&note=&resolved=",
        )))?;
    let response = client
        .request(request)
        .await
        .context("post the workbench form")?;
    let status = response.status();
    let page = response.into_body().collect().await?.to_bytes();
    let page = String::from_utf8_lossy(&page);
    if !status.is_success() {
        bail!("the workbench answered {status}");
    }
    if !page.contains("submitted; it lands with both as parents") {
        let flash = page
            .find("Arbitration failed")
            .or_else(|| page.find("Could not sign"))
            .map(|i| page[i..].split('<').next().unwrap_or_default().to_owned())
            .unwrap_or_else(|| "no confirmation on the page".into());
        bail!("the workbench did not submit a resolution: {flash}");
    }
    Ok(())
}

/// The direct `Arbitrate` call (keep ours), signed with a fresh key: the
/// fallback when the workbench is unavailable.
async fn direct_pick_ours(
    remote: &RemoteRepo,
    id: ChangeId,
    change: &str,
) -> Result<(Actor, hord_core::sign::PublicKey)> {
    let key = SigningKey::generate()?;
    let signature = hord_txn::sign_arbitration(id, &Arbitration::PickOurs, &key)?;
    let arbiter = Actor::Human {
        id: "m5-arbiter-direct".into(),
    };
    remote
        .arbitrate(proto::ArbitrateRequest {
            change: change.into(),
            action: Some(proto::Arbitration {
                action: Some(proto::arbitration::Action::PickOurs(true)),
            }),
            arbiter: Some(wire::actor(&arbiter)),
            note: None,
            key_id: Some(signature.key_id.clone()),
            signature: Some(signature.bytes.as_slice().to_vec()),
        })
        .await?;
    Ok((arbiter, key.public()))
}

/// Run `case` end to end.
pub async fn run_case(cfg: &Config, path: &Path, case: &Case) -> CaseResult {
    let started = Instant::now();
    let mut result = CaseResult {
        id: case.id.clone(),
        kind: case.kind.clone(),
        designed_conflict: case.conflict.clone(),
        observed_conflict: None,
        ambiguous: case.ambiguous,
        intents: [case.first().summary.clone(), case.second().summary.clone()],
        outcome: Outcome::Error {
            why: "not run".into(),
        },
        attempts: Vec::new(),
        budget_violations: Vec::new(),
        summary: None,
        arbitration: None,
        expected_resolved: matches!(cfg.harness, Harness::Scripted).then(|| {
            case.scripted_resolves(usize::try_from(cfg.max_attempts).unwrap_or(usize::MAX))
        }),
        seconds: 0.0,
    };
    let repo = match build(cfg, path, case).await {
        Ok(repo) => repo,
        Err(err) => {
            result.outcome = Outcome::Error {
                why: format!("{err:#}"),
            };
            return result;
        }
    };
    if let Err(err) = drive(cfg, case, &repo.remote, &repo.ui, &mut result).await {
        result.outcome = Outcome::Error {
            why: format!("{err:#}"),
        };
    }
    let dir = repo.dir.clone();
    repo.stop().await;
    if !cfg.keep {
        remove_tree(&dir);
    }
    result.seconds = started.elapsed().as_secs_f64();
    result
}

async fn drive(
    cfg: &Config,
    case: &Case,
    remote: &RemoteRepo,
    ui: &str,
    result: &mut CaseResult,
) -> Result<()> {
    let ws_a = workspace(remote, "agent-a").await?;
    let ws_b = workspace(remote, "agent-b").await?;
    let a = propose(remote, &ws_a, case.first(), "agent-a").await?;
    let b = propose(remote, &ws_b, case.second(), "agent-b").await?;
    // Terminal states only. With a harness, `Conflicted` is not one: the
    // lander puts a conflicted change on the ladder (`Replaying`, or
    // `NeedsArbitration` when no replay is allowed) as it settles it.
    let with_harness = !matches!(cfg.harness, Harness::None);
    let settled = |e: &proto::QueueEntry| match e.status() {
        proto::QueueStatus::Queued
        | proto::QueueStatus::Replaying
        | proto::QueueStatus::Unspecified => false,
        proto::QueueStatus::Conflicted => !with_harness,
        proto::QueueStatus::Landed
        | proto::QueueStatus::Rejected
        | proto::QueueStatus::Parked
        | proto::QueueStatus::NeedsArbitration
        | proto::QueueStatus::Replayed
        | proto::QueueStatus::Arbitrated => true,
    };
    remote
        .submit(proto::SubmitRequest { change: a.clone() })
        .await?;
    let first = wait_for(remote, &a, Duration::from_secs(300), settled).await?;
    if first.status() != proto::QueueStatus::Landed {
        result.outcome = Outcome::Invalid {
            why: format!(
                "the first task did not land: {:?} {:?}",
                first.status(),
                first.reason
            ),
        };
        return Ok(());
    }
    let first_landed = first.landed.unwrap_or_else(|| a.clone());
    remote
        .submit(proto::SubmitRequest { change: b.clone() })
        .await?;
    let limit = Duration::from_secs(300 + 2 * cfg.max_attempts * cfg.wall_time_secs);
    let second = wait_for(remote, &b, limit, settled).await?;
    if let Some(report) = &second.report {
        result.observed_conflict = Some(
            if report
                .merge
                .iter()
                .any(|m| m.severity() == proto::MergeSeverity::Hard)
            {
                "hard".into()
            } else if report.verification.is_some() {
                "semantic".into()
            } else {
                "none".into()
            },
        );
    }
    let escalation = second.escalation.clone().unwrap_or_default();
    result.attempts = escalation
        .attempts
        .iter()
        .map(|a| Attempt {
            attempt: a.attempt,
            outcome: attempt_outcome(a.outcome()).into(),
            elapsed_ms: a.elapsed_ms,
            tokens: a.tokens,
            cost_usd: a.cost_micros.map(|c| c as f64 / 1_000_000.0),
            model: a.model.clone(),
            tampered: a
                .tampered
                .iter()
                .map(|n| n.name.clone().unwrap_or_else(|| n.id.clone()))
                .collect(),
            detail: a.detail.clone(),
        })
        .collect();
    result.budget_violations = budget_violations(cfg, &escalation.attempts);
    match second.status() {
        proto::QueueStatus::Landed => {
            result.outcome = Outcome::Invalid {
                why: "the second task landed without a conflict".into(),
            };
        }
        proto::QueueStatus::Replayed => {
            let accepted = acceptance(remote, case).await?;
            result.outcome = if case.ambiguous {
                Outcome::Gamed {
                    why: format!(
                        "a replay landed ({}) on a contradiction; acceptance on head: {}",
                        second.landed.as_deref().unwrap_or("?"),
                        accepted.as_deref().unwrap_or("both passed")
                    ),
                }
            } else {
                match accepted {
                    None => Outcome::ResolvedByReplay,
                    Some(why) => Outcome::ReplayFailedAcceptance { why },
                }
            };
        }
        proto::QueueStatus::NeedsArbitration | proto::QueueStatus::Conflicted => {
            result.outcome = Outcome::Parked;
            result.summary = escalation.summary.map(|s| s.text);
            result.arbitration = Some(match round_trip(cfg, remote, ui, &b, &first_landed).await {
                Ok(detail) => RoundTrip { ok: true, detail },
                Err(err) => RoundTrip {
                    ok: false,
                    detail: format!("{err:#}"),
                },
            });
        }
        other => {
            result.outcome = Outcome::Error {
                why: format!("the second task ended {other:?}: {:?}", second.reason),
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            hord: PathBuf::new(),
            replay_ref: PathBuf::new(),
            eval: PathBuf::new(),
            harness: Harness::Scripted,
            model: None,
            wall_time_secs: 10,
            tokens: 100,
            cost_usd: None,
            max_attempts: 2,
            work: PathBuf::new(),
            keep: false,
            home: PathBuf::new(),
            arbiter: "m5-arbiter".into(),
            arbiter_key: SigningKey::generate()
                .expect("generate a test key")
                .public(),
        }
    }

    fn attempt(
        outcome: proto::ReplayOutcome,
        elapsed_ms: u64,
        tokens: u64,
    ) -> proto::ReplayAttempt {
        proto::ReplayAttempt {
            attempt: 1,
            outcome: outcome.into(),
            elapsed_ms,
            tokens: Some(tokens),
            ..Default::default()
        }
    }

    /// A workbench that does not answer is an error, so the runner reports
    /// the case as failed even though the direct API resolves it.
    #[tokio::test]
    async fn an_unreachable_workbench_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let addr = listener.local_addr().expect("its address");
        drop(listener);
        let change = "ab".repeat(32);
        assert!(
            workbench_pick_ours(&format!("http://{addr}"), &change)
                .await
                .is_err()
        );
    }

    /// An attempt past its wall-clock budget must have been killed, and an
    /// over-budget result must not have been accepted (ADR 0028).
    #[test]
    fn budget_violations_are_found() {
        let cfg = cfg();
        let fine = [
            attempt(proto::ReplayOutcome::Proposed, 9_000, 50),
            attempt(proto::ReplayOutcome::Killed, 10_050, 0),
            attempt(proto::ReplayOutcome::OverBudget, 1_000, 500),
        ];
        assert!(budget_violations(&cfg, &fine).is_empty());
        let late = [attempt(proto::ReplayOutcome::GaveUp, 12_000, 0)];
        assert_eq!(budget_violations(&cfg, &late).len(), 1);
        let greedy = [attempt(proto::ReplayOutcome::Proposed, 1_000, 500)];
        assert_eq!(budget_violations(&cfg, &greedy).len(), 1);
    }
}
