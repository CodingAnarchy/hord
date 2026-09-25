//! `Op::node_ids` and `IdentityDelta::node_ids` name every `NodeId` field.

use hord_core::{IdentityDelta, NodeId, ObjectId, Op, QualifiedName, RepoPath, TreeOpKind};

fn nid(n: u128) -> NodeId {
    NodeId::from_u128(n)
}

#[test]
fn op_node_ids_cover_every_node_id_field() -> Result<(), Box<dyn std::error::Error>> {
    let oid = ObjectId::from_bytes([1; 32]);
    let name = |s: &str| QualifiedName::from(s.to_owned());
    let path: RepoPath = "src/lib.rs".parse()?;
    let cases = [
        (
            Op::Insert {
                parent: nid(1),
                index: 0,
                node: oid,
            },
            vec![nid(1)],
        ),
        (Op::Delete { node: nid(2) }, vec![nid(2)]),
        (
            Op::Replace {
                node: nid(3),
                from: oid,
                to: oid,
            },
            vec![nid(3)],
        ),
        (
            Op::Rename {
                node: nid(4),
                from: name("a"),
                to: name("b"),
            },
            vec![nid(4)],
        ),
        (
            Op::Move {
                node: nid(5),
                from_parent: nid(6),
                to_parent: nid(7),
                index: 0,
            },
            vec![nid(5), nid(6), nid(7)],
        ),
        (
            Op::Blob {
                path: path.clone(),
                from: None,
                to: Some(oid),
            },
            vec![],
        ),
        (
            Op::Tree {
                path,
                kind: TreeOpKind::CreateFile,
            },
            vec![],
        ),
    ];
    for (op, ids) in cases {
        assert_eq!(op.node_ids().collect::<Vec<_>>(), ids, "{op:?}");
    }
    Ok(())
}

#[test]
fn delta_node_ids_cover_every_node_id_field() {
    let cases = [
        (IdentityDelta::Birth { node: nid(1) }, vec![nid(1)]),
        (IdentityDelta::Death { node: nid(2) }, vec![nid(2)]),
        (
            IdentityDelta::DerivedFrom {
                node: nid(3),
                from: nid(4),
            },
            vec![nid(3), nid(4)],
        ),
        (
            IdentityDelta::SplitInto {
                node: nid(5),
                into: vec![nid(6), nid(7)],
            },
            vec![nid(5), nid(6), nid(7)],
        ),
        (
            IdentityDelta::MergedFrom {
                node: nid(8),
                from: vec![nid(9)],
            },
            vec![nid(8), nid(9)],
        ),
    ];
    for (delta, ids) in cases {
        assert_eq!(delta.node_ids().collect::<Vec<_>>(), ids, "{delta:?}");
    }
}
