//! Public API tests for `hord-store`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{Blob, ObjectId};
use hord_store::{Error, Store, WorkspaceId};

fn temp_repo() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-api-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn store() -> (Guard, Store) {
    let path = temp_repo();
    let store = Store::create(&path).unwrap();
    (Guard(path), store)
}

#[test]
fn create_open_round_trip() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let created = Store::create(&path).unwrap();
    assert!(created.hord_dir().ends_with(".hord"));
    drop(created);
    let opened = Store::open(&path).unwrap();
    assert_eq!(opened.repo_root(), path.as_path());
}

#[test]
fn create_twice_fails() {
    let (g, _s) = store();
    let err = Store::create(&g.0).unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)));
}

#[test]
fn open_missing_fails() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let err = Store::open(&path).unwrap_err();
    assert!(matches!(err, Error::MissingStore(_)));
}

#[test]
fn put_get_contains() {
    let (_g, store) = store();
    let bytes = b"\xa0"; // empty CBOR map, but any bytes are hashed as-is
    let id = store.put(bytes).unwrap();
    assert_eq!(id, ObjectId::from_canonical(bytes));
    assert!(store.contains(id).unwrap());
    assert_eq!(store.get(id).unwrap(), bytes);
    let missing = ObjectId::from_canonical(b"nope");
    assert!(!store.contains(missing).unwrap());
    assert!(matches!(store.get(missing), Err(Error::MissingObject(_))));
}

#[test]
fn put_is_idempotent() {
    let (_g, store) = store();
    let bytes = b"hello";
    let a = store.put(bytes).unwrap();
    let b = store.put(bytes).unwrap();
    assert_eq!(a, b);
}

#[test]
fn put_object_matches_object_id_of() {
    let (_g, store) = store();
    let blob = Blob::new(b"deadbeef".to_vec());
    let id = store.put_object(&blob).unwrap();
    assert_eq!(id, ObjectId::of(&blob).unwrap());
    let got: Blob = store.get_object(id).unwrap();
    assert_eq!(got, blob);
}

#[test]
fn packed_objects_are_readable() {
    let (_g, store) = store();
    let mut ids = Vec::new();
    for i in 0..8u8 {
        ids.push(store.put(&[i]).unwrap());
    }
    let packed = store.pack().unwrap();
    assert_eq!(packed, 8);
    for (i, id) in ids.iter().enumerate() {
        assert!(store.contains(*id).unwrap());
        assert_eq!(store.get(*id).unwrap(), [i as u8]);
    }
    // Loose files should be gone; a second pack is a no-op.
    assert_eq!(store.pack().unwrap(), 0);
}

#[test]
fn pack_then_open() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let id;
    {
        let store = Store::create(&path).unwrap();
        id = store.put(b"packed-across-open").unwrap();
        store.pack().unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.get(id).unwrap(), b"packed-across-open");
}

#[test]
fn log_head_and_refs() {
    let (_g, store) = store();
    assert!(store.head().unwrap().is_none());
    assert!(store.log().unwrap().is_empty());
    assert!(store.get_ref("main").unwrap().is_none());

    let a = store.put(b"change-a").unwrap();
    let b = store.put(b"change-b").unwrap();
    store.append_log(a).unwrap();
    store.append_log(b).unwrap();
    store.set_head(b).unwrap();
    store.set_ref("main", b).unwrap();
    store.set_ref("release/1.2", a).unwrap();

    assert_eq!(store.log().unwrap(), vec![a, b]);
    assert_eq!(store.head().unwrap(), Some(b));
    assert_eq!(store.get_ref("main").unwrap(), Some(b));
    assert_eq!(store.get_ref("release/1.2").unwrap(), Some(a));
}

#[test]
fn log_read_then_append_then_reopen() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let a;
    let b;
    {
        let store = Store::create(&path).unwrap();
        assert!(store.log().unwrap().is_empty());
        a = store.put(b"log-a").unwrap();
        b = store.put(b"log-b").unwrap();
        store.append_log(a).unwrap();
        assert_eq!(store.log().unwrap(), vec![a]);
        store.append_log(b).unwrap();
        store.append_log(a).unwrap();
        assert_eq!(store.log().unwrap(), vec![a, b, a]);
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.log().unwrap(), vec![a, b, a]);
}

#[test]
fn duplicate_put_after_pack_stays_readable() {
    let (_g, store) = store();
    let bytes = b"packed-then-put-again";
    let id = store.put(bytes).unwrap();
    assert_eq!(store.pack().unwrap(), 1);
    assert_eq!(store.put(bytes).unwrap(), id);
    assert_eq!(store.get(id).unwrap(), bytes);
    assert!(store.contains(id).unwrap());
}

#[test]
fn invalid_ref_name_is_rejected() {
    let (_g, store) = store();
    let id = store.put(b"x").unwrap();
    assert!(matches!(store.set_ref("", id), Err(Error::InvalidRef(_))));
    assert!(matches!(
        store.set_ref("a\0b", id),
        Err(Error::InvalidRef(_))
    ));
}

#[test]
fn workspaces_create_and_list() {
    let (_g, store) = store();
    let base = store.put_object(&Blob::new(b"tree".to_vec())).unwrap();
    let ws1 = store.create_workspace(base).unwrap();
    let ws2 = store.create_workspace(base).unwrap();
    assert_ne!(ws1.id, ws2.id);
    assert!(ws1.path.ends_with(ws1.id.to_string()));
    assert!(ws1.path.is_dir());
    assert!(ws2.path.is_dir());
    assert_eq!(ws1.base, base);

    let listed = store.list_workspaces().unwrap();
    assert_eq!(listed.len(), 2);
    let ids: Vec<WorkspaceId> = listed.iter().map(|w| w.id).collect();
    assert!(ids.contains(&ws1.id));
    assert!(ids.contains(&ws2.id));
    assert_eq!(store.get_workspace(ws1.id).unwrap().unwrap().base, base);
    let missing: WorkspaceId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    assert!(store.get_workspace(missing).unwrap().is_none());
}

#[test]
fn workspace_id_parse_round_trip() {
    let id = WorkspaceId::generate();
    let parsed: WorkspaceId = id.to_string().parse().unwrap();
    assert_eq!(id, parsed);
    assert!("not-a-ulid".parse::<WorkspaceId>().is_err());
}

#[test]
fn store_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
}
