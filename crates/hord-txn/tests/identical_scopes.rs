//! Identical definitions in two modules resolve their references in their
//! own module (ADR 0012 read sets; positional identity). The two `helper`s
//! below are the same text in `a.rs` and `b.rs`, so they share one content
//! id; before the resolver was keyed by site, both took one copy's scope.

mod common;

use common::*;
use hord_core::NodeId;
use hord_txn::{ConflictKind, ConflictReport, QueueStatus, Repo};

const LIB: &str = "pub mod a;\npub mod b;\n";

fn files(helper: &str, a_rest: &str, b_rest: &str) -> Vec<(String, String)> {
    vec![
        ("src/lib.rs".into(), LIB.into()),
        ("src/a.rs".into(), format!("{helper}\n{a_rest}")),
        ("src/b.rs".into(), format!("{helper}\n{b_rest}")),
    ]
}

/// Both changes land (stub verifier) so each report can be inspected.
async fn repo_of(files: &[(String, String)]) -> TempRepo {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    stub_repo(&refs).await
}

async fn id_in(repo: &Repo, file: &str, name: &str) -> NodeId {
    let mut ws = begin(repo, "probe").await;
    def(&mut ws, file, name).await
}

/// Edit `file` (whose content is `text`) by replacing `from` with `to`,
/// propose, submit, and return the submitted change.
async fn change(
    repo: &Repo,
    who: &str,
    file: &str,
    text: &str,
    from: &str,
    to: &str,
) -> hord_core::ChangeId {
    let mut ws = begin(repo, who).await;
    edit(&mut ws, file, text, from, to).await;
    submit(repo, &mut ws, who).await
}

/// The id `change` landed under (a rebased change lands under a new id).
async fn landed_as(repo: &Repo, change: hord_core::ChangeId) -> hord_core::ChangeId {
    match repo.status(change).await.unwrap().status {
        QueueStatus::Landed { landed } => landed,
        other => panic!("{change} did not land: {other:?}"),
    }
}

fn read_write_nodes(report: &ConflictReport, landed: hord_core::ChangeId) -> Vec<NodeId> {
    report
        .conflicts
        .iter()
        .filter(|c| c.kind == ConflictKind::ReadWrite && c.landed == landed)
        .flat_map(|c| c.nodes.iter().copied())
        .collect()
}

/// As specified: `helper` calls `target()`. Change 1 edits `b::target`;
/// change 2 edits the second copy, `b::helper`. Change 2 conflicts
/// read-write with change 1 on `b::target`, not on `a::target`. The mirror
/// case (edits in `a`) holds too, so neither copy borrows the other's scope.
#[tokio::test]
async fn editing_a_copy_that_calls_a_changed_target_is_a_read_write_conflict() {
    let helper = "pub fn helper() -> u32 {\n    target()\n}\n";
    let a_rest = "pub fn target() -> u32 {\n    1\n}\n";
    let b_rest = "pub fn target() -> u32 {\n    2\n}\n";
    let fs = files(helper, a_rest, b_rest);
    for (module, file, text, target_body) in [
        ("b", "src/b.rs", &fs[2].1, "    2\n"),
        ("a", "src/a.rs", &fs[1].1, "    1\n"),
    ] {
        let t = repo_of(&fs).await;
        let a_target = id_in(&t.repo, "src/a.rs", "target").await;
        let b_target = id_in(&t.repo, "src/b.rs", "target").await;
        let own = if module == "b" { b_target } else { a_target };
        let other = if module == "b" { a_target } else { b_target };
        let c1 = change(&t.repo, "one", file, text, target_body, "    20\n").await;
        let c2 = change(
            &t.repo,
            "two",
            file,
            text,
            "    target()\n",
            "    target() + 1\n",
        )
        .await;
        let done = t.repo.land_local().await.unwrap();
        assert!(
            done.iter()
                .all(|e| matches!(e.status, QueueStatus::Landed { .. }))
        );
        let report = t.repo.conflicts(c2).await.unwrap();
        let nodes = read_write_nodes(&report, landed_as(&t.repo, c1).await);
        assert!(
            nodes.contains(&own),
            "{module}: no read-write on {module}::target: {report:#?}"
        );
        assert!(
            !nodes.contains(&other),
            "{module}: conflict names the other module's target"
        );
    }
}

/// The scope itself, where no over-approximation applies (a type path, not a
/// bare call or a field selector, ADR 0011): each module has its own unit
/// struct `Target`. A landed edit to `a::Target` does not conflict with an
/// edit to `b::helper`; a landed edit to `b::Target` does.
#[tokio::test]
async fn a_copy_reads_only_its_own_modules_types() {
    let helper = "pub fn helper() -> Target {\n    Target\n}\n";
    let rest = "pub struct Target;\n";
    let fs = files(helper, rest, rest);
    let t = repo_of(&fs).await;
    let a_target = id_in(&t.repo, "src/a.rs", "Target").await;
    let b_target = id_in(&t.repo, "src/b.rs", "Target").await;
    assert_ne!(
        a_target, b_target,
        "identical definitions in two files have two ids"
    );

    let widen = ("pub struct Target;", "pub(crate) struct Target;");
    let edit_a = change(&t.repo, "a-type", "src/a.rs", &fs[1].1, widen.0, widen.1).await;
    let edit_b = change(&t.repo, "b-type", "src/b.rs", &fs[2].1, widen.0, widen.1).await;
    let helper_b = change(
        &t.repo,
        "helper",
        "src/b.rs",
        &fs[2].1,
        "    Target\n}",
        "    let t = Target;\n    t\n}",
    )
    .await;
    let done = t.repo.land_local().await.unwrap();
    assert!(
        done.iter()
            .all(|e| matches!(e.status, QueueStatus::Landed { .. })),
        "{done:#?}"
    );

    let report = t.repo.conflicts(helper_b).await.unwrap();
    let with_b = read_write_nodes(&report, landed_as(&t.repo, edit_b).await);
    assert!(
        with_b.contains(&b_target),
        "b::helper must read b::Target: {report:#?}"
    );
    let with_a = read_write_nodes(&report, landed_as(&t.repo, edit_a).await);
    assert!(
        with_a.is_empty(),
        "b::helper must not read a::Target: {report:#?}"
    );
    assert!(!report.nodes().contains(&a_target));
}
