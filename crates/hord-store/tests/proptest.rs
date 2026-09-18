//! Property tests: `get(put(bytes)) == bytes` and ids are BLAKE3 of the bytes.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::ObjectId;
use hord_store::Store;
use proptest::prelude::*;

fn temp_repo() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-prop-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn put_get_bytes(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let path = temp_repo();
        let store = Store::create(&path).unwrap();
        let id = store.put(&bytes).unwrap();
        prop_assert_eq!(id, ObjectId::from_canonical(&bytes));
        prop_assert_eq!(store.get(id).unwrap(), bytes);
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn put_pack_get(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let path = temp_repo();
        let store = Store::create(&path).unwrap();
        let id = store.put(&bytes).unwrap();
        store.pack().unwrap();
        prop_assert_eq!(store.get(id).unwrap(), bytes);
        let _ = fs::remove_dir_all(&path);
    }
}
