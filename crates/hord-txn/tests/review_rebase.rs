//! ADR 0031: a `review:*` on the submitted record's result carries across a
//! clean structural rebase, and not across one that merged anything.

mod common;

use std::sync::Arc;

use common::*;
use hord_core::{Actor, ChangeId, EvidenceKind, EvidenceResult, SnapshotId, Timestamp};
use hord_txn::{LocalRepo, QueueStatus, Repo, RepoOptions, StubVerifier};
use hord_verify::EvidenceFields;

/// Only `src/**` needs a human review; nothing else is required.
const POLICY: &str = "[[rule]]\nname = \"src needs review\"\nwhen = { paths = [\"src/**\"] }\nrequire = [\"review:human\"]\n";

async fn reviewed_repo() -> TestResult<TempRepo> {
    let mut files = fixture();
    files.push((".hord-policy.toml", POLICY));
    repo_with(
        &files,
        RepoOptions {
            verifier: Some(Arc::new(StubVerifier)),
            ..RepoOptions::default()
        },
    )
    .await
}

/// Sign a passing human review of `snapshot` and attach it to `change`.
async fn review(repo: &Repo, change: ChangeId, snapshot: SnapshotId) -> TestResult {
    use hord_api::RepoBackend;
    let evidence = EvidenceFields {
        kind: EvidenceKind::Review,
        qualifier: Some("human".into()),
        snapshot,
        toolchain: hord_core::ObjectId::from_canonical(b"reviewer"),
        command: "hord review --as human".into(),
        scope: None,
        result: EvidenceResult::Pass,
        log: None,
        cost_ms: 0,
        produced_by: Actor::Human { id: "ada".into() },
        produced_at: Timestamp::from_millis(2),
    }
    .build();
    LocalRepo::without_lander(repo.clone())
        .attach_evidence(hord_api::proto::AttachEvidenceRequest {
            change: change.to_hex(),
            evidence: hord_encoding::encode(&evidence)?,
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_review_carries_across_a_clean_rebase() -> TestResult {
    let t = reviewed_repo().await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "README.md", README, "line one", "line ONE").await?;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    let pa = a.propose(intent("readme")).await?;
    let pb = b.propose(intent("beta twenty")).await?;
    review(&t.repo, pb.change, pb.record.result).await?;
    t.repo.submit(pa.change).await?;
    t.repo.submit(pb.change).await?;
    let done = t.repo.land_local().await?;
    assert_eq!(done[0].status, QueueStatus::Landed { landed: pa.change });
    let QueueStatus::Landed { landed } = done[1].status else {
        return Err(format!("the reviewed change should land: {:#?}", done[1]).into());
    };
    assert_ne!(landed, pb.change, "it landed rebased");
    let report = done[1].report.as_ref().ok_or("a report")?;
    assert!(report.merge.is_empty() && report.adapter_merged.is_empty());
    Ok(())
}

/// A rebase that an adapter merge resolved (ADR 0013) changed what the
/// reviewer saw: the review stays with the submitted snapshot.
#[tokio::test]
async fn a_review_does_not_carry_across_a_rebase_that_merged() -> TestResult {
    let (lock, a_lock, b_lock) = lock_additions();
    let mut files = fixture();
    files.push(("Cargo.lock", &lock));
    files.push((".hord-policy.toml", POLICY));
    let t = repo_with(
        &files,
        RepoOptions {
            verifier: Some(Arc::new(StubVerifier)),
            ..RepoOptions::default()
        },
    )
    .await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    a.write_file(&path("Cargo.lock"), a_lock).await?;
    // b adds the other lockfile entry (merged by the lockfile adapter on
    // rebase) and edits src/lib.rs, which needs the review.
    b.write_file(&path("Cargo.lock"), b_lock).await?;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    let pa = a.propose(intent("add zz-alpha")).await?;
    let pb = b.propose(intent("add aaa-beta, beta twenty")).await?;
    review(&t.repo, pb.change, pb.record.result).await?;
    t.repo.submit(pa.change).await?;
    t.repo.submit(pb.change).await?;
    let done = t.repo.land_local().await?;
    assert_eq!(done[0].status, QueueStatus::Landed { landed: pa.change });
    let QueueStatus::Parked { reason } = &done[1].status else {
        return Err(format!("expected a park for review: {:#?}", done[1]).into());
    };
    assert!(reason.contains("review:human"), "{reason}");
    let report = done[1].report.as_ref().ok_or("a report")?;
    assert!(!report.adapter_merged.is_empty(), "{report:#?}");
    Ok(())
}
