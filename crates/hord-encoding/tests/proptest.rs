//! Property tests: round-trip, map-order independence, stable ObjectId.

use std::collections::{BTreeMap, HashMap};

use cbor2::Value;
use hord_encoding::{ObjectId, decode, encode};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Atom {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Atom>),
    Map(BTreeMap<String, Atom>),
}

fn arb_atom() -> impl Strategy<Value = Atom> {
    let leaf = prop_oneof![
        Just(Atom::Null),
        any::<bool>().prop_map(Atom::Bool),
        prop_oneof![
            Just(0i64),
            Just(23),
            Just(24),
            Just(255),
            Just(256),
            Just(-1),
            Just(-24),
            Just(-25),
            any::<i64>(),
        ]
        .prop_map(Atom::Int),
        "[a-zA-Z0-9_ ]{0,32}".prop_map(Atom::Text),
        prop::collection::vec(any::<u8>(), 0..48).prop_map(Atom::Bytes),
    ];
    leaf.prop_recursive(4, 32, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..8).prop_map(Atom::Array),
            prop::collection::btree_map("[a-z]{1,8}", inner, 0..8).prop_map(Atom::Map),
        ]
    })
}

proptest! {
    #[test]
    fn encode_decode_round_trip(atom in arb_atom()) {
        let bytes = encode(&atom)?;
        let back: Atom = decode(&bytes)?;
        prop_assert_eq!(atom, back);
    }

    #[test]
    fn encode_is_idempotent_after_decode(atom in arb_atom()) {
        let first = encode(&atom)?;
        let decoded: Value = decode(&first)?;
        let second = encode(&decoded)?;
        prop_assert_eq!(first, second);
    }

    #[test]
    fn map_insertion_order_does_not_affect_encoding(
        mut pairs in prop::collection::vec(("[a-z]{1,12}", any::<i64>()), 0..16)
    ) {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs.dedup_by(|a, b| a.0 == b.0);

        let mut forward = HashMap::new();
        for (k, v) in &pairs {
            forward.insert(k.clone(), *v);
        }
        let mut reverse = HashMap::new();
        for (k, v) in pairs.iter().rev() {
            reverse.insert(k.clone(), *v);
        }

        let as_value = Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (Value::from(k.as_str()), Value::from(*v)))
                .collect(),
        );

        let a = encode(&forward)?;
        let b = encode(&reverse)?;
        let c = encode(&as_value)?;
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(&a, &c);
    }

    #[test]
    fn object_id_is_stable(atom in arb_atom()) {
        let encoded = encode(&atom)?;
        let id1 = ObjectId::of(&atom)?;
        let id2 = ObjectId::of(&atom)?;
        let id3 = ObjectId::from_canonical(&encoded);
        prop_assert_eq!(id1, id2);
        prop_assert_eq!(id1, id3);
        prop_assert_eq!(id1.as_bytes().len(), ObjectId::LEN);
    }

    #[test]
    fn object_id_follows_map_order_independence(
        mut pairs in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 1..10)
    ) {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs.dedup_by(|a, b| a.0 == b.0);
        prop_assume!(!pairs.is_empty());

        let mut a = HashMap::new();
        let mut b = HashMap::new();
        for (k, v) in &pairs {
            a.insert(k.clone(), *v);
        }
        for (k, v) in pairs.iter().rev() {
            b.insert(k.clone(), *v);
        }
        prop_assert_eq!(ObjectId::of(&a)?, ObjectId::of(&b)?);
    }
}
