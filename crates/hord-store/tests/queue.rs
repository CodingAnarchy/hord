//! Lander queue tables used by `hord-txn`, and the store format version.

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
fn queue_round_trips_and_persists() {
    let path = temp_repo();
    {
        let store = Store::create(&path).unwrap();
        assert!(store.queue_entries().unwrap().is_empty());
        assert_eq!(store.queue_push(b"first", &[], &[]).unwrap(), 0);
        assert_eq!(store.queue_push(b"second", &[], &[]).unwrap(), 1);
        store.queue_set(0, b"first, updated").unwrap();
        assert!(store.queue_set(7, b"missing").is_err());
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
    assert_eq!(store.queue_push(b"third", &[], &[]).unwrap(), 2);
    drop(store);
    let _ = fs::remove_dir_all(&path);
}

/// ADR 0017 changed what a snapshot id is, so a store written before
/// format versions existed (or by another format) is refused with a clear
/// error instead of being read as if its snapshot ids were `Snapshot`s.
#[test]
fn a_store_of_another_format_is_refused() {
    let path = temp_repo().with_extension("format");
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    drop(Store::create(&path).unwrap());
    drop(Store::open(&path).unwrap());

    // Rewrite the recorded format as an older build would have left it:
    // no `format` key at all.
    let index = path.join(".hord").join("index.redb");
    {
        let db = redb::Database::open(&index).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut meta = txn
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
                .unwrap();
            meta.remove("format").unwrap();
        }
        txn.commit().unwrap();
    }
    let err = Store::open(&path).unwrap_err();
    assert!(
        matches!(
            err,
            hord_store::Error::StoreFormat {
                found: None,
                expected: 2
            }
        ),
        "{err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("format 1") && text.contains("re-create"),
        "{text}"
    );
    let _ = fs::remove_dir_all(&path);
}
