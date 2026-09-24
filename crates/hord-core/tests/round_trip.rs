//! Types round-trip through canonical CBOR; ObjectId is stable.

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{
    Acceptance, Actor, Blob, Bytes, ChangeRecord, Evidence, EvidenceKind, EvidenceResult,
    FileIdentity, IdentityDelta, IdentityEntry, IdentityMap, IdentityTree, IndexPointers, Intent,
    IntentRef, LandPolicy, LangId, Node, NodeFile, NodeId, NodeKind, NodePath, ObjectId, Op,
    Policy, PolicyRule, PolicyWhen, Provenance, QualifiedName, RepoPath, Signature, Snapshot,
    SnapshotMetadata, Timestamp, Tree, TreeEntry, TreeOpKind,
};
use hord_encoding::{decode, encode};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes([byte; ObjectId::LEN])
}

fn nid(s: &str) -> NodeId {
    s.parse().unwrap()
}

fn assert_round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = encode(value).expect("encode");
    let back: T = decode(&bytes).expect("decode");
    assert_eq!(value, &back, "round-trip mismatch");
    let id1 = ObjectId::of(value).expect("ObjectId::of");
    let id2 = ObjectId::of(value).expect("ObjectId::of again");
    assert_eq!(id1, id2, "ObjectId must be stable for the same value");
    assert_eq!(id1, ObjectId::from_canonical(&bytes));
}

fn sample_blob() -> Blob {
    Blob::new(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]))
}

#[derive(Serialize)]
struct LeafWire {
    kind: NodeKind,
    lang: LangId,
    raw: Bytes,
    normalized: ObjectId,
    children: Vec<ObjectId>,
    name: Option<QualifiedName>,
}

fn sample_node() -> Node {
    Node {
        kind: NodeKind::new("identifier"),
        lang: LangId::new("rust"),
        raw: Bytes::from(b"fn".as_slice()),
        normalized: oid(0x01),
        children: Vec::new(),
        name: None,
    }
}

fn sample_internal_node() -> Node {
    Node {
        kind: NodeKind::new("fn_item"),
        lang: LangId::new("rust"),
        raw: Bytes::from(b"fn f() {}".as_slice()),
        normalized: oid(0x01),
        children: vec![oid(0x02), oid(0x03)],
        name: Some(QualifiedName::new("demo::f")),
    }
}

fn sample_tree() -> Tree {
    let mut entries = BTreeMap::new();
    entries.insert("README.md".into(), TreeEntry::Blob(oid(0x10)));
    entries.insert("src".into(), TreeEntry::Tree(oid(0x11)));
    entries.insert("lib.rs".into(), TreeEntry::NodeFile(oid(0x12)));
    Tree { entries }
}

fn sample_snapshot() -> Snapshot {
    Snapshot {
        tree: oid(0x11),
        metadata: SnapshotMetadata {
            toolchain: Some(oid(0x22)),
        },
        index: IndexPointers {
            identity: Some(oid(0x33)),
            edges: None,
        },
    }
}

fn sample_change() -> ChangeRecord {
    let n = nid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    let p = nid("01BX5ZZKBKACTAV9WEVGEMMVRZ");
    ChangeRecord {
        base: oid(0xaa),
        result: oid(0xbb),
        parents: vec![oid(0xcc)],
        ops: vec![
            Op::Insert {
                parent: p,
                index: 0,
                node: oid(0x01),
            },
            Op::Delete { node: n },
            Op::Replace {
                node: n,
                from: oid(0x02),
                to: oid(0x03),
            },
            Op::Move {
                node: n,
                from_parent: p,
                to_parent: p,
                index: 1,
            },
            Op::Rename {
                node: n,
                from: QualifiedName::new("old"),
                to: QualifiedName::new("new"),
            },
            Op::Blob {
                path: "a.bin".parse().unwrap(),
                from: Some(oid(0x04)),
                to: None,
            },
            Op::Tree {
                path: "src".parse().unwrap(),
                kind: TreeOpKind::CreateDir,
            },
        ],
        intent: Intent {
            summary: "do the thing".into(),
            body: "full description".into(),
            refs: vec![
                IntentRef::Issue { id: "1".into() },
                IntentRef::GitCommit {
                    sha: "deadbeef".into(),
                },
            ],
            acceptance: vec![Acceptance::Test {
                name: "types::round_trip".into(),
            }],
        },
        provenance: Provenance {
            actor: Actor::Human {
                id: "alice <a@example.com>".into(),
            },
            toolchain: oid(0x07),
            created_at: Timestamp::from_millis(1_700_000_000_000),
            session: Some("sess-1".into()),
            parent_intent: None,
        },
        read_set: BTreeSet::from([n]),
        write_set: BTreeSet::from([n, p]),
        identity_deltas: vec![IdentityDelta::Birth { node: n }],
        evidence: vec![oid(0x08)],
        signature: Some(Signature {
            key_id: "k1".into(),
            bytes: Bytes::from(vec![0u8; 64]),
        }),
        rebased_from: None,
    }
}

fn sample_evidence() -> Evidence {
    Evidence {
        kind: EvidenceKind::Check,
        qualifier: None,
        snapshot: oid(0xaa),
        toolchain: oid(0x07),
        command: "cargo check".into(),
        scope: Some(BTreeSet::from([nid("01ARZ3NDEKTSV4RRFFQ69G5FAV")])),
        result: EvidenceResult::Pass,
        log: None,
        cost_ms: 42,
        produced_by: Actor::Agent {
            id: "grok".into(),
            model: "grok-4".into(),
            model_hash: Bytes::from(vec![0x01, 0x02]),
            harness: "hord-cli".into(),
        },
        produced_at: Timestamp::from_millis(1),
    }
}

fn sample_policy() -> Policy {
    Policy {
        land: LandPolicy {
            require: vec!["check".into(), "test:selected".into(), "lint".into()],
            strict_reads: false,
            max_write_set: Some(200),
            max_impact: Some(50),
            max_replay_attempts: 2,
        },
        rules: vec![PolicyRule {
            name: "unsafe requires human".into(),
            when: PolicyWhen {
                touches_kind: Some("unsafe_block".into()),
                ..PolicyWhen::default()
            },
            require: vec!["review:human".into()],
        }],
    }
}

fn sample_identity_map() -> IdentityMap {
    let mut nodes = BTreeMap::new();
    nodes.insert(
        nid("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        NodePath {
            file: "src/lib.rs".parse().unwrap(),
            pointer: vec![0, 2],
        },
    );
    IdentityMap {
        nodes,
        deltas: vec![IdentityDelta::Death {
            node: nid("01BX5ZZKBKACTAV9WEVGEMMVRZ"),
        }],
    }
}

#[test]
fn blob_round_trip_and_stable_id() {
    assert_round_trip(&sample_blob());
}

#[test]
fn node_round_trip_and_stable_id() {
    assert_round_trip(&sample_node());
    let leaf = sample_node();
    let leaf_mirror = LeafWire {
        kind: leaf.kind,
        lang: leaf.lang,
        raw: leaf.raw.clone(),
        normalized: leaf.normalized,
        children: leaf.children.clone(),
        name: leaf.name.clone(),
    };
    assert_eq!(
        encode(&leaf).unwrap(),
        encode(&leaf_mirror).unwrap(),
        "leaf ObjectId preimage must stay the derived Node encoding"
    );
    let internal = sample_internal_node();
    let encoded = encode(&internal).expect("encode internal");
    let back: Node = decode(&encoded).expect("decode internal");
    assert!(back.raw.is_empty(), "stored internal node has no raw");
    assert_eq!(back.children, internal.children);
    assert_eq!(back.normalized, internal.normalized);
    assert_eq!(
        ObjectId::of(&internal).unwrap(),
        ObjectId::from_canonical(&encoded)
    );
    assert_eq!(
        ObjectId::of(&internal).unwrap(),
        ObjectId::of(&back).unwrap()
    );
}

#[test]
fn tree_and_node_file_round_trip() {
    assert_round_trip(&sample_tree());
    let file = NodeFile {
        adapter: "hord-lang-rust".into(),
        lang: LangId::new("rust"),
        root: oid(0x01),
        raw_hash: oid(0x02),
    };
    assert_round_trip(&file);
}

#[test]
fn snapshot_round_trip_and_stable_id() {
    assert_round_trip(&sample_snapshot());
}

#[test]
fn change_record_round_trip() {
    assert_round_trip(&sample_change());
    let rebased = ChangeRecord {
        rebased_from: Some(oid(0xdd)),
        signature: None,
        ..sample_change()
    };
    assert_round_trip(&rebased);
}

/// ADR 0018: `rebased_from` is omitted when `None`, so a record that was not
/// rebased encodes (and hashes) exactly as before the field existed.
#[test]
fn rebased_from_is_omitted_when_none() {
    let plain = encode(&sample_change()).unwrap();
    assert!(
        !plain
            .windows(b"rebased_from".len())
            .any(|w| w == b"rebased_from"),
        "a record that was not rebased must not name the field"
    );
    let rebased = ChangeRecord {
        rebased_from: Some(oid(0xdd)),
        ..sample_change()
    };
    let bytes = encode(&rebased).unwrap();
    assert!(
        bytes
            .windows(b"rebased_from".len())
            .any(|w| w == b"rebased_from")
    );
    assert_ne!(
        ObjectId::of(&rebased).unwrap(),
        ObjectId::of(&sample_change()).unwrap()
    );
}

/// ADR 0017: the identity tree and file identity objects round-trip, and
/// the empty snapshot names the empty tree and the empty identity tree.
#[test]
fn identity_tree_and_snapshot_round_trip() {
    let file = FileIdentity {
        blob: oid(0x10),
        nodes: vec![
            (vec![0], nid("01ARZ3NDEKTSV4RRFFQ69G5FAV")),
            (vec![2, 1], nid("01BX5ZZKBKACTAV9WEVGEMMVRZ")),
        ],
    };
    assert_round_trip(&file);
    let mut tree = IdentityTree::default();
    tree.entries
        .insert("src".into(), IdentityEntry::Dir(oid(0x11)));
    tree.entries.insert(
        "lib.rs".into(),
        IdentityEntry::File(ObjectId::of(&file).unwrap()),
    );
    assert_round_trip(&tree);
    let empty = Snapshot::empty();
    assert_eq!(empty.root(), ObjectId::of(&Tree::default()).unwrap());
    assert_eq!(
        empty.identity(),
        Some(ObjectId::of(&IdentityTree::default()).unwrap())
    );
    assert_round_trip(&Snapshot::new(oid(1), oid(2)));
    assert_ne!(
        ObjectId::of(&Snapshot::new(oid(1), oid(2))).unwrap(),
        ObjectId::of(&Snapshot::new(oid(1), oid(3))).unwrap(),
        "same content, different identity: different snapshots"
    );
}

#[test]
fn evidence_policy_identity_round_trip() {
    assert_round_trip(&sample_evidence());
    assert_round_trip(&sample_policy());
    assert_round_trip(&sample_identity_map());
}

#[test]
fn blob_bytes_are_cbor_major_type_2_not_int_array() {
    let blob = sample_blob();
    let encoded = encode(&blob).unwrap();
    let hex = hex::encode(&encoded);
    assert!(
        hex.contains("44deadbeef"),
        "expected bstr 0x44deadbeef, got {hex}"
    );
    assert!(
        !hex.contains("84"),
        "bytes must not encode as a 4-element array: {hex}"
    );
}

#[test]
fn nodeid_parse_display() {
    let s = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let id: NodeId = s.parse().unwrap();
    assert_eq!(id.to_string(), s);
    let bytes = encode(&id).unwrap();
    assert_eq!(decode::<NodeId>(&bytes).unwrap(), id);
}

#[test]
fn object_id_differs_when_value_differs() {
    let a = Blob::new(Bytes::from(vec![1]));
    let b = Blob::new(Bytes::from(vec![2]));
    assert_ne!(ObjectId::of(&a).unwrap(), ObjectId::of(&b).unwrap());
}

#[test]
fn repo_path_in_ops_round_trips() {
    let path: RepoPath = "crates/hord-core/Cargo.toml".parse().unwrap();
    let op = Op::Tree {
        path: path.clone(),
        kind: TreeOpKind::Rename {
            to: "crates/hord-core/Cargo.toml.bak".parse().unwrap(),
        },
    };
    assert_round_trip(&op);
    assert_eq!(path.to_string(), "crates/hord-core/Cargo.toml");
}
