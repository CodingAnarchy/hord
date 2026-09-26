//! Lander queue tables used by `hord-txn`, and the store format version.

mod common;

use std::fs;

use hord_core::ObjectId;
use hord_store::Store;

use common::temp_repo;

#[test]
fn queue_round_trips_and_persists() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    {
        let store = Store::create(&path)?;
        assert!(store.queue_entries()?.is_empty());
        assert_eq!(store.queue_push(b"first", &[], &[])?, 0);
        assert_eq!(store.queue_push(b"second", &[], &[])?, 1);
        store.queue_set(0, b"first, updated")?;
        assert!(store.queue_set(7, b"missing").is_err());
        // Make the non-fsynced writes durable, as the lander's set_head does.
        store.set_head(ObjectId::from_bytes([3; 32]))?;
    }
    let store = Store::open(&path)?;
    assert_eq!(
        store.queue_entries()?,
        vec![(0, b"first, updated".to_vec()), (1, b"second".to_vec())]
    );
    assert_eq!(store.queue_entry(1)?, Some(b"second".to_vec()));
    assert_eq!(store.queue_entry(2)?, None);
    assert_eq!(store.queue_push(b"third", &[], &[])?, 2);
    drop(store);
    let _ = fs::remove_dir_all(&path);
    Ok(())
}

/// ADR 0017 changed what a snapshot id is, so a store written before
/// format versions existed (or by another format) is refused with a clear
/// error instead of being read as if its snapshot ids were `Snapshot`s.
#[test]
fn a_store_of_another_format_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?.with_extension("format");
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path)?;
    drop(Store::create(&path)?);
    drop(Store::open(&path)?);

    // Rewrite the recorded format as an older build would have left it:
    // no `format` key at all.
    let index = path.join(".hord").join("index.redb");
    {
        let db = redb::Database::open(&index)?;
        let txn = db.begin_write()?;
        {
            let mut meta = txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))?;
            meta.remove("format")?;
        }
        txn.commit()?;
    }
    let err = Store::open(&path).expect_err("opening a store of another format fails");
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
    Ok(())
}
