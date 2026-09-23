//! Edge and node-history index (spec §3.8, §8.1).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{
    Actor, Blob, ChangeRecord, IdentityDelta, Intent, NodeId, ObjectId, Op, Provenance, RepoPath,
    SnapshotId, Timestamp,
};
use hord_store::{EdgeKind, Error, Store};
use proptest::prelude::*;

fn temp_repo() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-index-{}-{}",
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

fn snap(n: u8) -> SnapshotId {
    let mut bytes = [0u8; 32];
    bytes[0] = n;
    ObjectId::from_bytes(bytes)
}

fn nid(n: u128) -> NodeId {
    NodeId::from_u128(n)
}

fn record(
    write: impl IntoIterator<Item = NodeId>,
    read: impl IntoIterator<Item = NodeId>,
    ops: Vec<Op>,
    deltas: Vec<IdentityDelta>,
) -> ChangeRecord {
    ChangeRecord {
        base: snap(1),
        result: snap(2),
        parents: Vec::new(),
        ops,
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
            toolchain: snap(3),
            created_at: Timestamp::from_millis(0),
            session: None,
            parent_intent: None,
        },
        read_set: read.into_iter().collect(),
        write_set: write.into_iter().collect(),
        identity_deltas: deltas,
        evidence: Vec::new(),
        signature: None,
        rebased_from: None,
    }
}

#[test]
fn edges_are_per_snapshot_kind_and_source() {
    let (_g, store) = store();
    let source = nid(7);
    let kinds = [
        EdgeKind::Contains,
        EdgeKind::References,
        EdgeKind::Depends,
        EdgeKind::Tests,
        EdgeKind::DerivedFrom,
    ];
    for (i, kind) in kinds.iter().enumerate() {
        let id = store
            .put_edge(snap(1), *kind, source, nid(10 + i as u128))
            .unwrap();
        let again = store
            .put_edge(snap(1), *kind, source, nid(10 + i as u128))
            .unwrap();
        assert_eq!(id, again);
        assert!(store.contains(id).unwrap());
    }
    store
        .put_edge(snap(1), EdgeKind::References, source, nid(1))
        .unwrap();
    store
        .put_edge(snap(1), EdgeKind::References, source, nid(u128::MAX))
        .unwrap();
    store
        .put_edge(snap(1), EdgeKind::References, source, NodeId::nil())
        .unwrap();
    store
        .put_edge(snap(1), EdgeKind::References, nid(8), nid(9))
        .unwrap();
    store
        .put_edge(snap(2), EdgeKind::References, source, nid(4))
        .unwrap();

    for (i, kind) in kinds.iter().enumerate() {
        if *kind == EdgeKind::References {
            assert_eq!(
                store.edges(snap(1), source, *kind).unwrap(),
                vec![NodeId::nil(), nid(1), nid(10 + i as u128), nid(u128::MAX)]
            );
        } else {
            assert_eq!(
                store.edges(snap(1), source, *kind).unwrap(),
                vec![nid(10 + i as u128)]
            );
        }
    }
    assert_eq!(
        store.edges(snap(1), nid(8), EdgeKind::References).unwrap(),
        vec![nid(9)]
    );
    assert_eq!(
        store.edges(snap(2), source, EdgeKind::References).unwrap(),
        vec![nid(4)]
    );
    assert!(
        store
            .edges(snap(1), nid(99), EdgeKind::Contains)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn edges_survive_reopen_pack_and_rebuild() {
    let path = temp_repo();
    let _g = Guard(path.clone());
    let source = nid(1);
    let target = nid(2);
    {
        let store = Store::create(&path).unwrap();
        store
            .put_edge(snap(1), EdgeKind::Depends, source, target)
            .unwrap();
    }
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.edges(snap(1), source, EdgeKind::Depends).unwrap(),
            vec![target]
        );
        store.pack().unwrap();
        store.rebuild_index().unwrap();
        assert_eq!(
            store.edges(snap(1), source, EdgeKind::Depends).unwrap(),
            vec![target]
        );
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.edges(snap(1), source, EdgeKind::Depends).unwrap(),
        vec![target]
    );
}

#[test]
fn node_history_follows_landed_touches_and_rebuilds() {
    let (_g, store) = store();
    let n1 = nid(1);
    let n2 = nid(2);
    let n3 = nid(3);
    let c1 = record([n1], [], vec![Op::Delete { node: n1 }], vec![]);
    let c2 = record([n2], [n1], vec![], vec![]);
    let c3 = record([n1], [], vec![], vec![IdentityDelta::Birth { node: n3 }]);
    let blob_only = record(
        [],
        [],
        vec![Op::Blob {
            path: RepoPath::default(),
            from: None,
            to: None,
        }],
        vec![],
    );
    let id1 = store.put_object(&c1).unwrap();
    let id2 = store.put_object(&c2).unwrap();
    let id3 = store.put_object(&c3).unwrap();
    let id_blob = store.put_object(&blob_only).unwrap();
    let raw = store
        .put_object(&Blob::new(b"not-a-change".to_vec()))
        .unwrap();

    let err = store.index_change(id1).unwrap_err();
    assert!(matches!(err, Error::NotInLog(_)));

    store.append_log(id1).unwrap();
    store.append_log(raw).unwrap();
    store.append_log(id_blob).unwrap();
    store.append_log(id2).unwrap();
    store.append_log(id3).unwrap();
    // Index out of landing order. History must still follow the log.
    store.index_change(id3).unwrap();
    store.index_change(id1).unwrap();
    store.index_change(id2).unwrap();
    store.index_change(id_blob).unwrap();
    let err = store.index_change(raw).unwrap_err();
    assert!(matches!(err, Error::Encoding(_)));

    assert_eq!(store.node_history(n1).unwrap(), vec![id1, id3]);
    assert_eq!(store.node_history(n2).unwrap(), vec![id2]);
    assert_eq!(store.node_history(n3).unwrap(), vec![id3]);
    assert!(store.node_history(nid(99)).unwrap().is_empty());
    // Indexing again does not duplicate.
    store.index_change(id1).unwrap();
    assert_eq!(store.node_history(n1).unwrap(), vec![id1, id3]);

    store.pack().unwrap();
    store.rebuild_index().unwrap();
    assert_eq!(store.node_history(n1).unwrap(), vec![id1, id3]);
    assert_eq!(store.node_history(n2).unwrap(), vec![id2]);
    assert_eq!(store.node_history(n3).unwrap(), vec![id3]);
}

#[test]
fn rebuild_keeps_history_when_a_log_object_is_missing() {
    let (_g, store) = store();
    let n1 = nid(1);
    let change = record([n1], [], vec![], vec![]);
    let id = store.put_object(&change).unwrap();
    store.append_log(id).unwrap();
    store.index_change(id).unwrap();
    assert_eq!(store.node_history(n1).unwrap(), vec![id]);

    let missing = ObjectId::from_canonical(b"missing-change");
    store.append_log(missing).unwrap();
    let err = store.rebuild_index().unwrap_err();
    assert!(matches!(err, Error::MissingObject(_)));
    assert_eq!(store.node_history(n1).unwrap(), vec![id]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn edges_match_after_rebuild(
        edges in prop::collection::vec((0..5u8, 0..4u128, 0..4u128, 0..2u8), 0..12)
    ) {
        let (_g, store) = store();
        let mut expected: BTreeMap<(u8, u128, u8), BTreeSet<u128>> = BTreeMap::new();
        for (kind, source, target, snapshot) in &edges {
            let kind_enum = match kind {
                0 => EdgeKind::Contains,
                1 => EdgeKind::References,
                2 => EdgeKind::Depends,
                3 => EdgeKind::Tests,
                _ => EdgeKind::DerivedFrom,
            };
            store.put_edge(snap(*snapshot), kind_enum, nid(*source), nid(*target)).unwrap();
            expected.entry((*snapshot, *source, *kind)).or_default().insert(*target);
        }
        for pass in 0..2 {
            for ((snapshot, source, kind), targets) in &expected {
                let kind_enum = match kind {
                    0 => EdgeKind::Contains,
                    1 => EdgeKind::References,
                    2 => EdgeKind::Depends,
                    3 => EdgeKind::Tests,
                    _ => EdgeKind::DerivedFrom,
                };
                let got = store.edges(snap(*snapshot), nid(*source), kind_enum).unwrap();
                let want: Vec<NodeId> = targets.iter().copied().map(nid).collect();
                prop_assert_eq!(got, want);
            }
            if pass == 0 {
                store.rebuild_index().unwrap();
            }
        }
    }
}
