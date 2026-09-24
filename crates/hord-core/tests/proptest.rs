//! Property tests: encoding round-trip and stable ObjectId (spec §11.1).

use hord_core::{
    Blob, Bytes, EvidenceKind, EvidenceResult, LandPolicy, LangId, Node, NodeKind, ObjectId,
    Policy, PolicyRule, PolicyWhen, QualifiedName,
};
use hord_encoding::{decode, encode};
use proptest::prelude::*;

fn arb_bytes() -> impl Strategy<Value = Bytes> {
    prop::collection::vec(any::<u8>(), 0..48).prop_map(Bytes::from)
}

fn oid_from_seed(seed: u8) -> ObjectId {
    ObjectId::from_bytes([seed; ObjectId::LEN])
}

proptest! {
    #[test]
    fn blob_round_trip(payload in arb_bytes()) {
        let blob = Blob::new(payload);
        let encoded = encode(&blob).unwrap();
        let back: Blob = decode(&encoded).unwrap();
        prop_assert_eq!(&blob, &back);
        prop_assert_eq!(ObjectId::of(&blob).unwrap(), ObjectId::from_canonical(&encoded));
        let payload = encode(&blob.bytes).unwrap();
        prop_assert_eq!(payload[0] >> 5, 2, "Bytes must encode as CBOR major type 2");
    }

    #[test]
    fn blob_object_id_is_stable(payload in arb_bytes()) {
        let blob = Blob::new(payload);
        let a = ObjectId::of(&blob).unwrap();
        let b = ObjectId::of(&blob).unwrap();
        prop_assert_eq!(a, b);
    }

    #[test]
    fn node_round_trip(
        kind in "[a-z_]{1,16}",
        lang in "[a-z]{1,8}",
        raw in arb_bytes(),
        seed in any::<u8>(),
        child_seeds in prop::collection::vec(any::<u8>(), 0..4),
        name in prop::option::of("[A-Za-z0-9_:]+"),
    ) {
        let node = Node {
            kind: NodeKind::new(kind),
            lang: LangId::new(lang),
            raw,
            normalized: oid_from_seed(seed),
            children: child_seeds.into_iter().map(oid_from_seed).collect(),
            name: name.map(QualifiedName::new),
        };
        let encoded = encode(&node).unwrap();
        let back: Node = decode(&encoded).unwrap();
        if node.children.is_empty() {
            prop_assert_eq!(&node, &back);
        } else {
            prop_assert!(back.raw.is_empty());
            prop_assert_eq!(&back.kind, &node.kind);
            prop_assert_eq!(&back.children, &node.children);
            prop_assert_eq!(back.normalized, node.normalized);
        }
        prop_assert_eq!(ObjectId::of(&node).unwrap(), ObjectId::from_canonical(&encoded));
    }

    #[test]
    fn policy_round_trip(
        require in prop::collection::vec("[a-z:]{1,16}", 0..4),
        strict_reads in any::<bool>(),
        max_write_set in 0u64..10_000,
        max_replay_attempts in 0u64..8,
        rule_name in "[a-z ]{1,24}",
        touches_kind in prop::option::of("[a-z_]{1,16}"),
    ) {
        let policy = Policy {
            land: LandPolicy {
                require,
                strict_reads,
                max_write_set: Some(max_write_set),
                max_impact: None,
                max_replay_attempts,
            },
            rules: vec![PolicyRule {
                name: rule_name,
                when: PolicyWhen {
                    touches_kind,
                    ..PolicyWhen::default()
                },
                require: vec!["review:human".into()],
            }],
        };
        let encoded = encode(&policy).unwrap();
        let back: Policy = decode(&encoded).unwrap();
        prop_assert_eq!(&policy, &back);
        prop_assert_eq!(ObjectId::of(&policy).unwrap(), ObjectId::from_canonical(&encoded));
    }

    #[test]
    fn evidence_enums_round_trip(custom in "[a-z]{1,12}", summary in ".{0,32}") {
        for kind in [
            EvidenceKind::Check,
            EvidenceKind::Test,
            EvidenceKind::Bench,
            EvidenceKind::Lint,
            EvidenceKind::Review,
            EvidenceKind::Custom(custom.clone()),
        ] {
            let encoded = encode(&kind).unwrap();
            let back: EvidenceKind = decode(&encoded).unwrap();
            prop_assert_eq!(&kind, &back);
        }
        let fail = EvidenceResult::Fail { summary: summary.clone() };
        let encoded = encode(&fail).unwrap();
        prop_assert_eq!(decode::<EvidenceResult>(&encoded).unwrap(), fail);
    }
}
