//! Property tests: `get(put(bytes)) == bytes` and ids are BLAKE3 of the bytes.

mod common;

use std::fs;

use hord_core::ObjectId;
use hord_store::Store;
use proptest::prelude::*;

use common::temp_repo;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn put_get_bytes(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let path = temp_repo()?;
        let store = Store::create(&path)?;
        let id = store.put(&bytes)?;
        prop_assert_eq!(id, ObjectId::from_canonical(&bytes));
        prop_assert_eq!(store.get(id)?, bytes);
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn put_pack_get(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let path = temp_repo()?;
        let store = Store::create(&path)?;
        let id = store.put(&bytes)?;
        store.pack()?;
        prop_assert_eq!(store.get(id)?, bytes);
        let _ = fs::remove_dir_all(&path);
    }
}
