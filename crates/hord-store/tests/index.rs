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

fn temp_repo() -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-index-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn store() -> Result<(Guard, Store), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let store = Store::create(&path)?;
    Ok((Guard(path), store))
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
        intent: Intent::from_summary("s"),
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
fn edges_are_per_snapshot_kind_and_source() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let source = nid(7);
    let kinds = [
        EdgeKind::Contains,
        EdgeKind::References,
        EdgeKind::Depends,
        EdgeKind::Tests,
        EdgeKind::DerivedFrom,
    ];
    for (i, kind) in kinds.iter().enumerate() {
        let id = store.put_edge(snap(1), *kind, source, nid(10 + i as u128))?;
        let again = store.put_edge(snap(1), *kind, source, nid(10 + i as u128))?;
        assert_eq!(id, again);
        assert!(store.contains(id)?);
    }
    store.put_edge(snap(1), EdgeKind::References, source, nid(1))?;
    store.put_edge(snap(1), EdgeKind::References, source, nid(u128::MAX))?;
    store.put_edge(snap(1), EdgeKind::References, source, NodeId::nil())?;
    store.put_edge(snap(1), EdgeKind::References, nid(8), nid(9))?;
    store.put_edge(snap(2), EdgeKind::References, source, nid(4))?;

    for (i, kind) in kinds.iter().enumerate() {
        if *kind == EdgeKind::References {
            assert_eq!(
                store.edges(snap(1), source, *kind)?,
                vec![NodeId::nil(), nid(1), nid(10 + i as u128), nid(u128::MAX)]
            );
        } else {
            assert_eq!(
                store.edges(snap(1), source, *kind)?,
                vec![nid(10 + i as u128)]
            );
        }
    }
    assert_eq!(
        store.edges(snap(1), nid(8), EdgeKind::References)?,
        vec![nid(9)]
    );
    assert_eq!(
        store.edges(snap(2), source, EdgeKind::References)?,
        vec![nid(4)]
    );
    assert!(
        store
            .edges(snap(1), nid(99), EdgeKind::Contains)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn edges_survive_reopen_pack_and_rebuild() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let source = nid(1);
    let target = nid(2);
    {
        let store = Store::create(&path)?;
        store.put_edge(snap(1), EdgeKind::Depends, source, target)?;
    }
    {
        let store = Store::open(&path)?;
        assert_eq!(
            store.edges(snap(1), source, EdgeKind::Depends)?,
            vec![target]
        );
        store.pack()?;
        store.rebuild_index()?;
        assert_eq!(
            store.edges(snap(1), source, EdgeKind::Depends)?,
            vec![target]
        );
    }
    let store = Store::open(&path)?;
    assert_eq!(
        store.edges(snap(1), source, EdgeKind::Depends)?,
        vec![target]
    );
    Ok(())
}

#[test]
fn node_history_follows_landed_touches_and_rebuilds() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
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
    let id1 = store.put_object(&c1)?;
    let id2 = store.put_object(&c2)?;
    let id3 = store.put_object(&c3)?;
    let id_blob = store.put_object(&blob_only)?;
    let raw = store.put_object(&Blob::new(b"not-a-change".to_vec()))?;

    let err = store
        .index_change(id1)
        .expect_err("indexing a change not in the log fails");
    assert!(matches!(err, Error::NotInLog(_)));

    store.append_log(id1)?;
    store.append_log(raw)?;
    store.append_log(id_blob)?;
    store.append_log(id2)?;
    store.append_log(id3)?;
    // Index out of landing order. History must still follow the log.
    store.index_change(id3)?;
    store.index_change(id1)?;
    store.index_change(id2)?;
    store.index_change(id_blob)?;
    let err = store
        .index_change(raw)
        .expect_err("indexing an object that is not a change fails");
    assert!(matches!(err, Error::Encoding(_)));

    assert_eq!(store.node_history(n1)?, vec![id1, id3]);
    assert_eq!(store.node_history(n2)?, vec![id2]);
    assert_eq!(store.node_history(n3)?, vec![id3]);
    assert!(store.node_history(nid(99))?.is_empty());
    // Indexing again does not duplicate.
    store.index_change(id1)?;
    assert_eq!(store.node_history(n1)?, vec![id1, id3]);

    store.pack()?;
    store.rebuild_index()?;
    assert_eq!(store.node_history(n1)?, vec![id1, id3]);
    assert_eq!(store.node_history(n2)?, vec![id2]);
    assert_eq!(store.node_history(n3)?, vec![id3]);
    Ok(())
}

#[test]
fn rebuild_keeps_history_when_a_log_object_is_missing() -> Result<(), Box<dyn std::error::Error>> {
    let (_g, store) = store()?;
    let n1 = nid(1);
    let change = record([n1], [], vec![], vec![]);
    let id = store.put_object(&change)?;
    store.append_log(id)?;
    store.index_change(id)?;
    assert_eq!(store.node_history(n1)?, vec![id]);

    let missing = ObjectId::from_canonical(b"missing-change");
    store.append_log(missing)?;
    let err = store
        .rebuild_index()
        .expect_err("rebuilding with a missing log object fails");
    assert!(matches!(err, Error::MissingObject(_)));
    assert_eq!(store.node_history(n1)?, vec![id]);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn edges_match_after_rebuild(
        edges in prop::collection::vec((0..5u8, 0..4u128, 0..4u128, 0..2u8), 0..12)
    ) {
        let (_g, store) = store().expect("create a store in a temp dir");
        let mut expected: BTreeMap<(u8, u128, u8), BTreeSet<u128>> = BTreeMap::new();
        for (kind, source, target, snapshot) in &edges {
            let kind_enum = match kind {
                0 => EdgeKind::Contains,
                1 => EdgeKind::References,
                2 => EdgeKind::Depends,
                3 => EdgeKind::Tests,
                _ => EdgeKind::DerivedFrom,
            };
            store.put_edge(snap(*snapshot), kind_enum, nid(*source), nid(*target))?;
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
                let got = store.edges(snap(*snapshot), nid(*source), kind_enum)?;
                let want: Vec<NodeId> = targets.iter().copied().map(nid).collect();
                prop_assert_eq!(got, want);
            }
            if pass == 0 {
                store.rebuild_index()?;
            }
        }
    }
}
