//! Verification and policy at landing (spec §6.5, §7, §12 M4; ADRs 0022,
//! 0025, 0026): evidence reuse, policy parks and rejections, the
//! speculative window, impact sets, and `hord verify`'s library side.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use hord_core::{Actor, Evidence, EvidenceKind, EvidenceResult, SnapshotId, Timestamp};
use hord_policy::EvidenceState;
use hord_txn::{
    EngineVerifier, LocalRepo, QueueStatus, Repo, RepoOptions, Verdict, Verifier, VerifierFactory,
    VerifyFuture, VerifyRequest,
};
use hord_verify::{
    Check, Checkout, CoverageRecord, EvidenceFields, EvidenceIndex, ImpactSet, Toolchain,
    VerifyPlan, VerifyPolicy,
};

/// Plans one `true <requirement>` check per requirement it can produce
/// (everything but `review:*`) and counts what it runs.
struct FakeRunner {
    toolchain: Toolchain,
    runs: Arc<AtomicUsize>,
}

fn kind_of(requirement: &str) -> (EvidenceKind, Option<String>) {
    let (kind, qualifier) = match requirement.split_once(':') {
        Some((k, q)) => (k, Some(q.to_owned())),
        None => (requirement, None),
    };
    let kind = match kind {
        "check" => EvidenceKind::Check,
        "test" => EvidenceKind::Test,
        "lint" => EvidenceKind::Lint,
        "bench" => EvidenceKind::Bench,
        other => EvidenceKind::Custom(other.to_owned()),
    };
    (kind, qualifier)
}

impl hord_verify::Verifier for FakeRunner {
    fn lang(&self) -> &str {
        "fake"
    }

    fn toolchain(&self) -> &Toolchain {
        &self.toolchain
    }

    fn plan(
        &self,
        snapshot: SnapshotId,
        impact: &ImpactSet,
        policy: &VerifyPolicy,
    ) -> hord_verify::Result<VerifyPlan> {
        let mut plan = VerifyPlan::new(snapshot, self.toolchain.id()?);
        for requirement in policy.require.iter().filter(|r| !r.starts_with("review")) {
            let (kind, qualifier) = kind_of(requirement);
            plan.checks.push(Check {
                requirement: requirement.clone(),
                kind,
                qualifier,
                program: "true".into(),
                args: vec![requirement.clone()],
                env: BTreeMap::new(),
                dir: hord_core::RepoPath::default(),
                scope: Some(impact.node_set()),
            });
        }
        Ok(plan)
    }

    fn run(
        &self,
        checkout: &Checkout,
        plan: &VerifyPlan,
        _logs: &dyn EvidenceIndex,
    ) -> hord_verify::Result<Vec<Evidence>> {
        assert_eq!(checkout.snapshot, plan.snapshot);
        Ok(plan
            .checks
            .iter()
            .map(|check| {
                self.runs.fetch_add(1, Ordering::SeqCst);
                EvidenceFields {
                    kind: check.kind.clone(),
                    qualifier: check.qualifier.clone(),
                    snapshot: plan.snapshot,
                    toolchain: plan.toolchain,
                    command: check.command(),
                    scope: check.scope.clone(),
                    result: EvidenceResult::Pass,
                    log: None,
                    cost_ms: 1,
                    produced_by: Actor::Human { id: "fake".into() },
                    produced_at: Timestamp::from_millis(1),
                }
                .build()
            })
            .collect())
    }
}

struct FakeFactory {
    runs: Arc<AtomicUsize>,
    toolchain: bool,
}

impl VerifierFactory for FakeFactory {
    fn toolchain(&self) -> Option<Toolchain> {
        self.toolchain
            .then(|| Toolchain::new("fake").with("fake", "1"))
    }

    fn build(
        &self,
        _checkout: &Checkout,
        _coverage: Option<Arc<CoverageRecord>>,
    ) -> hord_verify::Result<Box<dyn hord_verify::Verifier>> {
        Ok(Box::new(FakeRunner {
            toolchain: self.toolchain().unwrap(),
            runs: Arc::clone(&self.runs),
        }))
    }
}

const POLICY: &str = "\
[land]
require = [\"check\", \"test:selected\"]

[[rule]]
name = \"src needs review\"
when = { paths = [\"src/**\"] }
require = [\"review:human\"]
";

async fn policed(policy: &str, toolchain: bool) -> (TempRepo, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let factory = FakeFactory {
        runs: Arc::clone(&runs),
        toolchain,
    };
    let mut files = fixture();
    files.push((".hord-policy.toml", policy));
    let options = RepoOptions {
        verifier: Some(Arc::new(EngineVerifier::new(Arc::new(factory)))),
        ..RepoOptions::default()
    };
    (repo_with(&files, options).await, runs)
}

fn review(snapshot: SnapshotId, result: EvidenceResult) -> Vec<u8> {
    let evidence = EvidenceFields {
        kind: EvidenceKind::Review,
        qualifier: Some("human".into()),
        snapshot,
        toolchain: hord_core::ObjectId::from_canonical(b"reviewer"),
        command: "hord review --as human".into(),
        scope: None,
        result,
        log: None,
        cost_ms: 0,
        produced_by: Actor::Human { id: "ada".into() },
        produced_at: Timestamp::from_millis(2),
    }
    .build();
    hord_encoding::encode(&evidence).unwrap()
}

async fn attach(repo: &Repo, change: hord_core::ChangeId, bytes: Vec<u8>) {
    use hord_api::RepoBackend;
    LocalRepo::without_lander(repo.clone())
        .attach_evidence(hord_api::proto::AttachEvidenceRequest {
            change: change.to_hex(),
            evidence: bytes,
        })
        .await
        .unwrap();
}

async fn land_one(repo: &Repo, change: hord_core::ChangeId) -> hord_txn::QueueEntry {
    repo.submit(change).await.unwrap();
    let done = repo.land_local().await.unwrap();
    done.into_iter()
        .find(|e| e.change == change)
        .expect("processed")
}

/// Spec §12 M4: resubmitting an unchanged change against an unchanged head
/// re-runs nothing. The change is parked for a missing review; resubmitted,
/// it is parked again with zero commands run; reviewed and resubmitted, it
/// lands, still with zero commands run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resubmitting_an_unchanged_change_runs_nothing() {
    let (t, runs) = policed(POLICY, true).await;
    let mut ws = begin(&t.repo, "a").await;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    let proposal = ws.propose(intent("beta twenty")).await.unwrap();
    let change = proposal.change;
    let head = t.repo.head().await.unwrap();

    let first = land_one(&t.repo, change).await;
    let QueueStatus::Parked { reason } = &first.status else {
        panic!("{first:#?}");
    };
    assert!(reason.contains("review:human"), "{reason}");
    let report = first.report.as_ref().unwrap();
    assert_eq!(report.policy.len(), 1, "{report:#?}");
    let violation = &report.policy[0];
    assert_eq!(violation.rule.as_deref(), Some("src needs review"));
    assert_eq!(violation.evidence, EvidenceState::Absent);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "check and test:selected ran"
    );

    let again = land_one(&t.repo, change).await;
    assert!(matches!(again.status, QueueStatus::Parked { .. }));
    assert_eq!(runs.load(Ordering::SeqCst), 2, "nothing re-ran");
    assert_eq!(t.repo.head().await.unwrap(), head, "head did not move");

    attach(
        &t.repo,
        change,
        review(proposal.record.result, EvidenceResult::Pass),
    )
    .await;
    let mut events = t.repo.events(None).await.unwrap();
    let landed = land_one(&t.repo, change).await;
    assert_eq!(landed.status, QueueStatus::Landed { landed: change });
    assert_eq!(runs.load(Ordering::SeqCst), 2, "still nothing re-ran");
    // The verdict's evidence is announced and listed in Landed.
    use tokio_stream::StreamExt;
    let mut attached = 0;
    loop {
        let event = events.next().await.unwrap().unwrap();
        match event.event.unwrap().kind.unwrap() {
            hord_api::proto::event::Kind::EvidenceAttached(_) => attached += 1,
            hord_api::proto::event::Kind::Landed(l) => {
                assert_eq!(l.evidence.len(), 2, "{l:?}");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(attached, 2);
}

/// A required check that failed on this very snapshot cannot pass: the
/// change is rejected, with the violation machine-readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_required_evidence_rejects() {
    let (t, _) = policed(POLICY, true).await;
    let mut ws = begin(&t.repo, "a").await;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    let proposal = ws.propose(intent("beta twenty")).await.unwrap();
    attach(
        &t.repo,
        proposal.change,
        review(
            proposal.record.result,
            EvidenceResult::Fail {
                summary: "no".into(),
            },
        ),
    )
    .await;
    let entry = land_one(&t.repo, proposal.change).await;
    let QueueStatus::Rejected { reason } = &entry.status else {
        panic!("{entry:#?}");
    };
    assert!(reason.contains("review:human"), "{reason}");
    let violation = &entry.report.as_ref().unwrap().policy[0];
    assert_eq!(violation.evidence, EvidenceState::Failed);
}

/// Without a toolchain the engine verifies nothing, and the policy still
/// holds: a required check that nothing produced parks the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_toolchain_policy_still_applies() {
    let (t, runs) = policed("[land]\nrequire = [\"check\"]\n", false).await;
    let mut ws = begin(&t.repo, "a").await;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    let change = ws.propose(intent("beta twenty")).await.unwrap().change;
    let entry = land_one(&t.repo, change).await;
    assert!(
        matches!(&entry.status, QueueStatus::Parked { reason } if reason.contains("check")),
        "{entry:#?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// Fails the change whose summary is `bad`, slowly, and counts calls.
#[derive(Default)]
struct FailBad {
    calls: AtomicUsize,
}

impl Verifier for FailBad {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bad = request.change.intent.summary == "bad";
        Box::pin(async move {
            if bad {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Verdict::Fail {
                    evidence: Vec::new(),
                    reason: "bad".into(),
                }
            } else {
                Verdict::Pass {
                    evidence: Vec::new(),
                }
            }
        })
    }
}

/// ADR 0025: the window prepares and verifies later candidates stacked on
/// earlier ones; when one fails, those behind it are prepared again on the
/// real head and land there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_candidate_invalidates_the_window_behind_it() {
    let verifier = Arc::new(FailBad::default());
    let t = repo_with(
        &fixture(),
        RepoOptions {
            verifier: Some(Arc::clone(&verifier) as Arc<dyn Verifier>),
            ..RepoOptions::default()
        },
    )
    .await;
    let head = t.repo.head().await.unwrap();
    let mut changes = Vec::new();
    for (who, file, content, from, to, summary) in [
        ("a", "src/lib.rs", LIB, "    2\n", "    20\n", "bad"),
        ("b", "src/lib.rs", LIB, "    4\n", "    40\n", "delta"),
        ("c", "README.md", README, "line one", "line 1", "readme"),
    ] {
        let mut ws = begin(&t.repo, who).await;
        edit(&mut ws, file, content, from, to).await;
        let change = ws.propose(intent(summary)).await.unwrap().change;
        t.repo.submit(change).await.unwrap();
        changes.push(change);
    }
    let done = t.repo.land_local().await.unwrap();
    let statuses: Vec<_> = done.iter().map(|e| e.status.clone()).collect();
    assert_eq!(statuses[0], QueueStatus::Conflicted, "{done:#?}");
    let QueueStatus::Landed { landed: b } = statuses[1] else {
        panic!("{done:#?}");
    };
    assert!(
        matches!(statuses[2], QueueStatus::Landed { .. }),
        "{done:#?}"
    );
    // b was verified once stacked on a (abandoned), then again on head.
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 5);
    let b = t.repo.change(b).await.unwrap();
    assert_eq!(b.parents, head.change.into_iter().collect::<Vec<_>>());
    assert_eq!(b.base, head.snapshot);
}

/// The impact set follows References dependents through the resolver:
/// editing `alpha` impacts `gamma`, which calls it, and not `beta`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn impact_sets_follow_resolved_references() {
    let t = repo(&fixture()).await;
    let mut ws = begin(&t.repo, "a").await;
    let alpha = def(&mut ws, "src/lib.rs", "alpha").await;
    let gamma = def(&mut ws, "src/lib.rs", "gamma").await;
    let beta = def(&mut ws, "src/lib.rs", "beta").await;
    edit(&mut ws, "src/lib.rs", LIB, "    1\n", "    10\n").await;
    let record = ws.propose(intent("alpha ten")).await.unwrap().record;
    let impact = t.repo.impact(record).await.unwrap();
    assert!(impact.write_set.contains(&alpha));
    assert_eq!(impact.nodes.get(&gamma), Some(&1), "{impact:#?}");
    assert!(!impact.nodes.contains_key(&beta), "{impact:#?}");
    assert!(
        impact.facts.touched.iter().any(|t| t.node == alpha),
        "{:#?}",
        impact.facts
    );
}

/// `hord verify` plans with reuse, runs, and indexes evidence under the
/// proposal's snapshot, which the lander then reuses: nothing runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_workspace_attaches_evidence_the_lander_reuses() {
    let (t, runs) = policed("[land]\nrequire = [\"check\", \"lint\"]\n", true).await;
    let mut ws = begin(&t.repo, "a").await;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    let plan = t.repo.verify_workspace(&mut ws, true).await.unwrap();
    assert_eq!(
        plan.requirements
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["check", "lint"]
    );
    assert_eq!(plan.plan.as_ref().unwrap().checks.len(), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 0, "plan only");
    let ran = t.repo.verify_workspace(&mut ws, false).await.unwrap();
    assert!(ran.verdict.passed());
    assert_eq!(ran.verdict.evidence().len(), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    let again = t.repo.verify_workspace(&mut ws, true).await.unwrap();
    assert_eq!(again.plan.unwrap().reused.len(), 2, "reused next time");
    let change = ws.propose(intent("beta twenty")).await.unwrap().change;
    let entry = land_one(&t.repo, change).await;
    assert_eq!(entry.status, QueueStatus::Landed { landed: change });
    assert_eq!(runs.load(Ordering::SeqCst), 2, "the lander reused it");
}

/// The default verifier runs cargo for head's policy (ADR 0022): a clean
/// edit lands with `check` evidence; a compile error parks as a semantic
/// conflict (spec §6.5) with cargo's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_verifier_runs_cargo() {
    if hord_verify_rust::detect_toolchain(&std::env::temp_dir()).is_err() {
        eprintln!("no Rust toolchain; skipped");
        return;
    }
    let mut files = fixture();
    files.push((".hord-policy.toml", "[land]\nrequire = [\"check\"]\n"));
    let t = repo_with(&files, RepoOptions::default()).await;
    let mut ws = begin(&t.repo, "a").await;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    let good = ws.propose(intent("beta twenty")).await.unwrap();
    let entry = land_one(&t.repo, good.change).await;
    assert_eq!(
        entry.status,
        QueueStatus::Landed {
            landed: good.change
        },
        "{entry:#?}"
    );
    let evidence = t.repo.store().evidence_at(good.record.result).unwrap();
    assert_eq!(evidence.len(), 1, "one cargo check");

    let mut ws = begin(&t.repo, "b").await;
    let lib = String::from_utf8(
        ws.read_file(&path("src/lib.rs"))
            .await
            .unwrap()
            .unwrap()
            .as_slice()
            .to_vec(),
    )
    .unwrap();
    edit(&mut ws, "src/lib.rs", &lib, "    4\n", "    not_a_value\n").await;
    let bad = ws.propose(intent("broken")).await.unwrap();
    let entry = land_one(&t.repo, bad.change).await;
    assert_eq!(entry.status, QueueStatus::Conflicted, "{entry:#?}");
    let reason = entry.report.unwrap().verification.unwrap();
    assert!(reason.contains("cargo check"), "{reason}");
}

/// Policy facts name each written definition's visibility and the kinds
/// inside it (ADR 0026): an `unsafe` block in a `pub fn`, a `pub(crate)`
/// function, and a field (not an item, so read from the whole file).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_facts_carry_kinds_and_visibility() {
    let lib = "pub struct S {\n    pub x: u32,\n}\n\npub fn a() -> u32 {\n    1\n}\n\npub(crate) fn b() -> u32 {\n    2\n}\n";
    let t = repo(&[("src/lib.rs", lib)]).await;
    let mut ws = begin(&t.repo, "a").await;
    let edited = lib
        .replace("    1\n", "    unsafe { 1 }\n")
        .replace("    2\n", "    3\n")
        .replace("pub x: u32", "pub x: u64");
    ws.write_file(&path("src/lib.rs"), edited).await.unwrap();
    let record = ws.propose(intent("edits")).await.unwrap().record;
    let facts = t.repo.policy_facts(record).await.unwrap();
    let by_name = |kind: &str| {
        facts
            .definitions
            .iter()
            .find(|d| d.kinds.contains(kind))
            .unwrap_or_else(|| panic!("no {kind} in {facts:#?}"))
    };
    let a = by_name("unsafe_block");
    assert_eq!(a.visibility.as_deref(), Some("pub"));
    assert!(a.kinds.contains("function_item"));
    let fields: Vec<_> = facts
        .definitions
        .iter()
        .filter(|d| d.kinds.contains("field_declaration"))
        .collect();
    assert_eq!(fields.len(), 1, "{facts:#?}");
    assert_eq!(fields[0].visibility.as_deref(), Some("pub"));
    assert!(
        facts
            .definitions
            .iter()
            .any(|d| d.visibility.as_deref() == Some("pub(crate)")),
        "{facts:#?}"
    );
}
