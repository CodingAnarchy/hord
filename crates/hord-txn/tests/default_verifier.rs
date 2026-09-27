//! The default verifier fails closed (spec §15: never land an unverified
//! merge). With no verification requirements, a change lands only when its
//! conflict report is clean; an overlap that rebases cleanly parks with the
//! reason.

mod common;

use common::*;
use hord_txn::{ConflictKind, QueueStatus};

fn landed(status: &QueueStatus) -> bool {
    matches!(status, QueueStatus::Landed { .. })
}

#[tokio::test]
async fn an_overlapping_pair_parks_the_later_change() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    let delta = def(&mut b, "src/lib.rs", "delta").await?;
    edit(&mut a, "src/lib.rs", LIB, "    4\n", "    40\n").await?;
    // b reads delta, which a writes, then edits another file: a read-write
    // overlap whose rebase is clean.
    b.read_definition(&path("src/lib.rs"), delta).await?;
    edit(&mut b, "src/other.rs", OTHER, "    1\n", "    11\n").await?;
    let ca = submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(landed(&done[0].status), "{:?}", done[0]);
    assert_eq!(done[1].status, QueueStatus::Conflicted, "{:?}", done[1]);
    assert_eq!(t.repo.head().await?.change, Some(ca));

    let report = t.repo.conflicts(cb).await?;
    assert!(report.has(ConflictKind::ReadWrite));
    assert!(!report.has_hard());
    let reason = report.verification.expect("verification reason");
    assert!(reason.contains("read-write"), "{reason}");
    Ok(())
}

#[tokio::test]
async fn a_soft_merge_alone_also_parks() -> TestResult {
    // Both edit the same line-merged blob file (README): write-write on its
    // path, merged by the line merge (soft).
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "README.md", README, "line one", "line ONE").await?;
    edit(&mut b, "README.md", README, "line five", "line FIVE").await?;
    submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(landed(&done[0].status));
    assert_eq!(done[1].status, QueueStatus::Conflicted);
    let report = t.repo.conflicts(cb).await?;
    assert!(report.verification.is_some());
    Ok(())
}

#[tokio::test]
async fn disjoint_clean_changes_land() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/other.rs", OTHER, "    2\n", "    22\n").await?;
    submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(done.iter().all(|e| landed(&e.status)), "{done:?}");
    assert!(t.repo.conflicts(cb).await?.is_clean());
    Ok(())
}

/// Concurrent `Cargo.lock` additions write the same `dependencies` node; the
/// lockfile merge resolves it, and the change lands under the default
/// verifier (ADR 0013 amendment: the exemption for adapter merges).
#[tokio::test]
async fn an_overlap_resolved_by_an_adapter_merge_lands() -> TestResult {
    let (lock, a_lock, b_lock) = lock_additions();
    let mut files = fixture();
    files.push(("Cargo.lock", &lock));
    let t = repo(&files).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    a.write_file(&path("Cargo.lock"), a_lock).await?;
    b.write_file(&path("Cargo.lock"), b_lock).await?;
    submit(&t.repo, &mut a, "add zz-alpha").await?;
    let cb = submit(&t.repo, &mut b, "add aaa-beta").await?;
    let done = t.repo.land_local().await?;
    assert!(done.iter().all(|e| landed(&e.status)), "{done:#?}");
    let report = t.repo.conflicts(cb).await?;
    assert!(report.has(ConflictKind::WriteWrite), "{report:#?}");
    assert!(report.verification.is_none());
    Ok(())
}

/// The exemption covers only the adapter-merged file: the same lockfile
/// overlap plus a read-write on a Rust definition in one change parks.
#[tokio::test]
async fn an_adapter_merge_does_not_exempt_other_overlaps_in_the_change() -> TestResult {
    let (lock, a_lock, b_lock) = lock_additions();
    let mut files = fixture();
    files.push(("Cargo.lock", &lock));
    let t = repo(&files).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    // a: lockfile addition and an edit of delta.
    a.write_file(&path("Cargo.lock"), a_lock).await?;
    edit(&mut a, "src/lib.rs", LIB, "    4\n", "    40\n").await?;
    // b: the other lockfile addition, having read delta.
    let delta = def(&mut b, "src/lib.rs", "delta").await?;
    b.read_definition(&path("src/lib.rs"), delta).await?;
    b.write_file(&path("Cargo.lock"), b_lock).await?;
    submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(landed(&done[0].status), "{:?}", done[0]);
    assert_eq!(done[1].status, QueueStatus::Conflicted, "{:?}", done[1]);
    let report = t.repo.conflicts(cb).await?;
    assert!(report.has(ConflictKind::WriteWrite), "{report:#?}");
    assert!(report.has(ConflictKind::ReadWrite), "{report:#?}");
    assert_eq!(report.adapter_merged.len(), 1, "{report:#?}");
    assert!(!report.only_adapter_merged());
    assert!(
        report
            .verification
            .ok_or("verification reason")?
            .contains("read-write")
    );
    Ok(())
}
