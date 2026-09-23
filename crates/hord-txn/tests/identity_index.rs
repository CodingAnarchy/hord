//! NodeIds are a function of stored objects (review finding: a carried id
//! must not depend on a redb row alone). The same snapshot in a store that
//! has its objects but no identity pointer is an error, never a fresh
//! assignment, and `rebuild_index` restores the pointer from the binding
//! object, giving the carried ids back.

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

    // A store with every object but no pointers and no log: the snapshot's
    // ids are unknown, which is an error rather than a fresh assignment.
    let copy = temp_dir("identity-copy");
    drop(Repo::create(&copy).await.unwrap());
    copy_dir(
        &t.path.join(".hord").join("objects"),
        &copy.join(".hord").join("objects"),
    );
    let other = Repo::open(&copy).await.unwrap();
    match ids(&other, snapshot).await {
        Err(Error::MissingIdentity(s)) => assert_eq!(s, snapshot),
        found => panic!("expected MissingIdentity, got {found:?}"),
    }

    // The binding object names (snapshot, index): a rebuild restores the
    // pointer, and the carried ids come back.
    other.store().rebuild_index().unwrap();
    assert_eq!(ids(&other, snapshot).await.unwrap(), carried);
    drop(other);
    let _ = fs::remove_dir_all(&copy);
}
