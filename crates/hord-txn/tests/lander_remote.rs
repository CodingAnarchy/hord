//! Changes submitted to a lander that did not propose them, and queue
//! lookups by id across processes (M3 perf review #5 and #7,
//! `docs/review/perf.md`).

mod common;

use common::*;
use hord_txn::{QueueStatus, Repo, RepoOptions};

fn landed(status: &QueueStatus) -> bool {
    matches!(status, QueueStatus::Landed { .. })
}

fn stub() -> RepoOptions {
    RepoOptions {
        verifier: Some(std::sync::Arc::new(hord_txn::StubVerifier)),
        ..RepoOptions::default()
    }
}

/// A bootstrapped repository in its own directory, which outlives the
/// returned handle so another `Repo` can open it.
async fn seeded(tag: &str) -> TestResult<(std::path::PathBuf, Repo)> {
    let dir = temp_dir(tag)?;
    let repo = Repo::create_with(&dir, stub()).await?;
    let files = fixture()
        .into_iter()
        .map(|(p, s)| (path(p), s.as_bytes().to_vec()))
        .collect();
    repo.bootstrap(files, intent("seed"), actor("seed")).await?;
    Ok((dir, repo))
}

/// `submit` records that `propose` checked the ops, so a lander opened
/// later (another process) does not validate the change again.
#[tokio::test]
async fn submit_records_that_propose_checked_the_ops() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    let proposal = a.propose(intent("a")).await?;
    assert!(!t.repo.store().is_checked(proposal.change)?);
    t.repo.submit(proposal.change).await?;
    assert!(t.repo.store().is_checked(proposal.change)?);
    Ok(())
}

/// A change proposed by another repository is validated by the lander,
/// ahead of its turn, and the outcome is recorded.
#[tokio::test]
async fn changes_proposed_elsewhere_are_validated_and_land() -> TestResult {
    let (dir, repo) = seeded("remote").await?;
    let mut changes = Vec::new();
    for (who, from, to) in [
        ("a", "    1\n", "    10\n"),
        ("b", "    2\n", "    20\n"),
        ("c", "alpha() + 1", "alpha() + 3"),
        ("d", "    4\n", "    40\n"),
    ] {
        let mut ws = begin(&repo, who).await?;
        edit(&mut ws, "src/lib.rs", LIB, from, to).await?;
        changes.push(ws.propose(intent(who)).await?.change);
    }
    drop(repo);
    let lander = Repo::open_with(&dir, stub()).await?;
    for change in &changes {
        lander.submit(*change).await?;
        assert!(!lander.store().is_checked(*change)?);
    }
    let done = lander.land_local().await?;
    assert_eq!(done.len(), changes.len());
    assert!(done.iter().all(|e| landed(&e.status)), "{done:#?}");
    for change in &changes {
        assert!(lander.store().is_checked(*change)?);
    }
    let mut ws = begin(&lander, "reader").await?;
    let head = ws
        .read_file(&path("src/lib.rs"))
        .await?
        .ok_or("src/lib.rs at head")?;
    let head = String::from_utf8(head.as_slice().to_vec())?;
    for text in ["    10\n", "    20\n", "alpha() + 3", "    40\n"] {
        assert!(head.contains(text), "{head}");
    }
    drop(lander);
    let _ = remove_tree(&dir);
    Ok(())
}

/// Looking ahead does not let a bad record through: queued behind and in
/// front of valid changes from elsewhere, it is rejected and never marked.
#[tokio::test]
async fn a_forged_record_among_changes_from_elsewhere_is_rejected() -> TestResult {
    let (dir, repo) = seeded("remote").await?;
    let mut ws = begin(&repo, "a").await?;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    let good = ws.propose(intent("a")).await?;
    let mut forged = good.record.clone();
    forged.result = forged.base;
    forged.intent.summary = "forged".into();
    let forged = repo.store().put_object(&forged)?;
    let mut ws_b = begin(&repo, "b").await?;
    edit(&mut ws_b, "README.md", README, "line one\n", "line 1\n").await?;
    let other = ws_b.propose(intent("b")).await?;
    drop((ws, ws_b, repo));
    let lander = Repo::open_with(&dir, stub()).await?;
    for change in [good.change, forged, other.change] {
        lander.submit(change).await?;
    }
    let done = lander.land_local().await?;
    assert!(landed(&done[0].status), "{done:#?}");
    assert!(
        matches!(done[1].status, QueueStatus::Rejected { .. }),
        "{done:#?}"
    );
    assert!(landed(&done[2].status), "{done:#?}");
    assert!(!lander.store().is_checked(forged)?);
    drop(lander);
    let _ = remove_tree(&dir);
    Ok(())
}

/// `status` and `submit` find an entry by its submitted or its rebased id
/// through the store's index, also in a process that did not queue it.
#[tokio::test]
async fn status_and_resubmit_find_rebased_entries_after_reopen() -> TestResult {
    let (dir, repo) = seeded("remote").await?;
    let mut a = begin(&repo, "a").await?;
    let mut b = begin(&repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "README.md", README, "line one\n", "line 1\n").await?;
    submit(&repo, &mut a, "a").await?;
    let cb = submit(&repo, &mut b, "b").await?;
    let done = repo.land_local().await?;
    let QueueStatus::Landed { landed: lb } = done[1].status else {
        return Err(format!("{done:#?}").into());
    };
    assert_ne!(lb, cb, "b lands rebased onto a");
    drop((a, b, repo));
    let repo = Repo::open_with(&dir, stub()).await?;
    let by_submitted = repo.status(cb).await?;
    let by_landed = repo.status(lb).await?;
    assert_eq!(by_submitted, by_landed);
    assert_eq!(by_submitted.seq, done[1].seq);
    // Submitting a landed change again, by either id, queues nothing.
    assert_eq!(repo.submit(cb).await?.seq, done[1].seq);
    assert_eq!(repo.submit(lb).await?.seq, done[1].seq);
    assert_eq!(repo.queue().await?.len(), 2);
    assert!(
        repo.status(hord_core::ObjectId::from_bytes([7; 32]))
            .await
            .is_err()
    );
    drop(repo);
    let _ = remove_tree(&dir);
    Ok(())
}
