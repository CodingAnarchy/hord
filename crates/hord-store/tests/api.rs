//! Public API tests for `hord-store`.

mod common;

use std::fs;

use hord_core::{Blob, ObjectId};
use hord_store::{Error, Store, WorkspaceId};

use common::{Guard, store, temp_repo};

#[test]
fn create_open_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let created = Store::create(&path)?;
    assert!(created.hord_dir().ends_with(".hord"));
    drop(created);
    let opened = Store::open(&path)?;
    assert_eq!(opened.repo_root(), path.as_path());
    Ok(())
}

#[test]
fn create_twice_fails() -> Result<(), Box<dyn std::error::Error>> {
    let (g, _s) = store()?;
    let err = Store::create(&g.0).expect_err("creating a store twice fails");
    assert!(matches!(err, Error::AlreadyExists(_)));
    Ok(())
}

#[test]
fn open_missing_fails() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let err = Store::open(&path).expect_err("opening a missing store fails");
    assert!(matches!(err, Error::MissingStore(_)));
    Ok(())
}

#[test]
fn put_get_contains() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let bytes = b"\xa0"; // empty CBOR map, but any bytes are hashed as-is
    let id = store.put(bytes)?;
    assert_eq!(id, ObjectId::from_canonical(bytes));
    assert!(store.contains(id)?);
    assert_eq!(store.get(id)?, bytes);
    let missing = ObjectId::from_canonical(b"nope");
    assert!(!store.contains(missing)?);
    assert!(matches!(store.get(missing), Err(Error::MissingObject(_))));
    Ok(())
}

#[test]
fn put_is_idempotent() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let bytes = b"hello";
    let a = store.put(bytes)?;
    let b = store.put(bytes)?;
    assert_eq!(a, b);
    Ok(())
}

#[test]
fn put_object_matches_object_id_of() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let blob = Blob::new(b"deadbeef".to_vec());
    let id = store.put_object(&blob)?;
    assert_eq!(id, ObjectId::of(&blob)?);
    let got: Blob = store.get_object(id)?;
    assert_eq!(got, blob);
    Ok(())
}

#[test]
fn packed_objects_are_readable() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let mut ids = Vec::new();
    for i in 0..8u8 {
        ids.push(store.put(&[i])?);
    }
    let packed = store.pack()?;
    assert_eq!(packed, 8);
    for (i, id) in ids.iter().enumerate() {
        assert!(store.contains(*id)?);
        assert_eq!(store.get(*id)?, [i as u8]);
    }
    // Loose files should be gone; a second pack is a no-op.
    assert_eq!(store.pack()?, 0);
    Ok(())
}

#[test]
fn pack_then_open() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let id;
    {
        let store = Store::create(&path)?;
        id = store.put(b"packed-across-open")?;
        store.pack()?;
    }
    let store = Store::open(&path)?;
    assert_eq!(store.get(id)?, b"packed-across-open");
    Ok(())
}

#[test]
fn log_head_and_refs() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    assert!(store.head()?.is_none());
    assert!(store.log()?.is_empty());
    assert!(store.get_ref("main")?.is_none());

    let a = store.put(b"change-a")?;
    let b = store.put(b"change-b")?;
    store.append_log(a)?;
    store.append_log(b)?;
    store.set_head(b)?;
    store.set_ref("main", b)?;
    store.set_ref("release/1.2", a)?;

    assert_eq!(store.log()?, vec![a, b]);
    assert_eq!(store.head()?, Some(b));
    assert_eq!(store.get_ref("main")?, Some(b));
    assert_eq!(store.get_ref("release/1.2")?, Some(a));
    Ok(())
}

#[test]
fn log_read_then_append_then_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let a;
    let b;
    {
        let store = Store::create(&path)?;
        assert!(store.log()?.is_empty());
        a = store.put(b"log-a")?;
        b = store.put(b"log-b")?;
        store.append_log(a)?;
        assert_eq!(store.log()?, vec![a]);
        store.append_log(b)?;
        store.append_log(a)?;
        assert_eq!(store.log()?, vec![a, b, a]);
    }
    let store = Store::open(&path)?;
    assert_eq!(store.log()?, vec![a, b, a]);
    Ok(())
}

#[test]
fn duplicate_put_after_pack_stays_readable() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let bytes = b"packed-then-put-again";
    let id = store.put(bytes)?;
    assert_eq!(store.pack()?, 1);
    assert_eq!(store.put(bytes)?, id);
    assert_eq!(store.get(id)?, bytes);
    assert!(store.contains(id)?);
    Ok(())
}

#[test]
fn pack_refuses_a_loose_object_that_fails_its_hash() -> Result<(), Box<dyn std::error::Error>> {
    let (guard, store) = store()?;
    let id = store.put(b"\x41\x61")?;
    let hex = id.to_hex();
    let loose = guard
        .0
        .join(".hord/objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::write(&loose, b"torn")?;
    let err = store.pack().expect_err("packing a torn loose object fails");
    assert!(
        matches!(err, Error::Corrupt { id: got, .. } if got == id),
        "{err}"
    );
    // Nothing was packed, and the loose file is still there to be re-put.
    assert!(loose.is_file());
    drop(store);
    let reopened = Store::open(&guard.0)?;
    reopened.put(b"\x41\x61")?;
    assert_eq!(reopened.pack()?, 1);
    assert_eq!(reopened.get(id)?, b"\x41\x61");
    Ok(())
}

#[test]
fn invalid_ref_name_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let id = store.put(b"x")?;
    assert!(matches!(store.set_ref("", id), Err(Error::InvalidRef(_))));
    assert!(matches!(
        store.set_ref("a\0b", id),
        Err(Error::InvalidRef(_))
    ));
    Ok(())
}

#[test]
fn workspaces_create_and_list() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let base = store.put_object(&Blob::new(b"tree".to_vec()))?;
    let ws1 = store.create_workspace(base)?;
    let ws2 = store.create_workspace(base)?;
    assert_ne!(ws1.id, ws2.id);
    assert!(ws1.path.ends_with(ws1.id.to_string()));
    assert!(ws1.path.is_dir());
    assert!(ws2.path.is_dir());
    assert_eq!(ws1.base, base);

    let listed = store.list_workspaces()?;
    assert_eq!(listed.len(), 2);
    let ids: Vec<WorkspaceId> = listed.iter().map(|w| w.id).collect();
    assert!(ids.contains(&ws1.id));
    assert!(ids.contains(&ws2.id));
    assert_eq!(
        store
            .get_workspace(ws1.id)?
            .ok_or("workspace ws1 is listed")?
            .base,
        base
    );
    let missing: WorkspaceId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse()?;
    assert!(store.get_workspace(missing)?.is_none());
    Ok(())
}

#[test]
fn workspace_id_parse_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let id = WorkspaceId::generate();
    let parsed: WorkspaceId = id.to_string().parse()?;
    assert_eq!(id, parsed);
    assert!("not-a-ulid".parse::<WorkspaceId>().is_err());
    Ok(())
}

#[test]
fn store_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
}
