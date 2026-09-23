//! Rebased records (ADR 0018), birth ids (ADR 0019), and file moves
//! (ADR 0020), from the system review's repros (items 3, 8, and 9).

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::*;
use hord_core::{
    ChangeId, ChangeRecord, Evidence, EvidenceKind, IdentityDelta, NodeId, Op, TreeOpKind,
};
use hord_txn::{QueueStatus, Repo, RepoOptions, Verdict, Verifier, VerifyFuture, VerifyRequest};

const TWINS_BASE: &str = "pub fn a() -> u32 {\n    1\n}\n\npub fn m() -> u32 {\n    3\n}\n\npub fn z() -> u32 {\n    2\n}\n";
const HELPER: &str = "\npub fn helper() -> u32 {\n    7\n}\n";

/// Name → id of every definition of `file` at head.
async fn head_ids(repo: &Repo, file: &str) -> BTreeMap<String, NodeId> {
    let head = repo.head().await.unwrap().snapshot;
    repo.definitions_in(head, path(file))
        .await
        .unwrap()
        .into_iter()
        .map(|d| {
            (
                d.name.map(|n| n.as_str().to_owned()).unwrap_or_default(),
                d.node,
            )
        })
        .collect()
}

/// Every id of a definition named `name` in `file` at head, in source order.
async fn ids_named(repo: &Repo, file: &str, name: &str) -> Vec<NodeId> {
    let head = repo.head().await.unwrap().snapshot;
    repo.definitions_in(head, path(file))
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.name.as_ref().is_some_and(|n| n.as_str().ends_with(name)))
        .map(|d| d.node)
        .collect()
}

fn births(record: &ChangeRecord) -> Vec<NodeId> {
    record
        .identity_deltas
        .iter()
        .filter_map(|d| match d {
            IdentityDelta::Birth { node } => Some(*node),
            _ => None,
        })
        .collect()
}

fn deaths(record: &ChangeRecord) -> Vec<NodeId> {
    record
        .identity_deltas
        .iter()
        .filter_map(|d| match d {
            IdentityDelta::Death { node } => Some(*node),
            _ => None,
        })
        .collect()
}

async fn landed(repo: &Repo, submitted: ChangeId) -> ChangeId {
    match repo.status(submitted).await.unwrap().status {
        QueueStatus::Landed { landed } => landed,
        other => panic!("{submitted} did not land: {other:?}"),
    }
}

/// The review's twins repro (system review item 3): two agents add the same
/// `helper` to `a.rs` from one base, at different places. The second lands
/// rebased. Its record states the birth it actually made, and `node_history`
/// follows the landed deltas (ADR 0018).
#[tokio::test]
async fn a_rebased_record_states_what_it_landed() {
    let t = stub_repo(&[("src/a.rs", TWINS_BASE)]).await;
    let repo = &t.repo;
    let mut w1 = begin(repo, "x").await;
    let mut w2 = begin(repo, "y").await;
    edit(
        &mut w1,
        "src/a.rs",
        TWINS_BASE,
        "}\n",
        &format!("}}\n{HELPER}"),
    )
    .await;
    edit(
        &mut w2,
        "src/a.rs",
        TWINS_BASE,
        "    3\n}\n",
        &format!("    3\n}}\n{HELPER}"),
    )
    .await;
    let p1 = w1.propose(intent("helper top")).await.unwrap();
    let p2 = w2.propose(intent("helper bottom")).await.unwrap();
    repo.submit(p1.change).await.unwrap();
    repo.submit(p2.change).await.unwrap();
    repo.land_local().await.unwrap();

    let first = landed(repo, p1.change).await;
    let second = landed(repo, p2.change).await;
    assert_eq!(first, p1.change, "landed as proposed");
    assert_ne!(second, p2.change, "landed rebased");
    let record = repo.change(second).await.unwrap();

    // Recomputed from head → result.
    let head = repo.change(first).await.unwrap().result;
    assert_eq!(record.base, head);
    assert_eq!(record.parents, vec![first]);
    let helpers = ids_named(repo, "src/a.rs", "helper").await;
    assert_eq!(helpers.len(), 2);
    assert_ne!(helpers[0], helpers[1], "two lifetimes, two ids");
    let born = births(&record);
    assert_eq!(born.len(), 1, "{:?}", record.identity_deltas);
    assert!(helpers.contains(&born[0]));
    assert!(record.write_set.contains(&born[0]));
    assert!(!record.write_set.contains(&births(&p1.record)[0]));
    // Kept from the submitted record; the signature is not the author's.
    assert_eq!(record.read_set, p2.record.read_set);
    assert_eq!(record.intent, p2.record.intent);
    assert_eq!(record.provenance, p2.record.provenance);
    // The submitted evidence, then the lander's attestation (ADR 0018
    // amendment): `Rebase { submitted }` on the landed result.
    assert_eq!(
        record.evidence[..record.evidence.len() - 1],
        p2.record.evidence[..]
    );
    let attestation: Evidence = repo
        .store()
        .get_object(*record.evidence.last().unwrap())
        .unwrap();
    assert_eq!(
        attestation.kind,
        EvidenceKind::Rebase {
            submitted: p2.change
        }
    );
    assert_eq!(attestation.snapshot, record.result);
    assert_eq!(record.signature, None);
    assert_eq!(record.rebased_from, Some(p2.change));
    assert_eq!(repo.store().rebased_to(p2.change).unwrap(), Some(second));

    // `node_history` follows the landed records' deltas.
    for id in &helpers {
        let history = repo.store().node_history(*id).unwrap();
        assert_eq!(history.len(), 1, "{id}: {history:?}");
    }
    assert_eq!(repo.store().node_history(born[0]).unwrap(), vec![second]);
}

/// ADR 0019, concurrent collision: two changes from one base add the same
/// definition at the same site of the same file, so both derive the same
/// birth id. The one that lands second re-derives it with the head snapshot,
/// and its landed record carries the final id.
#[tokio::test]
async fn a_concurrent_identical_birth_is_rederived_with_head() {
    let t = stub_repo(&[("src/a.rs", TWINS_BASE)]).await;
    let repo = &t.repo;
    let with_helper = TWINS_BASE.replacen("    3\n}\n", &format!("    3\n}}\n{HELPER}"), 1);
    let mut w1 = begin(repo, "x").await;
    let mut w2 = begin(repo, "y").await;
    // w1 also edits `z`, so the two results differ and w2 is not a no-op.
    w1.write_file(
        &path("src/a.rs"),
        with_helper.replace("    2\n", "    20\n"),
    )
    .await
    .unwrap();
    w2.write_file(&path("src/a.rs"), with_helper.clone())
        .await
        .unwrap();
    let p1 = w1.propose(intent("helper and z")).await.unwrap();
    let p2 = w2.propose(intent("helper")).await.unwrap();
    let (b1, b2) = (births(&p1.record), births(&p2.record));
    assert_eq!(b1, b2, "same content, file, site, and base: same birth id");
    repo.submit(p1.change).await.unwrap();
    repo.submit(p2.change).await.unwrap();
    repo.land_local().await.unwrap();

    let second = landed(repo, p2.change).await;
    let record = repo.change(second).await.unwrap();
    let born = births(&record);
    assert_eq!(born.len(), 1, "{:?}", record.identity_deltas);
    assert_ne!(born[0], b2[0], "the collision was re-derived");
    let helpers = ids_named(repo, "src/a.rs", "helper").await;
    assert!(
        helpers.contains(&b1[0]) && helpers.contains(&born[0]),
        "{helpers:?}"
    );
    assert_eq!(
        repo.store().node_history(b1[0]).unwrap(),
        vec![landed(repo, p1.change).await]
    );
    assert_eq!(repo.store().node_history(born[0]).unwrap(), vec![second]);
}

/// ADR 0019, resurrection (system review item 9): delete `foo` and land, then
/// re-add an identical `foo` and land. It is a new lifetime with a new id.
#[tokio::test]
async fn a_readded_definition_gets_a_new_id() {
    let foo = "pub fn foo() -> u32 {\n    1\n}\n\npub fn bar() -> u32 {\n    2\n}\n";
    let t = repo(&[("src/a.rs", foo)]).await;
    let repo = &t.repo;
    let old = head_ids(repo, "src/a.rs").await["foo"];
    let mut ws = begin(repo, "x").await;
    ws.write_file(&path("src/a.rs"), "pub fn bar() -> u32 {\n    2\n}\n")
        .await
        .unwrap();
    let deleted = submit(repo, &mut ws, "delete foo").await;
    repo.land_local().await.unwrap();
    let mut ws = begin(repo, "y").await;
    ws.write_file(&path("src/a.rs"), foo).await.unwrap();
    let readded = submit(repo, &mut ws, "re-add foo").await;
    repo.land_local().await.unwrap();

    let new = head_ids(repo, "src/a.rs").await["foo"];
    assert_ne!(new, old, "a re-added definition is born again");
    assert_eq!(
        repo.store().node_history(old).unwrap(),
        vec![landed(repo, deleted).await],
        "the old lifetime ends at its deletion"
    );
    assert_eq!(
        repo.store().node_history(new).unwrap(),
        vec![landed(repo, readded).await]
    );
}

const MOVED: &str = "\
pub fn foo() -> u32 {
    1
}

pub fn bar() -> u32 {
    foo() + 1
}

pub struct Thing {
    pub a: u32,
}
";

/// ADR 0020 (system review item 8): `git mv` keeps every NodeId. The change
/// records a tree rename plus moves, no births or deaths, and lands.
#[tokio::test]
async fn moving_a_file_keeps_every_node_id() {
    let t = repo(&[("src/a.rs", MOVED), ("README.md", "x\n")]).await;
    let repo = &t.repo;
    let before = head_ids(repo, "src/a.rs").await;
    let mut ws = begin(repo, "x").await;
    ws.delete_file(&path("src/a.rs")).await.unwrap();
    ws.write_file(&path("src/b.rs"), MOVED).await.unwrap();
    let proposal = ws.propose(intent("move a -> b")).await.unwrap();
    let record = &proposal.record;
    assert!(
        births(record).is_empty() && deaths(record).is_empty(),
        "{:?}",
        record.identity_deltas
    );
    assert!(record.ops.iter().any(|op| matches!(
        op,
        Op::Tree { path: p, kind: TreeOpKind::Rename { to } }
            if *p == path("src/a.rs") && *to == path("src/b.rs")
    )));
    let moves = record
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Move { .. }))
        .count();
    assert_eq!(
        moves,
        before.len() - 1,
        "each top-level definition moves (the field stays in its struct)"
    );
    assert!(
        record
            .write_set
            .contains(&hord_txn::path_node_id(&path("src/a.rs")))
    );
    assert!(
        record
            .write_set
            .contains(&hord_txn::path_node_id(&path("src/b.rs")))
    );
    repo.submit(proposal.change).await.unwrap();
    repo.land_local().await.unwrap();
    landed(repo, proposal.change).await;
    assert_eq!(head_ids(repo, "src/b.rs").await, before);
    // Blame follows the definition through the move.
    let history = repo.store().node_history(before["foo"]).unwrap();
    assert_eq!(history.last(), Some(&proposal.change));
}

/// A larger file, so one edit and one addition keep the pair above ADR
/// 0007's 0.8 (7 of 8 definitions shared, Dice 14/17).
fn moved_long() -> String {
    let mut text = MOVED.to_owned();
    for n in 1..=4 {
        text.push_str(&format!("\npub fn p{n}() -> u32 {{\n    {n}\n}}\n"));
    }
    text
}

/// ADR 0020: a move that also edits the file keeps the ids §3.4 still
/// matches; a new definition is a birth. It lands after an unrelated change,
/// so it is rebased and re-validated on the way.
#[tokio::test]
async fn a_move_with_an_edit_keeps_matched_ids_and_lands_rebased() {
    let t = repo(&[("src/a.rs", &moved_long()), ("README.md", "x\n")]).await;
    let repo = &t.repo;
    let before = head_ids(repo, "src/a.rs").await;
    let mut other = begin(repo, "other").await;
    other.write_file(&path("README.md"), "y\n").await.unwrap();
    let unrelated = submit(repo, &mut other, "readme").await;

    let mut ws = begin(repo, "x").await;
    ws.delete_file(&path("src/a.rs")).await.unwrap();
    let edited =
        moved_long().replace("foo() + 1", "foo() + 2") + "\npub fn baz() -> u32 {\n    9\n}\n";
    ws.write_file(&path("src/b.rs"), edited).await.unwrap();
    let moved = submit(repo, &mut ws, "move and edit").await;
    repo.land_local().await.unwrap();
    landed(repo, unrelated).await;
    let landed_id = landed(repo, moved).await;
    assert_ne!(landed_id, moved, "rebased onto the readme change");

    let after = head_ids(repo, "src/b.rs").await;
    for name in ["foo", "bar", "Thing", "p1", "p4"] {
        assert_eq!(after[name], before[name], "{name} keeps its id");
    }
    let record = repo.change(landed_id).await.unwrap();
    assert_eq!(births(&record), vec![after["baz"]]);
    assert!(deaths(&record).is_empty());
    assert!(
        record.write_set.contains(&after["bar"]),
        "bar's body changed"
    );
}

/// Below ADR 0007's threshold the files are not paired: a small file with
/// half its definitions changed is a deletion and a creation.
#[tokio::test]
async fn a_dissimilar_created_file_is_not_a_move() {
    let t = repo(&[("src/a.rs", MOVED)]).await;
    let repo = &t.repo;
    let mut ws = begin(repo, "x").await;
    ws.delete_file(&path("src/a.rs")).await.unwrap();
    let edited = MOVED.replace("foo() + 1", "foo() + 2") + "\npub fn baz() -> u32 {\n    9\n}\n";
    ws.write_file(&path("src/b.rs"), edited).await.unwrap();
    let proposal = ws.propose(intent("rewrite")).await.unwrap();
    assert!(!proposal.record.ops.iter().any(|op| matches!(
        op,
        Op::Tree {
            kind: TreeOpKind::Rename { .. },
            ..
        }
    )));
    assert!(!deaths(&proposal.record).is_empty());
}

/// ADR 0020: a concurrent edit to the old path conflicts with the move.
#[tokio::test]
async fn an_edit_of_the_old_path_conflicts_with_a_move() {
    let t = repo(&[("src/a.rs", MOVED)]).await;
    let repo = &t.repo;
    let mut editor = begin(repo, "editor").await;
    edit(&mut editor, "src/a.rs", MOVED, "    1\n", "    10\n").await;
    let mut mover = begin(repo, "mover").await;
    mover.delete_file(&path("src/a.rs")).await.unwrap();
    mover.write_file(&path("src/b.rs"), MOVED).await.unwrap();
    let edited = submit(repo, &mut editor, "edit foo").await;
    let moved = submit(repo, &mut mover, "move").await;
    repo.land_local().await.unwrap();
    landed(repo, edited).await;
    let entry = repo.status(moved).await.unwrap();
    assert_eq!(entry.status, QueueStatus::Conflicted, "{:?}", entry.report);
    let report = entry.report.unwrap();
    assert!(
        report
            .conflicts
            .iter()
            .any(|c| c.paths.contains(&path("src/a.rs")))
            || report.merge.iter().any(|m| m.path == path("src/a.rs")),
        "{report:?}"
    );
}

/// A verifier that fails every rebased record and remembers their ids.
#[derive(Default)]
struct FailRebased {
    failed: Mutex<Vec<ChangeId>>,
}

impl Verifier for FailRebased {
    fn verify<'a>(&'a self, request: VerifyRequest<'a>) -> VerifyFuture<'a> {
        let verdict = if request.change.rebased_from.is_some() {
            let mut failed = self.failed.lock().unwrap();
            failed.push(request.change_id);
            failed.extend(request.change.evidence.last().copied());
            Verdict::Fail {
                reason: "rebased".into(),
            }
        } else {
            Verdict::Pass
        };
        Box::pin(async move { verdict })
    }
}

/// ADR 0018: the rebased record is stored only when it lands, so a failed
/// verification leaves no orphan record.
#[tokio::test]
async fn a_rebased_record_that_fails_verification_is_not_stored() {
    let verifier = Arc::new(FailRebased::default());
    let t = repo_with(
        &fixture(),
        RepoOptions {
            verifier: Some(verifier.clone()),
            ..RepoOptions::default()
        },
    )
    .await;
    let repo = &t.repo;
    let mut a = begin(repo, "a").await;
    edit(&mut a, "README.md", README, "line one\n", "line 1\n").await;
    let mut b = begin(repo, "b").await;
    edit(&mut b, "src/other.rs", OTHER, "    2\n", "    20\n").await;
    let pa = submit(repo, &mut a, "a").await;
    let pb = submit(repo, &mut b, "b").await;
    repo.land_local().await.unwrap();
    landed(repo, pa).await;
    assert_eq!(
        repo.status(pb).await.unwrap().status,
        QueueStatus::Conflicted
    );
    let failed = verifier.failed.lock().unwrap().clone();
    assert_eq!(
        failed.len(),
        2,
        "b was verified rebased, with an attestation"
    );
    assert_ne!(failed[0], pb);
    assert!(
        !repo.store().contains(failed[0]).unwrap(),
        "no orphan rebased record"
    );
    assert!(
        !repo.store().contains(failed[1]).unwrap(),
        "no orphan rebase attestation"
    );
    assert!(
        repo.store().contains(pb).unwrap(),
        "the submitted record stays"
    );
}
