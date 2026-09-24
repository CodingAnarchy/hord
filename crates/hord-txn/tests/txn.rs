//! Propose, conflict check, rebase, and landing (spec §6).

mod common;

use common::*;
use hord_core::{ChangeRecord, NodeId, Op};
use hord_txn::{ConflictKind, Error, QueueStatus, ReadDeclaration, RepoConfig, RepoOptions};

fn landed(status: &QueueStatus) -> bool {
    matches!(status, QueueStatus::Landed { .. })
}

async fn head_file(repo: &hord_txn::Repo, file: &str) -> TestResult<String> {
    let mut ws = begin(repo, "reader").await?;
    let bytes = ws
        .read_file(&path(file))
        .await?
        .ok_or_else(|| format!("{file} missing at head"))?;
    Ok(String::from_utf8(bytes.into_vec())?)
}

#[tokio::test]
async fn propose_records_ops_sets_and_identity() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    let beta = def(&mut ws, "src/lib.rs", "beta").await?;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    let proposal = ws.propose(intent("beta returns 20")).await?;
    let record = &proposal.record;
    assert_eq!(
        record.write_set.iter().copied().collect::<Vec<_>>(),
        vec![beta]
    );
    // ADR 0015: a parsed edit is structural ops only, with no Blob header.
    assert!(!record.ops.iter().any(|op| matches!(op, Op::Blob { .. })));
    assert!(
        record
            .ops
            .iter()
            .any(|op| matches!(op, Op::Replace { node, .. } if *node == beta))
    );
    assert_eq!(
        record.parents,
        vec![t.repo.head().await?.change.ok_or("head has a change")?]
    );
    let stored: ChangeRecord = t.repo.change(proposal.change).await?;
    assert_eq!(&stored, record);

    // The result snapshot keeps beta's id.
    t.repo.submit(proposal.change).await?;
    t.repo.land_local().await?;
    let mut after = begin(&t.repo, "b").await?;
    assert_eq!(def(&mut after, "src/lib.rs", "beta").await?, beta);
    Ok(())
}

#[tokio::test]
async fn nothing_to_propose_is_an_error() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    ws.write_file(&path("src/lib.rs"), LIB).await?;
    assert!(matches!(
        ws.propose(intent("noop")).await,
        Err(Error::NothingToPropose)
    ));
    Ok(())
}

#[tokio::test]
async fn disjoint_changes_land_without_conflict() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/other.rs", OTHER, "    2\n", "    22\n").await?;
    let ca = submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert_eq!(done.len(), 2);
    for entry in &done {
        assert!(landed(&entry.status), "{entry:?}");
        assert!(
            entry
                .report
                .as_ref()
                .ok_or("landed entry has a report")?
                .is_clean(),
            "{entry:?}"
        );
    }
    assert!(head_file(&t.repo, "src/lib.rs").await?.contains("    20\n"));
    assert!(
        head_file(&t.repo, "src/other.rs")
            .await?
            .contains("    22\n")
    );
    // The first landed as proposed; the second was rebased onto it.
    assert_eq!(done[0].status, QueueStatus::Landed { landed: ca });
    let QueueStatus::Landed { landed: lb } = done[1].status else {
        unreachable!()
    };
    assert_ne!(lb, cb);
    let rebased = t.repo.change(lb).await?;
    assert_eq!(rebased.parents, vec![ca]);
    assert_eq!(t.repo.head().await?.change, Some(lb));
    assert_eq!(t.repo.status(cb).await?.seq, done[1].seq);
    assert_eq!(t.repo.status(lb).await?.seq, done[1].seq);
    Ok(())
}

#[tokio::test]
async fn rebase_of_disjoint_changes_on_the_same_file_composes() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/lib.rs", LIB, "    4\n", "    40\n").await?;
    submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(done.iter().all(|e| landed(&e.status)), "{done:#?}");
    let report = t.repo.conflicts(cb).await?;
    assert!(report.conflicts.is_empty(), "{report:#?}");
    assert!(!report.has_hard());
    let lib = head_file(&t.repo, "src/lib.rs").await?;
    assert_eq!(
        lib,
        LIB.replace("    2\n", "    20\n")
            .replace("    4\n", "    40\n")
    );
    Ok(())
}

#[tokio::test]
async fn overlapping_writes_are_a_write_write_conflict() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    let beta = def(&mut a, "src/lib.rs", "beta").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    21\n").await?;
    let ca = submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(landed(&done[0].status));
    assert_eq!(done[1].status, QueueStatus::Conflicted, "{:#?}", done[1]);
    let report = t.repo.conflicts(cb).await?;
    let ww = report
        .conflicts
        .iter()
        .find(|c| c.kind == ConflictKind::WriteWrite)
        .expect("write-write");
    assert_eq!(ww.landed, ca);
    assert!(ww.nodes.contains(&beta));
    assert!(report.has_hard());
    // Parked, not landed: head is still the first change.
    assert_eq!(t.repo.head().await?.change, Some(ca));
    let json = serde_json::to_string(&report)?;
    assert!(json.contains("WriteWrite"));
    Ok(())
}

#[tokio::test]
async fn reading_what_another_change_wrote_is_a_read_write_conflict() -> TestResult {
    let t = stub_repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    let delta = def(&mut b, "src/lib.rs", "delta").await?;
    edit(&mut a, "src/lib.rs", LIB, "    4\n", "    40\n").await?;
    // b reads delta through the API, then edits other.rs.
    b.read_definition(&path("src/lib.rs"), delta).await?;
    assert!(b.access_log().reads.contains(&delta));
    edit(&mut b, "src/other.rs", OTHER, "    1\n", "    11\n").await?;
    let ca = submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    // The rebase is clean, so it lands flagged (spec §6.4 rung 1).
    assert!(done.iter().all(|e| landed(&e.status)));
    let report = t.repo.conflicts(cb).await?;
    let rw = report
        .conflicts
        .iter()
        .find(|c| c.kind == ConflictKind::ReadWrite)
        .expect("read-write");
    assert_eq!(rw.landed, ca);
    assert_eq!(rw.nodes, vec![delta]);
    assert!(!report.is_clean());
    Ok(())
}

#[tokio::test]
async fn calling_a_function_another_change_edited_is_a_read_write_conflict() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    let alpha = def(&mut a, "src/lib.rs", "alpha").await?;
    // a edits alpha; b edits gamma (which calls alpha) without reading alpha.
    edit(&mut a, "src/lib.rs", LIB, "    1\n", "    10\n").await?;
    edit(&mut b, "src/lib.rs", LIB, "alpha() + 1", "alpha() + 2").await?;
    assert!(b.access_log().reads.is_empty());
    submit(&t.repo, &mut a, "a").await?;
    let proposal = b.propose(intent("b")).await?;
    assert!(
        proposal.record.read_set.contains(&alpha),
        "one hop of references"
    );
    t.repo.submit(proposal.change).await?;
    t.repo.land_local().await?;
    let report = t.repo.conflicts(proposal.change).await?;
    assert!(
        report
            .conflicts
            .iter()
            .any(|c| c.kind == ConflictKind::ReadWrite && c.nodes.contains(&alpha)),
        "{report:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn dropping_a_call_still_reads_the_old_callee() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut b = begin(&t.repo, "b").await?;
    let alpha = def(&mut b, "src/lib.rs", "alpha").await?;
    edit(&mut b, "src/lib.rs", LIB, "alpha() + 1", "7").await?;
    let proposal = b.propose(intent("gamma no longer calls alpha")).await?;
    assert!(proposal.record.read_set.contains(&alpha));
    Ok(())
}

#[tokio::test]
async fn write_read_is_reported_only_when_strict() -> TestResult {
    for strict in [false, true] {
        let options = RepoOptions {
            config: RepoConfig {
                strict_reads: strict,
            },
            ..RepoOptions::default()
        };
        let t = repo_with(&fixture(), options).await?;
        let mut a = begin(&t.repo, "a").await?;
        let mut b = begin(&t.repo, "b").await?;
        let beta = def(&mut a, "src/lib.rs", "beta").await?;
        a.read_definition(&path("src/lib.rs"), beta).await?;
        edit(&mut a, "src/other.rs", OTHER, "    1\n", "    11\n").await?;
        edit(&mut b, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
        submit(&t.repo, &mut a, "a").await?;
        let cb = submit(&t.repo, &mut b, "b").await?;
        t.repo.land_local().await?;
        let report = t.repo.conflicts(cb).await?;
        assert_eq!(report.has(ConflictKind::WriteRead), strict, "{report:#?}");
        assert!(!report.has(ConflictKind::WriteWrite));
    }
    Ok(())
}

#[tokio::test]
async fn blob_tier_files_conflict_by_path_and_line_merge() -> TestResult {
    let t = stub_repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "README.md", README, "line one", "line ONE").await?;
    edit(&mut b, "README.md", README, "line five", "line FIVE").await?;
    submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(done.iter().all(|e| landed(&e.status)), "{done:#?}");
    let report = t.repo.conflicts(cb).await?;
    let ww = &report.conflicts[0];
    assert_eq!(ww.kind, ConflictKind::WriteWrite);
    assert_eq!(ww.paths, vec![path("README.md")]);
    let readme = head_file(&t.repo, "README.md").await?;
    assert!(readme.contains("line ONE") && readme.contains("line FIVE"));
    Ok(())
}

#[tokio::test]
async fn cargo_lock_goes_through_the_lockfile_merge_call_site() -> TestResult {
    let lock = "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"m\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"z\"\nversion = \"1.0.0\"\n";
    let mut files = fixture();
    files.push(("Cargo.lock", lock));
    let t = repo(&files).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    let add = |after: &str, name: &str| {
        lock.replacen(
            &format!("name = \"{after}\"\nversion = \"1.0.0\"\n"),
            &format!("name = \"{after}\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\n"),
            1,
        )
    };
    a.write_file(&path("Cargo.lock"), add("a", "b")).await?;
    b.write_file(&path("Cargo.lock"), add("z", "zz")).await?;
    submit(&t.repo, &mut a, "a").await?;
    submit(&t.repo, &mut b, "b").await?;
    let done = t.repo.land_local().await?;
    assert!(done.iter().all(|e| landed(&e.status)), "{done:#?}");
    let merged = head_file(&t.repo, "Cargo.lock").await?;
    assert!(merged.contains("name = \"b\"") && merged.contains("name = \"zz\""));
    Ok(())
}

#[tokio::test]
async fn write_definition_splices_and_keeps_identity() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    let beta = def(&mut ws, "src/lib.rs", "beta").await?;
    let old = ws.read_definition(&path("src/lib.rs"), beta).await?;
    let old = String::from_utf8(old.into_vec())?;
    ws.write_definition(
        &path("src/lib.rs"),
        beta,
        old.replace("    2\n", "    200\n"),
    )
    .await?;
    assert_eq!(def(&mut ws, "src/lib.rs", "beta").await?, beta);
    assert!(ws.access_log().writes.contains(&beta));
    let proposal = ws.propose(intent("beta 200")).await?;
    assert!(proposal.record.write_set.contains(&beta));
    let lib = ws
        .read_file(&path("src/lib.rs"))
        .await?
        .ok_or("src/lib.rs missing in workspace")?;
    assert_eq!(
        lib.as_slice(),
        LIB.replace("    2\n", "    200\n").as_bytes()
    );
    Ok(())
}

#[tokio::test]
async fn read_range_logs_overlapping_definitions() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    let defs = ws.definitions(&path("src/lib.rs")).await?;
    let beta = defs
        .iter()
        .find(|d| {
            d.name
                .as_ref()
                .is_some_and(|n| n.as_str().ends_with("beta"))
        })
        .ok_or("no definition beta in src/lib.rs")?;
    let mid = (beta.span.start + beta.span.end) / 2;
    ws.read_range(&path("src/lib.rs"), mid..mid + 1).await?;
    assert_eq!(
        ws.access_log().reads.iter().copied().collect::<Vec<_>>(),
        vec![beta.node]
    );
    assert!(ws.access_log().read_paths.contains(&path("src/lib.rs")));
    Ok(())
}

#[tokio::test]
async fn declarations_join_the_read_set_and_unknown_names_fail() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    let two = def(&mut ws, "src/other.rs", "two").await?;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    ws.declare_read(ReadDeclaration::Name("two".into()));
    let proposal = ws.propose(intent("declared")).await?;
    assert!(proposal.record.read_set.contains(&two));
    ws.declare_read(ReadDeclaration::Name("no_such_fn".into()));
    assert!(matches!(
        ws.propose(intent("bad")).await,
        Err(Error::UnresolvedDeclaration(_))
    ));
    Ok(())
}

#[tokio::test]
async fn new_and_deleted_files() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    ws.write_file(&path("src/new.rs"), "pub fn fresh() {}\n")
        .await?;
    ws.delete_file(&path("README.md")).await?;
    let change = submit(&t.repo, &mut ws, "add and remove").await?;
    let record = t.repo.change(change).await?;
    assert!(
        record
            .write_set
            .contains(&NodeId::file_root(&path("README.md")))
    );
    let done = t.repo.land_local().await?;
    assert!(landed(&done[0].status));
    let mut after = begin(&t.repo, "b").await?;
    let files = after.list_files().await?;
    assert!(files.contains(&path("src/new.rs")));
    assert!(!files.contains(&path("README.md")));
    Ok(())
}

#[tokio::test]
async fn a_record_whose_ops_do_not_reproduce_its_result_is_rejected() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = begin(&t.repo, "a").await?;
    edit(&mut ws, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    let proposal = ws.propose(intent("a")).await?;
    // Claim a result the ops do not produce.
    let mut forged = proposal.record.clone();
    forged.result = forged.base;
    forged.intent.summary = "forged".into();
    let id = t.repo.store().put_object(&forged)?;
    t.repo.submit(id).await?;
    let done = t.repo.land_local().await?;
    assert!(
        matches!(done[0].status, QueueStatus::Rejected { .. }),
        "{done:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn conflicts_before_landing_checks_against_head() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/lib.rs", LIB, "    2\n", "    21\n").await?;
    submit(&t.repo, &mut a, "a").await?;
    t.repo.land_local().await?;
    let pb = b.propose(intent("b")).await?;
    let report = t.repo.conflicts(pb.change).await?;
    assert!(report.has(ConflictKind::WriteWrite));
    assert!(matches!(
        t.repo.status(pb.change).await,
        Err(Error::NotQueued(_))
    ));
    Ok(())
}

#[tokio::test]
async fn the_queue_persists_across_reopen() -> TestResult {
    let dir = temp_dir("reopen")?;
    let ca = {
        let store = hord_store::Store::create(&dir)?;
        let repo = hord_txn::Repo::from_store(store, RepoOptions::default()).await?;
        let files = fixture()
            .into_iter()
            .map(|(p, s)| (path(p), s.as_bytes().to_vec()))
            .collect();
        repo.bootstrap(files, intent("seed"), actor("seed")).await?;
        let mut a = begin(&repo, "a").await?;
        edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
        submit(&repo, &mut a, "a").await?
    };
    let reopened = hord_txn::Repo::open(&dir).await?;
    let queue = reopened.queue().await?;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].status, QueueStatus::Queued);
    // Validated at landing: this process did not propose it.
    let done = reopened.land_local().await?;
    assert_eq!(done[0].status, QueueStatus::Landed { landed: ca });
    assert_eq!(reopened.head().await?.change, Some(ca));
    drop(reopened);
    let _ = remove_tree(&dir);
    Ok(())
}

#[tokio::test]
async fn directory_workspaces_find_writes_by_diffing() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = t
        .repo
        .begin_directory(hord_txn::BeginOptions::at_head(actor("dir")))
        .await?;
    let hord_txn::Materialization::Directory { path: dir } = ws.materialization().clone() else {
        return Err("directory".into());
    };
    assert!(!ws.access_log().reads_observed);
    let lib = dir.join("src/lib.rs");
    assert_eq!(std::fs::read_to_string(&lib)?, LIB);
    std::fs::write(&lib, LIB.replace("    4\n", "    44\n"))?;
    std::fs::write(dir.join("notes.txt"), "hi\n")?;
    let delta = def(&mut ws, "src/lib.rs", "delta").await?;
    let proposal = ws.propose(intent("dir edit")).await?;
    assert!(proposal.record.write_set.contains(&delta));
    assert!(
        proposal
            .record
            .write_set
            .contains(&NodeId::file_root(&path("notes.txt")))
    );
    assert!(ws.access_log().written_paths.contains(&path("src/lib.rs")));
    t.repo.submit(proposal.change).await?;
    assert!(landed(&t.repo.land_local().await?[0].status));

    // Reopen by id, as the CLI would.
    let again = t.repo.open_workspace(ws.id(), actor("dir"), None).await?;
    assert_eq!(again.base(), ws.base());
    Ok(())
}

#[tokio::test]
async fn comment_edits_conflict_only_with_edits_of_the_same_text() -> TestResult {
    let t = repo(&fixture()).await?;
    // a and c both put a comment before `mod other;` (leading trivia, so it
    // is part of that definition, ADR 0015 amendment: write sets come from
    // content); b edits another function.
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    let mut c = begin(&t.repo, "c").await?;
    let module = def(&mut a, "src/lib.rs", "other").await?;
    edit(
        &mut a,
        "src/lib.rs",
        LIB,
        "mod other;\n",
        "// A\nmod other;\n",
    )
    .await?;
    edit(&mut b, "src/lib.rs", LIB, "    4\n", "    40\n").await?;
    edit(
        &mut c,
        "src/lib.rs",
        LIB,
        "mod other;\n",
        "// C\nmod other;\n",
    )
    .await?;
    let pa = a.propose(intent("a")).await?;
    assert!(
        pa.record.write_set.contains(&module),
        "{:?}",
        pa.record.write_set
    );
    t.repo.submit(pa.change).await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    let cc = submit(&t.repo, &mut c, "c").await?;
    let done = t.repo.land_local().await?;
    let b_report = t.repo.conflicts(cb).await?;
    assert!(b_report.conflicts.is_empty(), "{b_report:#?}");
    assert!(landed(&done[1].status));
    let c_report = t.repo.conflicts(cc).await?;
    let ww = c_report
        .conflicts
        .iter()
        .find(|x| x.kind == ConflictKind::WriteWrite)
        .expect("same text edited twice");
    assert!(ww.nodes.contains(&module), "{ww:?}");
    Ok(())
}

#[tokio::test]
async fn rebased_records_carry_structural_ops_and_land_validated() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    edit(&mut a, "src/lib.rs", LIB, "    2\n", "    20\n").await?;
    edit(&mut b, "src/lib.rs", LIB, "    4\n", "    40\n").await?;
    submit(&t.repo, &mut a, "a").await?;
    let cb = submit(&t.repo, &mut b, "b").await?;
    t.repo.land_local().await?;
    let QueueStatus::Landed { landed: lb } = t.repo.status(cb).await?.status else {
        return Err("b did not land".into());
    };
    assert_ne!(lb, cb, "rebased");
    let rebased = t.repo.change(lb).await?;
    assert!(!rebased.ops.is_empty());
    assert!(!rebased.ops.iter().any(|op| matches!(op, Op::Blob { .. })));
    // Validated at landing: a reopened lander would accept it too.
    let head = t.repo.head().await?;
    assert_eq!(head.change, Some(lb));
    assert_eq!(head.snapshot, rebased.result);
    Ok(())
}

/// ADR 0013, spec §12 M3: concurrent dependency additions to a real
/// `Cargo.lock` both land through the lockfile merge.
#[tokio::test]
async fn concurrent_cargo_lock_dependency_additions_both_land() -> TestResult {
    let (lock, a_lock, b_lock) = lock_additions();
    const STORE_DEPS: &str = LOCK_STORE_DEPS;
    assert_ne!(a_lock, lock);
    assert_ne!(b_lock, lock);

    let mut files = fixture();
    files.push(("Cargo.lock", &lock));
    // Write-write on `dependencies`, resolved at rung 1 by the lockfile
    // merge. It lands under the default verifier (ADR 0013 amendment).
    let t = repo(&files).await?;
    let mut a = begin(&t.repo, "a").await?;
    let mut b = begin(&t.repo, "b").await?;
    a.write_file(&path("Cargo.lock"), a_lock.clone()).await?;
    b.write_file(&path("Cargo.lock"), b_lock.clone()).await?;
    let pa = a.propose(intent("add zz-alpha")).await?;
    let pb = b.propose(intent("add aaa-beta")).await?;
    // Parsed under CargoLockAdapter: structural ops, not a Blob.
    for p in [&pa, &pb] {
        assert!(
            !p.record.ops.iter().any(|op| matches!(op, Op::Blob { .. })),
            "{:?}",
            p.record.ops
        );
    }
    t.repo.submit(pa.change).await?;
    t.repo.submit(pb.change).await?;
    let done = t.repo.land_local().await?;
    assert!(done.iter().all(|e| landed(&e.status)), "{done:#?}");

    // Both edit hord-store's dependency list, so the set check sees a
    // write-write overlap, which the lockfile merge resolves.
    let report = t.repo.conflicts(pb.change).await?;
    assert!(report.has(ConflictKind::WriteWrite), "{report:#?}");
    assert!(!report.has_hard(), "{report:#?}");
    assert_eq!(
        report
            .adapter_merged
            .iter()
            .map(|m| m.path.clone())
            .collect::<Vec<_>>(),
        vec![path("Cargo.lock")],
        "{report:#?}"
    );
    assert!(report.only_adapter_merged() && report.verification.is_none());

    let merged = head_file(&t.repo, "Cargo.lock").await?;
    assert!(merged.contains("name = \"zz-alpha\"") && merged.contains("name = \"aaa-beta\""));
    let store_deps = &merged[merged
        .find(STORE_DEPS)
        .ok_or("hord-store dependencies in merged Cargo.lock")?..];
    let store_deps = &store_deps[..store_deps
        .find(']')
        .ok_or("end of hord-store dependencies")?];
    assert!(
        store_deps.contains("\"aaa-beta\"") && store_deps.contains("\"zz-alpha\""),
        "{store_deps}"
    );
    Ok(())
}
