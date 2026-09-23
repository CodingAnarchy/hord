//! Lander queue and identity-index tables used by `hord-txn`.

use std::fs;
use std::path::PathBuf;

use hord_core::ObjectId;
use hord_store::Store;

fn temp_repo() -> PathBuf {
    let path = std::env::temp_dir().join(format!("hord-store-queue-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn queue_and_identity_index_round_trip_and_persist() {
    let path = temp_repo();
    {
        let store = Store::create(&path).unwrap();
        assert!(store.queue_entries().unwrap().is_empty());
        assert_eq!(store.queue_push(b"first").unwrap(), 0);
        assert_eq!(store.queue_push(b"second").unwrap(), 1);
        store.queue_set(0, b"first, updated").unwrap();
        assert!(store.queue_set(7, b"missing").is_err());
        let snapshot = ObjectId::from_bytes([1; 32]);
        let index = ObjectId::from_bytes([2; 32]);
        assert_eq!(store.identity_index(snapshot).unwrap(), None);
        store.set_identity_index(snapshot, index).unwrap();
        assert_eq!(store.identity_index(snapshot).unwrap(), Some(index));
        // Make the non-fsynced writes durable, as the lander's set_head does.
        store.set_head(ObjectId::from_bytes([3; 32])).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.queue_entries().unwrap(),
        vec![(0, b"first, updated".to_vec()), (1, b"second".to_vec())]
    );
    assert_eq!(store.queue_entry(1).unwrap(), Some(b"second".to_vec()));
    assert_eq!(store.queue_entry(2).unwrap(), None);
    assert_eq!(
        store.identity_index(ObjectId::from_bytes([1; 32])).unwrap(),
        Some(ObjectId::from_bytes([2; 32]))
    );
    assert_eq!(store.queue_push(b"third").unwrap(), 2);
    drop(store);
    let _ = fs::remove_dir_all(&path);
}

#[test]
fn identity_index_pointers_are_rebuilt_from_binding_objects() {
    let path = temp_repo().with_extension("rebuild");
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    let store = Store::create(&path).unwrap();
    let a = ObjectId::from_bytes([1; 32]);
    let b = ObjectId::from_bytes([4; 32]);
    let first = ObjectId::from_bytes([2; 32]);
    let second = ObjectId::from_bytes([3; 32]);
    let other = ObjectId::from_bytes([5; 32]);
    // `a` is repointed; the later binding supersedes the earlier one.
    store.set_identity_index(a, first).unwrap();
    store.set_identity_index(a, second).unwrap();
    store.set_identity_index(b, other).unwrap();
    drop(store);

    // A new index over the same objects (the redb rows are lost).
    let copy = path.with_extension("copy");
    let _ = fs::remove_dir_all(&copy);
    fs::create_dir_all(&copy).unwrap();
    drop(Store::create(&copy).unwrap());
    copy_dir(
        &path.join(".hord").join("objects"),
        &copy.join(".hord").join("objects"),
    );
    let _ = fs::remove_dir_all(&path);
    let path = copy;
    let store = Store::open(&path).unwrap();
    assert_eq!(store.identity_index(a).unwrap(), None);
    store.rebuild_index().unwrap();
    assert_eq!(store.identity_index(a).unwrap(), Some(second));
    assert_eq!(store.identity_index(b).unwrap(), Some(other));

    // A rebuilt row keeps its binding, so the next repoint chains from it.
    store.set_identity_index(a, first).unwrap();
    store.rebuild_index().unwrap();
    assert_eq!(store.identity_index(a).unwrap(), Some(first));
    drop(store);
    let _ = fs::remove_dir_all(&path);
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
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
