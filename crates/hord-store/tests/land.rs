//! One-commit landings, the queue name index, and log slices (M3 perf
//! review #6 and #7, `docs/review/perf.md`).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{Actor, ChangeRecord, Intent, NodeId, ObjectId, Provenance, SnapshotId, Timestamp};
use hord_store::{Landing, Store};

fn temp_repo(tag: &str) -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-land-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn oid(n: u8) -> ObjectId {
    ObjectId::from_bytes([n; 32])
}

fn record(base: SnapshotId, result: SnapshotId, write: NodeId) -> ChangeRecord {
    ChangeRecord {
        base,
        result,
        parents: Vec::new(),
        ops: Vec::new(),
        intent: Intent::from_summary("s"),
        provenance: Provenance {
            actor: Actor::Human {
                id: "tester".into(),
            },
            toolchain: oid(9),
            created_at: Timestamp::from_millis(0),
            session: None,
            parent_intent: None,
            voucher: None,
        },
        read_set: Default::default(),
        write_set: [write].into_iter().collect(),
        identity_deltas: Vec::new(),
        evidence: Vec::new(),
        signature: None,
        rebased_from: None,
    }
}

/// Store a record, queue it, and land it under `landed` (a second record
/// when the lander rebased it). Returns `(submitted, landed)`.
fn queue_and_land(
    store: &Store,
    rebased: bool,
) -> Result<(ObjectId, ObjectId, NodeId), hord_store::Error> {
    let node = NodeId::from_u128(42);
    let submitted = store.put_object(&record(oid(1), oid(2), node))?;
    let seq = store.queue_push(b"queued", &[submitted], &[])?;
    let (landed, landed_record) = if rebased {
        let r = ChangeRecord {
            rebased_from: Some(submitted),
            ..record(oid(3), oid(4), node)
        };
        (store.put_object(&r)?, r)
    } else {
        (submitted, record(oid(1), oid(2), node))
    };
    let names = if rebased { vec![landed] } else { Vec::new() };
    store.land(&Landing {
        change: landed,
        record: &landed_record,
        entry: (seq, b"landed"),
        names: &names,
    })?;
    Ok((submitted, landed, node))
}

fn assert_landed(
    store: &Store,
    submitted: ObjectId,
    landed: ObjectId,
    node: NodeId,
) -> Result<(), hord_store::Error> {
    assert_eq!(store.queue_entry(0)?, Some(b"landed".to_vec()));
    assert_eq!(store.log()?, vec![landed]);
    assert_eq!(store.head()?, Some(landed));
    // ADR 0018: the submitted id resolves to the landed one.
    assert_eq!(store.rebased_to(submitted)?, Some(landed));
    assert_eq!(store.node_history(node)?, vec![landed]);
    assert_eq!(store.queue_named(submitted)?, vec![0]);
    assert_eq!(store.queue_named(landed)?, vec![0]);
    Ok(())
}

/// The whole landing is in the index after `land` returns, with nothing
/// else flushing it: a process killed right after it loses none of it.
/// Before, the queue status was a separate non-durable commit and
/// `node_history` a separate durable one after `head`.
#[test]
fn a_landing_survives_an_abort_right_after_land() -> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "HORD_STORE_LAND_ABORT_DIR";
    if let Ok(dir) = std::env::var(CHILD) {
        let store = Store::open(&dir)?;
        queue_and_land(&store, true)?;
        // No drop, no flush: the process dies with the store open.
        std::process::abort();
    }
    let dir = temp_repo("abort")?;
    drop(Store::create(&dir)?);
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "a_landing_survives_an_abort_right_after_land",
            "--nocapture",
        ])
        .env(CHILD, &dir)
        .status()?;
    assert!(!status.success(), "the child must abort");
    let store = Store::open(&dir)?;
    let log = store.log()?;
    assert_eq!(log.len(), 1, "the landing reached the log");
    let landed = log[0];
    // Content-addressed: storing the record again gives its id.
    let submitted = store.put_object(&record(oid(1), oid(2), NodeId::from_u128(42)))?;
    assert_landed(&store, submitted, landed, NodeId::from_u128(42))?;
    drop(store);
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn land_writes_the_entry_log_head_and_history_together() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_repo("together")?;
    let store = Store::create(&dir)?;
    // A buffered log entry from before (git import) lands first, in order.
    let earlier = oid(5);
    store.append_log(earlier)?;
    let (submitted, landed, node) = {
        let node = NodeId::from_u128(42);
        let submitted = store.put_object(&record(oid(1), oid(2), node))?;
        let seq = store.queue_push(b"queued", &[submitted], &[])?;
        store.land(&Landing {
            change: submitted,
            record: &record(oid(1), oid(2), node),
            entry: (seq, b"landed"),
            names: &[],
        })?;
        (submitted, submitted, node)
    };
    assert_eq!(store.log()?, vec![earlier, landed]);
    assert_eq!(store.log_position(landed)?, Some(1));
    assert_eq!(store.log_since(1)?, vec![landed]);
    assert!(store.log_contains(earlier)?);
    assert_eq!(store.node_history(node)?, vec![landed]);
    assert_eq!(store.queue_named(submitted)?, vec![0]);
    // Indexing it again, as `hord index` would, changes nothing.
    store.index_change(landed)?;
    assert_eq!(store.node_history(node)?, vec![landed]);
    drop(store);
    let store = Store::open(&dir)?;
    assert_eq!(store.log()?, vec![earlier, landed]);
    assert_eq!(store.head()?, Some(landed));
    assert_eq!(store.rebased_to(submitted)?, None, "not rebased");
    drop(store);
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn a_rebased_landing_is_found_by_both_ids() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_repo("rebased")?;
    let store = Store::create(&dir)?;
    let (submitted, landed, node) = queue_and_land(&store, true)?;
    assert_ne!(submitted, landed);
    assert_landed(&store, submitted, landed, node)?;
    // The `rebased` row is derived from the landed record (ADR 0018).
    store.rebuild_index()?;
    assert_eq!(store.rebased_to(submitted)?, Some(landed));
    drop(store);
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn queue_names_are_sorted_sets_and_survive_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_repo("names")?;
    let store = Store::create(&dir)?;
    assert!(store.queue_names_indexed()?);
    let a = oid(1);
    let b = oid(2);
    assert_eq!(store.queue_push(b"0", &[a], &[])?, 0);
    assert_eq!(store.queue_push(b"1", &[b], &[])?, 1);
    assert_eq!(store.queue_push(b"2", &[a, a], &[])?, 2);
    assert_eq!(store.queue_named(a)?, vec![0, 2]);
    assert_eq!(store.queue_named(b)?, vec![1]);
    assert!(store.queue_named(oid(3))?.is_empty());
    // Re-indexing an entry is a no-op.
    store.index_queue_names(&[(0, vec![a, b])])?;
    assert_eq!(store.queue_named(a)?, vec![0, 2]);
    assert_eq!(store.queue_named(b)?, vec![0, 1]);
    drop(store);
    let store = Store::open(&dir)?;
    assert!(store.queue_names_indexed()?);
    assert_eq!(store.queue_named(a)?, vec![0, 2]);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn checked_marks_persist() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_repo("checked")?;
    let store = Store::create(&dir)?;
    assert!(!store.is_checked(oid(1))?);
    store.mark_checked(oid(1))?;
    assert!(store.is_checked(oid(1))?);
    // Durable with the next durable commit, which can carry marks too.
    store.queue_push(b"x", &[oid(3)], &[oid(3)])?;
    assert!(store.is_checked(oid(3))?);
    drop(store);
    let store = Store::open(&dir)?;
    assert!(store.is_checked(oid(1))?);
    assert!(store.is_checked(oid(3))?);
    assert!(!store.is_checked(oid(2))?);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}
