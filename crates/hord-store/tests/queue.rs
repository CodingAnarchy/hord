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
