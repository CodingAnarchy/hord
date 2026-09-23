//! One-commit landings, the queue name index, and log slices (M3 perf
//! review #6 and #7, `docs/review/perf.md`).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{Actor, ChangeRecord, Intent, NodeId, ObjectId, Provenance, SnapshotId, Timestamp};
use hord_store::{Landing, Store};

fn temp_repo(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-land-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
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
        intent: Intent {
            summary: "s".into(),
            body: String::new(),
            refs: Vec::new(),
            acceptance: Vec::new(),
        },
        provenance: Provenance {
            actor: Actor::Human {
                id: "tester".into(),
            },
            toolchain: oid(9),
            created_at: Timestamp::from_millis(0),
            session: None,
            parent_intent: None,
        },
        read_set: Default::default(),
        write_set: [write].into_iter().collect(),
        identity_deltas: Vec::new(),
        evidence: Vec::new(),
        signature: None,
    }
}

/// Store a record, queue it, and land it under `landed` (a second record
/// when the lander rebased it). Returns `(submitted, landed)`.
fn queue_and_land(store: &Store, rebased: bool) -> (ObjectId, ObjectId, NodeId) {
    let node = NodeId::from_u128(42);
    let submitted = store.put_object(&record(oid(1), oid(2), node)).unwrap();
    let seq = store.queue_push(b"queued", &[submitted], &[]).unwrap();
    let (landed, landed_record) = if rebased {
        let r = record(oid(3), oid(4), node);
        (store.put_object(&r).unwrap(), r)
    } else {
        (submitted, record(oid(1), oid(2), node))
    };
    let names = if rebased { vec![landed] } else { Vec::new() };
    store
        .land(&Landing {
            change: landed,
            record: &landed_record,
            entry: (seq, b"landed"),
            names: &names,
            identity_index: Some((landed_record.result, oid(7))),
        })
        .unwrap();
    (submitted, landed, node)
}

fn assert_landed(store: &Store, submitted: ObjectId, landed: ObjectId, node: NodeId) {
    assert_eq!(store.queue_entry(0).unwrap(), Some(b"landed".to_vec()));
    assert_eq!(store.log().unwrap(), vec![landed]);
    assert_eq!(store.head().unwrap(), Some(landed));
    assert_eq!(store.identity_index(oid(4)).unwrap(), Some(oid(7)));
    assert_eq!(store.node_history(node).unwrap(), vec![landed]);
    assert_eq!(store.queue_named(submitted).unwrap(), vec![0]);
    assert_eq!(store.queue_named(landed).unwrap(), vec![0]);
}

/// The whole landing is in the index after `land` returns, with nothing
/// else flushing it: a process killed right after it loses none of it.
/// Before, the queue status and identity pointer were separate non-durable
/// commits and `node_history` a separate durable one after `head`.
#[test]
fn a_landing_survives_an_abort_right_after_land() {
    const CHILD: &str = "HORD_STORE_LAND_ABORT_DIR";
    if let Ok(dir) = std::env::var(CHILD) {
        let store = Store::open(&dir).unwrap();
        queue_and_land(&store, true);
        // No drop, no flush: the process dies with the store open.
        std::process::abort();
    }
    let dir = temp_repo("abort");
    drop(Store::create(&dir).unwrap());
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_landing_survives_an_abort_right_after_land",
            "--nocapture",
        ])
        .env(CHILD, &dir)
        .status()
        .unwrap();
    assert!(!status.success(), "the child must abort");
    let store = Store::open(&dir).unwrap();
    let log = store.log().unwrap();
    assert_eq!(log.len(), 1, "the landing reached the log");
    let landed = log[0];
    // Content-addressed: storing the record again gives its id.
    let submitted = store
        .put_object(&record(oid(1), oid(2), NodeId::from_u128(42)))
        .unwrap();
    assert_landed(&store, submitted, landed, NodeId::from_u128(42));
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn land_writes_the_entry_log_head_identity_pointer_and_history_together() {
    let dir = temp_repo("together");
    let store = Store::create(&dir).unwrap();
    // A buffered log entry from before (git import) lands first, in order.
    let earlier = oid(5);
    store.append_log(earlier).unwrap();
    let (submitted, landed, node) = {
        let node = NodeId::from_u128(42);
        let submitted = store.put_object(&record(oid(1), oid(2), node)).unwrap();
        let seq = store.queue_push(b"queued", &[submitted], &[]).unwrap();
        store
            .land(&Landing {
                change: submitted,
                record: &record(oid(1), oid(2), node),
                entry: (seq, b"landed"),
                names: &[],
                identity_index: Some((oid(2), oid(7))),
            })
            .unwrap();
        (submitted, submitted, node)
    };
    assert_eq!(store.log().unwrap(), vec![earlier, landed]);
    assert_eq!(store.log_position(landed).unwrap(), Some(1));
    assert_eq!(store.log_since(1).unwrap(), vec![landed]);
    assert!(store.log_contains(earlier).unwrap());
    assert_eq!(store.node_history(node).unwrap(), vec![landed]);
    assert_eq!(store.queue_named(submitted).unwrap(), vec![0]);
    // Indexing it again, as `hord index` would, changes nothing.
    store.index_change(landed).unwrap();
    assert_eq!(store.node_history(node).unwrap(), vec![landed]);
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.log().unwrap(), vec![earlier, landed]);
    assert_eq!(store.head().unwrap(), Some(landed));
    assert_eq!(store.identity_index(oid(2)).unwrap(), Some(oid(7)));
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_rebased_landing_is_found_by_both_ids() {
    let dir = temp_repo("rebased");
    let store = Store::create(&dir).unwrap();
    let (submitted, landed, node) = queue_and_land(&store, true);
    assert_ne!(submitted, landed);
    assert_landed(&store, submitted, landed, node);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn queue_names_are_sorted_sets_and_survive_reopen() {
    let dir = temp_repo("names");
    let store = Store::create(&dir).unwrap();
    assert!(store.queue_names_indexed().unwrap());
    let a = oid(1);
    let b = oid(2);
    assert_eq!(store.queue_push(b"0", &[a], &[]).unwrap(), 0);
    assert_eq!(store.queue_push(b"1", &[b], &[]).unwrap(), 1);
    assert_eq!(store.queue_push(b"2", &[a, a], &[]).unwrap(), 2);
    assert_eq!(store.queue_named(a).unwrap(), vec![0, 2]);
    assert_eq!(store.queue_named(b).unwrap(), vec![1]);
    assert!(store.queue_named(oid(3)).unwrap().is_empty());
    // Re-indexing an entry is a no-op.
    store.index_queue_names(&[(0, vec![a, b])]).unwrap();
    assert_eq!(store.queue_named(a).unwrap(), vec![0, 2]);
    assert_eq!(store.queue_named(b).unwrap(), vec![0, 1]);
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.queue_names_indexed().unwrap());
    assert_eq!(store.queue_named(a).unwrap(), vec![0, 2]);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn checked_marks_persist() {
    let dir = temp_repo("checked");
    let store = Store::create(&dir).unwrap();
    assert!(!store.is_checked(oid(1)).unwrap());
    store.mark_checked(oid(1)).unwrap();
    assert!(store.is_checked(oid(1)).unwrap());
    // Durable with the next durable commit, which can carry marks too.
    store.queue_push(b"x", &[oid(3)], &[oid(3)]).unwrap();
    assert!(store.is_checked(oid(3)).unwrap());
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.is_checked(oid(1)).unwrap());
    assert!(store.is_checked(oid(3)).unwrap());
    assert!(!store.is_checked(oid(2)).unwrap());
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}
