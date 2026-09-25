//! The escalation ladder (spec §6.4 rungs 2 and 3, §6.6): replay through a
//! scripted harness, budget enforcement, arbitration candidates (ADR 0029),
//! and arbitration landing a change with both parents.

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use hord_api::proto;
use hord_api::proto::event::Kind;
use hord_core::{Bytes, ChangeId};
use hord_txn::{
    Arbiter, Arbitration, CommandHarness, QueueEntry, QueueStatus, ReplayFuture, ReplayHarness,
    ReplayOutcome, Repo, RepoOptions, StubVerifier, Verdict, Verifier, VerifyFuture, VerifyRequest,
};
use tokio_stream::StreamExt;

/// One scripted harness answer.
#[derive(Clone, Debug)]
enum Step {
    /// Replace `from` with `to` in `src/lib.rs` and propose.
    Edit {
        from: &'static str,
        to: &'static str,
        cost_micros: Option<u64>,
    },
    /// Give up.
    GiveUp,
}

/// A replay harness that plays a script, one step per attempt, in process.
#[derive(Clone, Default)]
struct Scripted {
    steps: Arc<Mutex<VecDeque<Step>>>,
    requests: Arc<Mutex<Vec<proto::ReplayRequest>>>,
}

impl Scripted {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            requests: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<proto::ReplayRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

async fn play(
    step: Step,
    request: proto::ReplayRequest,
    repo: Repo,
) -> Result<proto::ReplayResult, String> {
    use proto::replay_result::Status;
    let (from, to, cost_micros) = match step {
        Step::GiveUp => {
            return Ok(proto::ReplayResult {
                status: Some(Status::GaveUp(proto::ReplayGaveUp {
                    reason: "scripted".into(),
                })),
                ..Default::default()
            });
        }
        Step::Edit {
            from,
            to,
            cost_micros,
        } => (from, to, cost_micros),
    };
    let id = request
        .workspace
        .parse()
        .map_err(|e| format!("workspace id: {e}"))?;
    let mut ws = repo
        .open_workspace(id, actor("replayer"), None)
        .await
        .map_err(|e| e.to_string())?;
    let file = path("src/lib.rs");
    let text = ws
        .read_file(&file)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("src/lib.rs is missing")?;
    let text = String::from_utf8(text.as_slice().to_vec()).map_err(|e| e.to_string())?;
    ws.write_file(&file, text.replacen(from, to, 1))
        .await
        .map_err(|e| e.to_string())?;
    let summary = request.intent.map(|i| i.summary).unwrap_or_default();
    let proposal = ws
        .propose(intent(&format!("replay: {summary}")))
        .await
        .map_err(|e| e.to_string())?;
    Ok(proto::ReplayResult {
        status: Some(Status::Proposed(proto::ReplayProposed {
            change: proposal.change.to_hex(),
        })),
        tokens: Some(10),
        cost_micros,
        model: Some("scripted".into()),
    })
}

impl ReplayHarness for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }

    fn replay(&self, request: proto::ReplayRequest, repo: Repo) -> ReplayFuture {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        let step = self.steps.lock().ok().and_then(|mut s| s.pop_front());
        Box::pin(async move {
            match step {
                Some(step) => play(step, request, repo).await,
                None => Err("the script ran out".into()),
            }
        })
    }
}

/// Passes everything, except replays while `fail_replays` is set (a
/// semantic conflict on every replay, spec §6.5).
#[derive(Default)]
struct FailReplays {
    fail_replays: AtomicBool,
}

impl Verifier for FailReplays {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        let fail = self.fail_replays.load(Ordering::SeqCst)
            && request.change.provenance.parent_intent.is_some();
        Box::pin(async move {
            if fail {
                Verdict::Fail {
                    evidence: Vec::new(),
                    reason: "the replay's acceptance test failed".into(),
                }
            } else {
                Verdict::Pass {
                    evidence: Vec::new(),
                }
            }
        })
    }
}

fn files_with_policy(policy: &'static str) -> Vec<(&'static str, &'static str)> {
    let mut files = fixture();
    files.push((".hord-policy.toml", policy));
    files
}

async fn ladder_repo(
    policy: &'static str,
    harness: Option<Arc<dyn ReplayHarness>>,
    verifier: Arc<dyn Verifier>,
) -> TestResult<TempRepo> {
    repo_with(
        &files_with_policy(policy),
        RepoOptions {
            verifier: Some(verifier),
            harness,
            ..RepoOptions::default()
        },
    )
    .await
}

/// a makes beta 20 and lands; b makes beta 21 on the same base: a hard
/// merge conflict. Returns (a, b).
async fn collide(repo: &Repo) -> TestResult<(ChangeId, ChangeId)> {
    let mut a = begin(repo, "a").await?;
    let mut b = begin(repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    21\n").await?;
    let ca = submit(repo, &mut a, "beta returns 20").await?;
    let cb = submit(repo, &mut b, "beta returns 21").await?;
    Ok((ca, cb))
}

async fn head_lib(repo: &Repo) -> TestResult<String> {
    let mut ws = begin(repo, "reader").await?;
    let bytes = ws
        .read_file(&path("src/lib.rs"))
        .await?
        .ok_or("src/lib.rs at head")?;
    Ok(String::from_utf8(bytes.as_slice().to_vec())?)
}

/// Every recorded event so far.
async fn events(repo: &Repo) -> TestResult<Vec<Kind>> {
    let mut stream = repo.events(Some(0)).await?;
    let mut out = Vec::new();
    while let Ok(Some(next)) = tokio::time::timeout(Duration::from_millis(300), stream.next()).await
    {
        if let Some(kind) = next?.event.and_then(|e| e.kind) {
            out.push(kind);
        }
    }
    Ok(out)
}

fn escalation(entry: &QueueEntry) -> TestResult<&hord_txn::Escalation> {
    Ok(entry
        .escalation
        .as_ref()
        .ok_or_else(|| format!("no escalation on {entry:#?}"))?)
}

const TWO_ATTEMPTS: &str = "[land]\nmax_replay_attempts = 2\n";

#[tokio::test]
async fn a_replay_resolves_a_hard_conflict() -> TestResult {
    let harness = Scripted::new([Step::Edit {
        from: "    20\n",
        to: "    21\n",
        cost_micros: None,
    }]);
    let t = ladder_repo(
        TWO_ATTEMPTS,
        Some(Arc::new(harness.clone())),
        Arc::new(StubVerifier),
    )
    .await?;
    let (ca, cb) = collide(&t.repo).await?;
    t.repo.land_local().await?;

    let entry = t.repo.status(cb).await?;
    let QueueStatus::Replayed { landed } = entry.status else {
        return Err(format!("expected Replayed, got {entry:#?}").into());
    };
    let record = t.repo.change(landed).await?;
    assert_eq!(
        record.provenance.parent_intent,
        Some(cb),
        "Hord records parent_intent"
    );
    assert_eq!(record.parents, vec![ca]);
    assert_eq!(t.repo.head().await?.change, Some(landed));
    assert!(head_lib(&t.repo).await?.contains("    21\n"));

    let escalation = escalation(&entry)?;
    assert_eq!(escalation.attempts.len(), 1);
    let attempt = &escalation.attempts[0];
    assert_eq!(attempt.outcome, ReplayOutcome::Proposed);
    assert_eq!(attempt.harness, "scripted");
    assert_eq!(attempt.tokens, Some(10));
    let summary = escalation.summary.as_ref().ok_or("summary")?;
    assert_eq!(summary.sides.len(), 2, "{summary:#?}");
    assert!(!summary.sides[0].landed && summary.sides[1].landed);
    assert_eq!(summary.sides[1].change, ca);
    assert!(
        summary
            .nodes
            .iter()
            .any(|n| n.name.as_deref() == Some("beta")
                || n.name.as_deref().is_some_and(|s| s.ends_with("::beta"))),
        "{summary:#?}"
    );
    assert!(summary.text.contains("beta returns 20"), "{}", summary.text);

    // The harness got the intent, a workspace on the new base, the report,
    // and the budget.
    let requests = harness.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.change, cb.to_hex());
    assert_eq!(request.attempt, 1);
    assert_eq!(
        request.intent.as_ref().map(|i| i.summary.as_str()),
        Some("beta returns 21")
    );
    let head_after_a = t.repo.change(ca).await?.result;
    assert_eq!(request.base, head_after_a.to_hex());
    assert!(
        request
            .conflict
            .as_ref()
            .is_some_and(|c| !c.merge.is_empty())
    );
    assert_eq!(
        request.budget.as_ref().map(|b| b.wall_time_ms),
        Some(600_000)
    );
    assert!(!request.workspace_path.is_empty());

    let events = events(&t.repo).await?;
    assert!(
        events.iter().any(|k| matches!(k, Kind::Replaying(r)
            if r.change == cb.to_hex() && r.attempt == 1 && r.harness == "scripted")),
        "{events:#?}"
    );
    Ok(())
}

/// ADR 0028: a harness that runs past its wall-clock budget is killed,
/// with its process group, and the change goes on up the ladder.
#[cfg(unix)]
#[tokio::test]
async fn a_replay_past_its_budget_is_killed() -> TestResult {
    let marker = temp_dir("marker")?.join("finished");
    let harness = CommandHarness::new(vec![
        "sh".into(),
        "-c".into(),
        "sleep 4 && touch \"$0\"".into(),
        marker.display().to_string(),
    ])
    .ok_or("a command")?;
    let t = ladder_repo(
        "[land]\nmax_replay_attempts = 1\n\n[replay]\nbudget = { wall_time_secs = 1 }\n",
        Some(Arc::new(harness)),
        Arc::new(StubVerifier),
    )
    .await?;
    let (_, cb) = collide(&t.repo).await?;
    let started = std::time::Instant::now();
    t.repo.land_local().await?;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the harness was not stopped at its budget: {:?}",
        started.elapsed()
    );
    let entry = t.repo.status(cb).await?;
    assert_eq!(entry.status, QueueStatus::NeedsArbitration, "{entry:#?}");
    let escalation = escalation(&entry)?;
    assert_eq!(escalation.attempts.len(), 1);
    let attempt = &escalation.attempts[0];
    assert_eq!(attempt.outcome, ReplayOutcome::Killed, "{attempt:#?}");
    assert!(attempt.elapsed_ms >= 1_000 && attempt.elapsed_ms < 4_000);
    assert!(escalation.candidates.is_empty());
    // The whole process group died: nothing finishes after the kill.
    tokio::time::sleep(Duration::from_millis(4_500)).await;
    assert!(!marker.exists(), "the harness's child outlived the kill");
    Ok(())
}

/// ADR 0029: attempts run out into arbitration; every proposed replay is
/// a candidate, equal ops collapse into one, and a result over its cost
/// budget is rejected (not submitted, not a candidate). The arbiter then
/// lands a candidate directly.
#[tokio::test]
async fn attempts_run_out_into_arbitration_with_deduplicated_candidates() -> TestResult {
    let edit = |to: &'static str, cost_micros: Option<u64>| Step::Edit {
        from: "    20\n",
        to,
        cost_micros,
    };
    let harness = Scripted::new([
        edit("    21\n", None),
        edit("    21\n", Some(500_000)),
        edit("    22\n", None),
        edit("    23\n", Some(2_000_000)),
    ]);
    let verifier = Arc::new(FailReplays {
        fail_replays: AtomicBool::new(true),
    });
    let t = ladder_repo(
        "[land]\nmax_replay_attempts = 4\n\n[replay]\nbudget = { cost_usd = 1.0 }\n",
        Some(Arc::new(harness.clone())),
        verifier.clone(),
    )
    .await?;
    let (ca, cb) = collide(&t.repo).await?;
    t.repo.land_local().await?;

    let entry = t.repo.status(cb).await?;
    assert_eq!(entry.status, QueueStatus::NeedsArbitration, "{entry:#?}");
    let escalation = escalation(&entry)?;
    let outcomes: Vec<ReplayOutcome> = escalation.attempts.iter().map(|a| a.outcome).collect();
    assert_eq!(
        outcomes,
        vec![
            ReplayOutcome::Proposed,
            ReplayOutcome::Proposed,
            ReplayOutcome::Proposed,
            ReplayOutcome::OverBudget,
        ]
    );
    assert!(escalation.attempts[3].change.is_none(), "not submitted");
    assert!(
        escalation.attempts[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.starts_with("conflicted")),
        "{:#?}",
        escalation.attempts[0]
    );
    let candidates: Vec<Vec<u32>> = escalation
        .candidates
        .iter()
        .map(|c| c.attempts.clone())
        .collect();
    assert_eq!(candidates, vec![vec![1, 2], vec![3]]);
    assert_eq!(harness.requests().len(), 4);
    assert_eq!(t.repo.head().await?.change, Some(ca), "nothing else landed");
    let events = events(&t.repo).await?;
    assert!(
        events.iter().any(|k| matches!(k, Kind::Parked(p)
            if p.change == cb.to_hex()
                && p.reason() == proto::ParkReason::NeedsArbitration)),
        "{events:#?}"
    );

    // Pick the second candidate (beta 22).
    verifier.fail_replays.store(false, Ordering::SeqCst);
    let pick = escalation.candidates[1].change;
    let (resolution, _) = t
        .repo
        .arbitrate(
            cb,
            Arbitration::Resolved(pick),
            Arbiter {
                actor: actor("arbiter"),
                key_id: None,
                signature: None,
            },
        )
        .await?;
    t.repo.land_local().await?;
    let entry = t.repo.status(cb).await?;
    let QueueStatus::Arbitrated { landed } = entry.status else {
        return Err(format!("expected Arbitrated, got {entry:#?}").into());
    };
    let record = t.repo.change(landed).await?;
    assert_eq!(record.rebased_from.unwrap_or(landed), resolution);
    assert!(record.parents.contains(&cb) && record.parents.contains(&ca));
    assert!(head_lib(&t.repo).await?.contains("    22\n"));
    Ok(())
}

/// Spec §6.4 rung 3: with no harness, a conflicted change is arbitrated
/// directly; the resolution lands as a change whose parents include both
/// colliding changes, and a signed `Arbitrated` event names the arbiter.
#[tokio::test]
async fn arbitration_lands_a_change_with_both_parents() -> TestResult {
    let t = ladder_repo(TWO_ATTEMPTS, None, Arc::new(StubVerifier)).await?;
    let (ca, cb) = collide(&t.repo).await?;
    t.repo.land_local().await?;
    assert_eq!(t.repo.status(cb).await?.status, QueueStatus::Conflicted);

    let arbiter = Arbiter {
        actor: hord_core::Actor::Human { id: "ann".into() },
        key_id: Some("ann-key".into()),
        signature: Some(Bytes::new(vec![7; 64])),
    };
    let (resolution, pending) = t
        .repo
        .arbitrate(cb, Arbitration::PickTheirs, arbiter.clone())
        .await?;
    assert_eq!(
        pending
            .escalation
            .as_ref()
            .and_then(|e| e.resolution.as_ref())
            .map(|r| r.change),
        Some(resolution)
    );
    // A second decision while one is pending is refused.
    assert!(
        t.repo
            .arbitrate(cb, Arbitration::PickOurs, arbiter.clone())
            .await
            .is_err()
    );
    t.repo.land_local().await?;

    let entry = t.repo.status(cb).await?;
    let QueueStatus::Arbitrated { landed } = entry.status else {
        return Err(format!("expected Arbitrated, got {entry:#?}").into());
    };
    assert_eq!(landed, resolution, "landed on the head it was made on");
    let record = t.repo.change(landed).await?;
    assert_eq!(record.parents, vec![ca, cb]);
    assert_eq!(record.provenance.actor, arbiter.actor);
    assert!(head_lib(&t.repo).await?.contains("    21\n"), "theirs won");
    assert_eq!(t.repo.head().await?.change, Some(landed));

    let events = events(&t.repo).await?;
    let arbitrated = events
        .iter()
        .find_map(|k| match k {
            Kind::Arbitrated(a) => Some(a.clone()),
            _ => None,
        })
        .ok_or("an Arbitrated event")?;
    assert_eq!(arbitrated.change, cb.to_hex());
    assert_eq!(arbitrated.result, landed.to_hex());
    assert_eq!(
        arbitrated.by.as_ref().map(hord_api::wire::actor_id),
        Some("ann")
    );
    assert_eq!(arbitrated.key_id.as_deref(), Some("ann-key"));
    assert_eq!(arbitrated.signature, Some(vec![7; 64]));

    // Arbitrated is final.
    assert!(
        t.repo
            .arbitrate(cb, Arbitration::PickOurs, arbiter)
            .await
            .is_err()
    );
    Ok(())
}

/// "Keep ours" changes nothing, and still lands to record the decision and
/// its parents.
#[tokio::test]
async fn keeping_ours_lands_an_empty_change_with_both_parents() -> TestResult {
    let t = ladder_repo(TWO_ATTEMPTS, None, Arc::new(StubVerifier)).await?;
    let (ca, cb) = collide(&t.repo).await?;
    t.repo.land_local().await?;
    let before = t.repo.head().await?;
    let (resolution, _) = t
        .repo
        .arbitrate(
            cb,
            Arbitration::PickOurs,
            Arbiter {
                actor: actor("arbiter"),
                key_id: None,
                signature: None,
            },
        )
        .await?;
    t.repo.land_local().await?;
    let entry = t.repo.status(cb).await?;
    assert_eq!(entry.status, QueueStatus::Arbitrated { landed: resolution });
    let record = t.repo.change(resolution).await?;
    assert_eq!(record.parents, vec![ca, cb]);
    assert!(record.ops.is_empty());
    let after = t.repo.head().await?;
    assert_eq!(after.change, Some(resolution));
    assert_eq!(after.snapshot, before.snapshot, "ours: nothing changed");
    assert!(head_lib(&t.repo).await?.contains("    20\n"));
    Ok(())
}

/// An arbiter's replay runs once more with the note, past the limit, and
/// parks again if it does not land.
#[tokio::test]
async fn an_arbiter_can_ask_for_one_more_replay_with_a_note() -> TestResult {
    let harness = Scripted::new([Step::GiveUp, Step::GiveUp]);
    let t = ladder_repo(
        "[land]\nmax_replay_attempts = 1\n",
        Some(Arc::new(harness.clone())),
        Arc::new(StubVerifier),
    )
    .await?;
    let (_, cb) = collide(&t.repo).await?;
    t.repo.land_local().await?;
    assert_eq!(
        t.repo.status(cb).await?.status,
        QueueStatus::NeedsArbitration
    );
    let (id, entry) = t
        .repo
        .arbitrate(
            cb,
            Arbitration::Replay {
                note: Some("keep beta's doc comment".into()),
            },
            Arbiter {
                actor: actor("arbiter"),
                key_id: None,
                signature: None,
            },
        )
        .await?;
    assert_eq!(id, cb);
    assert_eq!(entry.status, QueueStatus::Replaying { attempt: 2 });
    t.repo.land_local().await?;
    let entry = t.repo.status(cb).await?;
    assert_eq!(entry.status, QueueStatus::NeedsArbitration, "{entry:#?}");
    let attempts = &escalation(&entry)?.attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].outcome, ReplayOutcome::GaveUp);
    assert_eq!(attempts[1].note.as_deref(), Some("keep beta's doc comment"));
    let requests = harness.requests();
    assert_eq!(
        requests.get(1).and_then(|r| r.note.as_deref()),
        Some("keep beta's doc comment")
    );
    Ok(())
}
