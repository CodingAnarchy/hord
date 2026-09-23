//! Regressions from the M3 quality review (`docs/review/quality.md`).

mod common;

use common::*;
use hord_core::{ChangeId, Timestamp};
use hord_txn::{
    ConflictKind, ConflictReport, QueueStatus, ReadDeclaration, Repo, RepoOptions, path_node_id,
};
use serde::{Deserialize, Serialize};

fn landed(status: &QueueStatus) -> bool {
    matches!(status, QueueStatus::Landed { .. })
}

async fn head_file(repo: &Repo, file: &str) -> String {
    let mut ws = begin(repo, "reader").await;
    let bytes = ws.read_file(&path(file)).await.unwrap().unwrap();
    String::from_utf8(bytes.as_slice().to_vec()).unwrap()
}

const SIZES: &str = "\
pub fn sizes() -> Vec<u32> {
    let v = vec![0; 3];
    v
}
";

/// Finding 1: `vec![0; 3]` → `vec![0, 3]` changes only a `;` leaf. It is a
/// write of `sizes`, and a concurrent edit of `sizes` must not revert it.
#[tokio::test]
async fn separator_only_code_change_is_a_write_and_is_not_reverted() {
    let t = repo(&[("src/lib.rs", SIZES)]).await;
    let mut a = begin(&t.repo, "a").await;
    let mut b = begin(&t.repo, "b").await;
    let sizes = def(&mut a, "src/lib.rs", "sizes").await;
    edit(&mut a, "src/lib.rs", SIZES, "vec![0; 3]", "vec![0, 3]").await;
    edit(&mut b, "src/lib.rs", SIZES, "    v\n}", "    v.clone()\n}").await;
    let pa = a.propose(intent("a")).await.unwrap();
    assert!(
        pa.record.write_set.contains(&sizes),
        "{:?}",
        pa.record.write_set
    );
    t.repo.submit(pa.change).await.unwrap();
    let cb = submit(&t.repo, &mut b, "b").await;
    let done = t.repo.land_local().await.unwrap();
    assert!(landed(&done[0].status));
    let report = t.repo.conflicts(cb).await.unwrap();
    assert!(report.has(ConflictKind::WriteWrite), "{report:#?}");
    let head = head_file(&t.repo, "src/lib.rs").await;
    assert!(
        head.contains("vec![0, 3]"),
        "a's landed edit was reverted:\n{head}"
    );
}

/// Finding 1: a comment after a statement's `;` is content of the function.
#[tokio::test]
async fn comment_after_a_semicolon_is_a_write_and_is_not_reverted() {
    const SRC: &str = "pub fn beta() -> u32 {\n    let x = 2; // two\n    x\n}\n";
    let t = repo(&[("src/lib.rs", SRC)]).await;
    let mut a = begin(&t.repo, "a").await;
    let mut b = begin(&t.repo, "b").await;
    let beta = def(&mut a, "src/lib.rs", "beta").await;
    edit(&mut a, "src/lib.rs", SRC, "// two", "// two, see issue 7").await;
    edit(&mut b, "src/lib.rs", SRC, "    x\n}", "    x + 1\n}").await;
    let pa = a.propose(intent("a")).await.unwrap();
    assert!(
        pa.record.write_set.contains(&beta),
        "{:?}",
        pa.record.write_set
    );
    t.repo.submit(pa.change).await.unwrap();
    submit(&t.repo, &mut b, "b").await;
    let done = t.repo.land_local().await.unwrap();
    assert!(landed(&done[0].status));
    let head = head_file(&t.repo, "src/lib.rs").await;
    assert!(
        head.contains("see issue 7"),
        "a's landed edit was reverted:\n{head}"
    );
}

/// Finding 1: trailing trivia on an item's own `;` writes that item.
#[tokio::test]
async fn trailing_trivia_on_an_items_semicolon_is_a_write() {
    let t = repo(&fixture()).await;
    let mut a = begin(&t.repo, "a").await;
    let module = def(&mut a, "src/lib.rs", "other").await;
    edit(
        &mut a,
        "src/lib.rs",
        LIB,
        "mod other;\n",
        "mod other; // x\n",
    )
    .await;
    let pa = a.propose(intent("a")).await.unwrap();
    assert!(
        pa.record.write_set.contains(&module),
        "{:?}",
        pa.record.write_set
    );
}

/// Finding 1 guard: separators between child definitions are still not
/// content. Adding a field is a birth, not a write of the struct.
#[tokio::test]
async fn adding_a_field_is_not_a_write_of_the_struct() {
    const SRC: &str = "pub struct S {\n    pub a: u32,\n    pub b: u32,\n}\n";
    let t = repo(&[("src/lib.rs", SRC)]).await;
    let mut a = begin(&t.repo, "a").await;
    let s = def(&mut a, "src/lib.rs", "S").await;
    edit(
        &mut a,
        "src/lib.rs",
        SRC,
        "    pub b: u32,\n",
        "    pub b: u32,\n    pub c: u32,\n",
    )
    .await;
    let pa = a.propose(intent("a")).await.unwrap();
    assert!(
        !pa.record.write_set.contains(&s),
        "{:?}",
        pa.record.write_set
    );
}

/// Finding 2: an agent that read a missing path, or declared it, conflicts
/// with a change that creates it.
#[tokio::test]
async fn reading_or_declaring_a_missing_file_conflicts_with_its_creation() {
    for declare in [false, true] {
        let t = repo(&fixture()).await;
        let mut a = begin(&t.repo, "a").await;
        let mut b = begin(&t.repo, "b").await;
        if declare {
            a.declare_read(ReadDeclaration::Path(path("src/new.rs")));
        } else {
            assert!(a.read_file(&path("src/new.rs")).await.unwrap().is_none());
            assert!(a.access_log().read_paths.contains(&path("src/new.rs")));
        }
        edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
        b.write_file(&path("src/new.rs"), "pub fn fresh() -> u32 {\n    7\n}\n")
            .await
            .unwrap();
        let cb = submit(&t.repo, &mut b, "b creates").await;
        let rb = t.repo.change(cb).await.unwrap();
        assert!(rb.write_set.contains(&path_node_id(&path("src/new.rs"))));
        let ca = submit(&t.repo, &mut a, "a assumes absent").await;
        t.repo.land_local().await.unwrap();
        let report = t.repo.conflicts(ca).await.unwrap();
        assert!(
            report.has(ConflictKind::ReadWrite),
            "declare={declare}: {report:#?}"
        );
    }
}

/// Finding 2: two changes creating one parsed path overlap in the set check,
/// not only in the merge.
#[tokio::test]
async fn creating_the_same_path_twice_is_a_write_write_conflict() {
    let t = repo(&fixture()).await;
    let mut a = begin(&t.repo, "a").await;
    let mut b = begin(&t.repo, "b").await;
    a.write_file(&path("src/new.rs"), "pub fn by_a() -> u32 {\n    1\n}\n")
        .await
        .unwrap();
    b.write_file(&path("src/new.rs"), "pub fn by_b() -> u32 {\n    2\n}\n")
        .await
        .unwrap();
    submit(&t.repo, &mut a, "a").await;
    let cb = submit(&t.repo, &mut b, "b").await;
    let done = t.repo.land_local().await.unwrap();
    assert!(landed(&done[0].status));
    assert_eq!(done[1].status, QueueStatus::Conflicted);
    let report = t.repo.conflicts(cb).await.unwrap();
    let ww = report
        .conflicts
        .iter()
        .find(|c| c.kind == ConflictKind::WriteWrite)
        .unwrap_or_else(|| panic!("{report:#?}"));
    assert_eq!(ww.paths, vec![path("src/new.rs")]);
}

/// Finding 7: submitting a landed change again (by either id) is a no-op.
#[tokio::test]
async fn resubmitting_a_landed_change_is_a_no_op() {
    let t = repo(&fixture()).await;
    let mut a = begin(&t.repo, "a").await;
    let mut b = begin(&t.repo, "b").await;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    edit(&mut b, "src/lib.rs", LIB, "    4\n", "    40\n").await;
    let ca = submit(&t.repo, &mut a, "a").await;
    let cb = submit(&t.repo, &mut b, "b").await;
    t.repo.land_local().await.unwrap();
    let QueueStatus::Landed { landed: lb } = t.repo.status(cb).await.unwrap().status else {
        panic!("b did not land");
    };
    let log = t.repo.store().log().unwrap().len();
    for id in [ca, cb, lb] {
        let entry = t.repo.submit(id).await.unwrap();
        assert!(landed(&entry.status), "{entry:?}");
    }
    assert!(t.repo.land_local().await.unwrap().is_empty());
    assert_eq!(t.repo.queue().await.unwrap().len(), 2);
    assert_eq!(t.repo.store().log().unwrap().len(), log);
}

/// Finding 7: a change whose effect head already has (the same edit landed
/// by someone else) appends nothing to the log.
#[tokio::test]
async fn an_already_applied_change_appends_nothing() {
    let t = repo(&fixture()).await;
    let mut a = begin(&t.repo, "a").await;
    let mut b = begin(&t.repo, "b").await;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    20\n").await;
    // Different intents, so different records with the same result.
    submit(&t.repo, &mut a, "a").await;
    submit(&t.repo, &mut b, "b").await;
    let done = t.repo.land_local().await.unwrap();
    assert!(landed(&done[0].status));
    let QueueStatus::Rejected { reason } = &done[1].status else {
        panic!("{:?}", done[1].status);
    };
    assert!(reason.contains("already applied"), "{reason}");
    assert_eq!(t.repo.store().log().unwrap().len(), 2, "bootstrap + a only");
}

#[derive(Serialize, Deserialize)]
struct StoredEntry {
    change: ChangeId,
    status: QueueStatus,
    submitted_at: Timestamp,
    updated_at: Timestamp,
    report: Option<ConflictReport>,
}

async fn seeded(dir: &std::path::Path) -> Repo {
    let store = hord_store::Store::create(dir).unwrap();
    let repo = Repo::from_store(store, RepoOptions::default())
        .await
        .unwrap();
    let files = fixture()
        .into_iter()
        .map(|(p, s)| (path(p), s.as_bytes().to_vec()))
        .collect();
    repo.bootstrap(files, intent("seed"), actor("seed"))
        .await
        .unwrap();
    repo
}

/// Lander crash recovery (lander.rs module doc): an entry whose `Landed`
/// status became durable while its log append did not is queued again on
/// the next run, and lands.
#[tokio::test]
async fn a_landed_entry_missing_from_the_log_is_requeued_on_restart() {
    let dir = temp_dir("recover");
    let change = {
        let repo = seeded(&dir).await;
        let mut a = begin(&repo, "a").await;
        edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
        let change = submit(&repo, &mut a, "a").await;
        // The state a crash between the durable queue write and set_head
        // leaves: status landed, change not in the log.
        let bytes = repo.store().queue_entry(0).unwrap().unwrap();
        let mut stored: StoredEntry = hord_encoding::decode(&bytes).unwrap();
        stored.status = QueueStatus::Landed { landed: change };
        let bytes = hord_encoding::encode(&stored).unwrap();
        repo.store().queue_set(0, &bytes).unwrap();
        let head_change = repo.head().await.unwrap().change.unwrap();
        repo.store().set_head(head_change).unwrap();
        change
    };
    let reopened = Repo::open(&dir).await.unwrap();
    assert!(landed(&reopened.queue().await.unwrap()[0].status));
    let done = reopened.land_local().await.unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].status, QueueStatus::Landed { landed: change });
    assert_eq!(reopened.head().await.unwrap().change, Some(change));
    assert!(
        head_file(&reopened, "src/lib.rs")
            .await
            .contains("    20\n")
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 6: a deterministic failure while rebasing one change (here the
/// identity object head records for the file is gone) parks that change as
/// rejected, with its report. `land_local` does not fail, and the queue
/// moves on.
#[tokio::test]
async fn a_deterministic_rebase_failure_parks_instead_of_wedging() {
    let dir = temp_dir("wedge");
    let (cb, cc) = {
        let repo = seeded(&dir).await;
        let mut a = begin(&repo, "a").await;
        let mut b = begin(&repo, "b").await;
        edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await;
        edit(&mut b, "src/lib.rs", LIB, "    4\n", "    40\n").await;
        submit(&repo, &mut a, "a").await;
        repo.land_local().await.unwrap();
        let pb = b.propose(intent("b")).await.unwrap();
        repo.submit(pb.change).await.unwrap();
        // Lose the identity object head records for the file b edits.
        let head = repo.head().await.unwrap().snapshot;
        let lost = file_identity(repo.store(), head, "src/lib.rs").expect("a carried identity");
        remove_loose_object(&dir, lost);
        // A later change to another file, queued behind b.
        let mut c = begin(&repo, "c").await;
        edit(&mut c, "README.md", README, "line one\n", "line 1\n").await;
        let pc = c.propose(intent("c")).await.unwrap();
        repo.submit(pc.change).await.unwrap();
        (pb.change, pc.change)
    };
    let reopened = Repo::open(&dir).await.unwrap();
    let done = reopened
        .land_local()
        .await
        .expect("a bad change must not fail the lander");
    let b = done.iter().find(|e| e.change == cb).unwrap();
    let QueueStatus::Rejected { reason } = &b.status else {
        panic!("{:?}", b.status);
    };
    assert!(!reason.is_empty());
    assert!(b.report.is_some(), "the set check's report is kept");
    let c = done.iter().find(|e| e.change == cc).unwrap();
    assert!(!matches!(c.status, QueueStatus::Queued), "{:?}", c.status);
    assert!(reopened.land_local().await.unwrap().is_empty());
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Landing is deterministic: the same changes landed in two fresh
/// repositories give the same snapshot and the same `NodeId`s, through
/// glue edits, births, a nested impl, and a 3-way file merge.
#[tokio::test]
async fn landing_the_same_changes_twice_gives_the_same_ids() {
    async fn run_once() -> (hord_core::SnapshotId, Vec<(String, String)>) {
        let t = repo(&fixture()).await;
        let mut a = begin(&t.repo, "a").await;
        let mut b = begin(&t.repo, "b").await;
        let mut c = begin(&t.repo, "c").await;
        edit(
            &mut a,
            "src/lib.rs",
            LIB,
            "mod other;\n",
            "mod other;\nuse std::fmt;\n\npub fn born_a() -> u32 {\n    if true { 1 } else { 2 }\n}\n",
        )
        .await;
        edit(
            &mut b,
            "src/lib.rs",
            LIB,
            "mod other;\n",
            "mod other;\nuse std::io;\n\npub struct Born;\n\nimpl Born {\n    pub fn m(&self) -> u32 {\n        3\n    }\n}\n",
        )
        .await;
        edit(
            &mut c,
            "src/lib.rs",
            LIB,
            "    4\n",
            "    let q = 4;\n    q\n",
        )
        .await;
        for (ws, s) in [(&mut a, "a"), (&mut b, "b"), (&mut c, "c")] {
            submit(&t.repo, ws, s).await;
        }
        let done = t.repo.land_local().await.unwrap();
        assert!(done.iter().all(|d| landed(&d.status)), "{done:#?}");
        let head = t.repo.head().await.unwrap().snapshot;
        let defs = t
            .repo
            .definitions_in(head, path("src/lib.rs"))
            .await
            .unwrap()
            .into_iter()
            .map(|d| {
                let name = d.name.map(|n| n.as_str().to_owned()).unwrap_or_default();
                (name, d.node.to_string())
            })
            .collect();
        (head, defs)
    }
    let first = run_once().await;
    let second = run_once().await;
    assert_eq!(first, second);
}
