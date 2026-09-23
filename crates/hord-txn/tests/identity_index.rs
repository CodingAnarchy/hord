//! NodeIds are a function of the snapshot's objects (ADR 0017; system review
//! item 1: a carried id must not depend on a redb row). A store holding only
//! the snapshot's objects, with no log and no index rows, gives the same
//! carried ids. A snapshot whose identity objects are missing is an error,
//! never a fresh assignment.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use hord_core::{NodeId, SnapshotId};
use hord_txn::{Error, Repo};

const SRC: &str = "\
pub fn compute_total(a: u32, b: u32) -> u32 {
    let x = a * 2;
    let y = b * 3;
    x + y + 17
}

pub fn other() -> u32 {
    1
}
";

async fn ids(repo: &Repo, snapshot: SnapshotId) -> Result<Vec<(String, NodeId)>, Error> {
    Ok(repo
        .definitions_in(snapshot, path("src/a.rs"))
        .await?
        .into_iter()
        .map(|d| {
            (
                d.name.map(|n| n.as_str().to_owned()).unwrap_or_default(),
                d.node,
            )
        })
        .collect())
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
async fn carried_ids_survive_losing_the_pointer_and_never_fall_back_to_fresh() {
    let t = repo(&[("src/a.rs", SRC)]).await;
    let before = ids(&t.repo, t.repo.head().await.unwrap().snapshot)
        .await
        .unwrap();
    let mut ws = begin(&t.repo, "renamer").await;
    ws.write_file(
        &path("src/a.rs"),
        SRC.replace("compute_total", "sum_weighted"),
    )
    .await
    .unwrap();
    submit(&t.repo, &mut ws, "rename compute_total").await;
    t.repo.land_local().await.unwrap();
    let snapshot = t.repo.head().await.unwrap().snapshot;
    let carried = ids(&t.repo, snapshot).await.unwrap();
    assert_eq!(carried[0].0, "sum_weighted");
    // The rename carried the id (spec §3.4 rule 4).
    assert_eq!(carried[0].1, before[0].1);

    // A store with every object but no log and no index rows (what a
    // remote client has after fetching the snapshot's objects): the carried
    // ids come from the objects alone. Before ADR 0017 this was
    // MissingIdentity until `rebuild_index` restored a redb pointer.
    let copy = temp_dir("identity-copy");
    drop(Repo::create(&copy).await.unwrap());
    copy_dir(
        &t.path.join(".hord").join("objects"),
        &copy.join(".hord").join("objects"),
    );
    let other = Repo::open(&copy).await.unwrap();
    assert!(other.store().log().unwrap().is_empty());
    assert_eq!(ids(&other, snapshot).await.unwrap(), carried);

    // The carried ids are recorded in the snapshot's identity tree: without
    // them the snapshot's ids are unknown, which is an error rather than a
    // fresh assignment.
    let file = file_identity(t.repo.store(), snapshot, "src/a.rs").expect("carried ids stored");
    drop(other);
    remove_loose_object(&copy, file);
    let other = Repo::open(&copy).await.unwrap();
    match ids(&other, snapshot).await {
        Err(Error::Store(hord_store::Error::MissingObject(id))) => assert_eq!(id, file),
        found => panic!("expected the missing identity object, got {found:?}"),
    }
    drop(other);
    let _ = fs::remove_dir_all(&copy);
}

/// A snapshot whose identity tree is gone is `MissingIdentity`.
#[tokio::test]
async fn a_missing_identity_tree_is_an_error() {
    let t = repo(&[("src/a.rs", SRC)]).await;
    let mut ws = begin(&t.repo, "renamer").await;
    ws.write_file(
        &path("src/a.rs"),
        SRC.replace("compute_total", "sum_weighted"),
    )
    .await
    .unwrap();
    submit(&t.repo, &mut ws, "rename compute_total").await;
    t.repo.land_local().await.unwrap();
    let snapshot = t.repo.head().await.unwrap().snapshot;
    let object: hord_core::Snapshot = t.repo.store().get_object(snapshot).unwrap();
    let identity = object.identity().unwrap();

    let copy = temp_dir("identity-tree");
    drop(Repo::create(&copy).await.unwrap());
    copy_dir(
        &t.path.join(".hord").join("objects"),
        &copy.join(".hord").join("objects"),
    );
    remove_loose_object(&copy, identity);
    let other = Repo::open(&copy).await.unwrap();
    match ids(&other, snapshot).await {
        Err(Error::MissingIdentity(s)) => assert_eq!(s, snapshot),
        found => panic!("expected MissingIdentity, got {found:?}"),
    }
    drop(other);
    let _ = fs::remove_dir_all(&copy);
}
