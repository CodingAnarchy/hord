//! The streamed node encoding equals the canonical one.
//!
//! [`Node::content_id`] and [`ObjectId::of_ordered`] skip cbor2's
//! canonicalizing pass. That is only sound while the streamed bytes equal
//! [`hord_encoding::encode`]'s, which these properties check on random nodes,
//! on random normalized-children arrays, and on leaf byte strings.

use hord_core::{Bytes, LangId, Node, NodeKind, ObjectId, QualifiedName};
use hord_encoding::{encode, encode_ordered_into};
use proptest::prelude::*;

fn arb_oid() -> impl Strategy<Value = ObjectId> {
    any::<[u8; ObjectId::LEN]>().prop_map(ObjectId::from_bytes)
}

fn arb_node() -> impl Strategy<Value = Node> {
    (
        "[a-z_]{0,24}",
        "[a-z]{0,8}",
        // Lengths straddle every CBOR length-header width (23/24, 255/256).
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..30),
            prop::collection::vec(any::<u8>(), 250..300),
        ],
        arb_oid(),
        // Empty children makes a leaf (six fields); otherwise five.
        prop_oneof![
            Just(Vec::new()),
            prop::collection::vec(arb_oid(), 1..4),
            prop::collection::vec(arb_oid(), 20..30),
        ],
        prop::option::of("[a-zA-Z0-9_:<> ]{0,40}"),
    )
        .prop_map(|(kind, lang, raw, normalized, children, name)| Node {
            kind: NodeKind::new(kind),
            lang: LangId::new(lang),
            raw: Bytes::from(raw),
            normalized,
            children,
            name: name.map(QualifiedName::new),
        })
}

fn streamed<T: serde::Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, hord_encoding::Error> {
    let mut out = Vec::new();
    encode_ordered_into(value, &mut out)?;
    Ok(out)
}

proptest! {
    #[test]
    fn node_streamed_bytes_equal_canonical(node in arb_node()) {
        prop_assert_eq!(streamed(&node)?, encode(&node)?);
        prop_assert_eq!(node.content_id()?, ObjectId::of(&node)?);
    }

    #[test]
    fn normalized_children_streamed_bytes_equal_canonical(
        ids in prop::collection::vec(arb_oid(), 0..40),
    ) {
        prop_assert_eq!(streamed(ids.as_slice())?, encode(ids.as_slice())?);
        prop_assert_eq!(
            ObjectId::of_ordered(ids.as_slice())?,
            ObjectId::of(ids.as_slice())?
        );
    }

    #[test]
    fn leaf_text_streamed_bytes_equal_canonical(
        bytes in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..300),
            prop::collection::vec(any::<u8>(), 65_530..65_540),
        ],
    ) {
        let bytes = Bytes::from(bytes);
        prop_assert_eq!(streamed(&bytes)?, encode(&bytes)?);
        prop_assert_eq!(ObjectId::of_ordered(&bytes)?, ObjectId::of(&bytes)?);
        prop_assert_eq!(ObjectId::of_byte_string(bytes.as_slice()), ObjectId::of(&bytes)?);
    }
}

/// A node whose `Serialize` used the old (non-canonical) field order would
/// stream different bytes; this pins the fast path to the canonical bytes
/// of a fixed leaf and branch.
#[test]
fn fixed_nodes_match_canonical() -> Result<(), Box<dyn std::error::Error>> {
    let leaf = Node {
        kind: NodeKind::new("identifier"),
        lang: LangId::new("rust"),
        raw: Bytes::from(b"  foo\n".as_slice()),
        normalized: ObjectId::from_bytes([7; ObjectId::LEN]),
        children: Vec::new(),
        name: None,
    };
    let branch = Node {
        children: vec![ObjectId::of(&leaf)?; 3],
        name: Some(QualifiedName::new("m::f")),
        ..leaf.clone()
    };
    for node in [&leaf, &branch] {
        assert_eq!(streamed(node)?, encode(node)?);
        assert_eq!(node.content_id()?, ObjectId::of(node)?);
    }
    Ok(())
}
