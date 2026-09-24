//! ADR 0024 layering in `hord-txn`: the lander as a task with an event
//! stream, the persisted event log, and reads through an [`ObjectSource`].

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use hord_api::proto::event::Kind;
use hord_api::{EventStream, proto};
use hord_core::{ObjectId, SnapshotId};
use hord_txn::{Base, BeginOptions, Lander, ObjectSource, QueueStatus, Repo, RepoOptions};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

async fn next_kind(stream: &mut EventStream) -> Kind {
    let envelope = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("an event in time")
        .expect("stream open")
        .expect("no stream error");
    envelope.event.unwrap().kind.unwrap()
}

/// Kind names, for comparing sequences.
fn name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Submitted(_) => "submitted",
        Kind::ConflictCheck(_) => "conflict_check",
        Kind::Verifying(_) => "verifying",
        Kind::EvidenceAttached(_) => "evidence_attached",
        Kind::Replaying(_) => "replaying",
        Kind::Parked(_) => "parked",
        Kind::Arbitrated(_) => "arbitrated",
        Kind::Landed(_) => "landed",
        Kind::Rejected(_) => "rejected",
        Kind::HeadMoved(_) => "head_moved",
    }
}

/// A spawned lander lands what is submitted without `land_local`, reports
/// it on its stream, and stops when cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lander_task_lands_submissions_and_stops_on_cancel() {
    let t = repo(&fixture()).await;
    let cancel = CancellationToken::new();
    let (task, mut events) = Lander::spawn(t.repo.clone(), cancel.clone()).unwrap();
    let mut ws = begin(&t.repo, "a").await;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    let change = submit(&t.repo, &mut ws, "twenty").await;
    let mut seen = Vec::new();
    loop {
        let kind = next_kind(&mut events).await;
        seen.push(name(&kind));
        if let Kind::HeadMoved(moved) = &kind {
            assert_eq!(moved.to, change.to_hex());
            break;
        }
    }
    assert_eq!(
        seen,
        ["submitted", "conflict_check", "landed", "head_moved"]
    );
    assert_eq!(t.repo.head().await.unwrap().change, Some(change));
    assert!(matches!(
        t.repo.status(change).await.unwrap().status,
        QueueStatus::Landed { .. }
    ));
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the lander stops when cancelled")
        .unwrap();
}

/// `land_local` emits the same events, and the log keeps them with their
/// cursors across a reopen, so a reader resumes where it left off.
#[tokio::test]
async fn events_persist_across_a_reopen() {
    let dir = temp_dir("events");
    let options = || RepoOptions {
        verifier: Some(Arc::new(hord_txn::StubVerifier)),
        ..RepoOptions::default()
    };
    let (first, second) = {
        let repo = Repo::create_with(&dir, options()).await.unwrap();
        let files = fixture()
            .into_iter()
            .map(|(p, s)| (path(p), s.as_bytes().to_vec()))
            .collect();
        repo.bootstrap(files, intent("seed"), actor("seed"))
            .await
            .unwrap();
        let mut a = begin(&repo, "a").await;
        edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
        let first = submit(&repo, &mut a, "a").await;
        let mut b = begin(&repo, "b").await;
        edit(&mut b, "src/lib.rs", LIB, "    4\n", "    40\n").await;
        let second = submit(&repo, &mut b, "b").await;
        let done = repo.land_local().await.unwrap();
        assert_eq!(done.len(), 2);
        (first, second)
    };
    let repo = Repo::open_with(&dir, options()).await.unwrap();
    let mut all = repo.events(Some(0)).await.unwrap();
    let mut kinds = Vec::new();
    let mut cursors = Vec::new();
    // bootstrap: landed, head_moved; then two submissions and two landings.
    while kinds.len() < 10 {
        let envelope = tokio::time::timeout(Duration::from_secs(10), all.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        cursors.push(envelope.cursor);
        kinds.push(envelope.event.unwrap().kind.unwrap());
    }
    assert_eq!(cursors, (1..=10).collect::<Vec<u64>>());
    assert_eq!(
        kinds.iter().map(name).collect::<Vec<_>>(),
        [
            "landed",
            "head_moved",
            "submitted",
            "submitted",
            // The speculative window (ADR 0025) prepares the second change
            // on the first before the first lands.
            "conflict_check",
            "conflict_check",
            "landed",
            "head_moved",
            "landed",
            "head_moved",
        ]
    );
    let Kind::Landed(landed) = &kinds[6] else {
        unreachable!()
    };
    assert_eq!(
        (landed.change.as_str(), landed.position),
        (first.to_hex().as_str(), 1)
    );
    // The second change landed rebased: the event names both ids.
    let Kind::Landed(rebased) = &kinds[8] else {
        unreachable!()
    };
    assert_eq!(rebased.submitted.as_deref(), Some(second.to_hex().as_str()));
    assert_eq!(rebased.position, 2);
    // Resume after cursor 7: exactly the last three.
    let mut resumed = repo.events(Some(7)).await.unwrap();
    for expected in 8..=10 {
        let e = resumed.next().await.unwrap().unwrap();
        assert_eq!(e.cursor, expected);
    }
    drop(resumed);
    drop(all);
    drop(repo);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Parked and rejected changes are reported as such.
#[tokio::test]
async fn parked_changes_emit_parked() {
    let t = repo(&fixture()).await;
    let mut a = begin(&t.repo, "a").await;
    let mut b = begin(&t.repo, "b").await;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    21\n").await;
    submit(&t.repo, &mut a, "a").await;
    let parked = submit(&t.repo, &mut b, "b").await;
    let mut events = t.repo.events(None).await.unwrap();
    t.repo.land_local().await.unwrap();
    loop {
        if let Kind::Parked(p) = next_kind(&mut events).await {
            assert_eq!(p.change, parked.to_hex());
            assert_ne!(p.reason, i32::from(proto::ParkReason::Unspecified));
            assert!(!p.detail.is_empty());
            break;
        }
    }
}

/// Counts reads and serves them from another repository's objects.
struct Counting {
    inner: Repo,
    reads: AtomicUsize,
}

impl ObjectSource for Counting {
    fn get_objects(&self, ids: &[ObjectId]) -> hord_txn::Result<Vec<Vec<u8>>> {
        self.reads.fetch_add(ids.len(), Ordering::Relaxed);
        self.inner.get_objects(ids)
    }

    fn has(&self, ids: &[ObjectId]) -> hord_txn::Result<Vec<bool>> {
        ObjectSource::has(&self.inner, ids)
    }
}

/// A repository whose store holds none of a snapshot's objects reads them
/// through its [`ObjectSource`] (ADR 0024, spec §8.3): a workspace on that
/// snapshot reads files and proposes a change, and only the objects it
/// touched were fetched.
#[tokio::test]
async fn a_workspace_reads_through_the_object_source() {
    let origin = repo(&fixture()).await;
    let snapshot: SnapshotId = origin.repo.head().await.unwrap().snapshot;
    let source = Arc::new(Counting {
        inner: origin.repo.clone(),
        reads: AtomicUsize::new(0),
    });
    let dir = temp_dir("sourced");
    let local = Repo::create_with(
        &dir,
        RepoOptions {
            objects: Some(source.clone()),
            ..RepoOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(!local.store().contains(snapshot).unwrap());
    let mut ws = local
        .begin(BeginOptions {
            base: Base::Snapshot(snapshot),
            ..BeginOptions::at_head(actor("remote"))
        })
        .await
        .unwrap();
    assert_eq!(source.reads.load(Ordering::Relaxed), 0, "begin is O(1)");
    let readme = ws.read_file(&path("README.md")).await.unwrap().unwrap();
    assert_eq!(readme.as_slice(), README.as_bytes());
    assert!(source.reads.load(Ordering::Relaxed) > 0);
    assert_eq!(
        ws.objects().snapshot_identity(snapshot).unwrap(),
        origin.repo.snapshot_identity(snapshot).unwrap()
    );
    edit(&mut ws, "README.md", README, "line two", "line 2").await;
    let proposal = ws.propose(intent("remote edit")).await.unwrap();
    assert_eq!(proposal.record.base, snapshot);
    // The proposal's new objects are written locally.
    assert!(local.store().contains(proposal.change).unwrap());
    assert!(local.store().contains(proposal.record.result).unwrap());
    drop(ws);
    drop(local);
    let _ = std::fs::remove_dir_all(&dir);
}
