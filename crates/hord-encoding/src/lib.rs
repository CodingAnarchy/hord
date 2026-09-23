//! Canonical CBOR encoding and content-addressed [`ObjectId`]s for Hord.
//!
//! Objects are encoded with **canonical CBOR** (RFC 8949 §4.2.1): one encoding
//! per value, preferred (shortest) integer and length serialization, definite
//! lengths only, and map keys sorted in the bytewise lexicographic order of
//! their deterministic encodings. [`ObjectId`] is BLAKE3-256 over that encoding
//! (spec §3.1, §3.9).
//!
//! Hashed object fields must not contain floating-point values.
//!
//! Serde's default `Vec<u8>` encoding is a CBOR array of integers, not a byte
//! string. Use `cbor2::Value::Bytes` or `serde_bytes` when the wire type must
//! be major type 2.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod error;
mod object_id;

pub use error::Error;
pub use object_id::ObjectId;

use serde::{Deserialize, Serialize};

/// Encode `value` as canonical CBOR (RFC 8949 §4.2.1).
///
/// Map keys are sorted in the bytewise lexicographic order of their
/// deterministic encodings. Integers and lengths use preferred (shortest)
/// serialization. Indefinite-length items are never produced.
///
/// The same value always yields the same bytes, regardless of map insertion
/// order in the input.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    cbor2::to_canonical_vec(value).map_err(|e| Error::Encode(e.to_string()))
}

/// Encode `value` into `out` with cbor2's streaming serializer, skipping the
/// canonicalization pass that [`encode`] runs.
///
/// The bytes equal [`encode`]'s only for a value already in canonical form:
/// every struct and map emits its keys in RFC 8949 §4.2.1 order (shorter
/// encoded key first, then bytewise), no map is emitted from unordered
/// storage, and no float appears. Integers, lengths, and definite-length
/// items are the same in both paths. Callers own that guarantee and must
/// check it with a test against [`encode`] (for example, a proptest).
pub fn encode_ordered_into<T: Serialize + ?Sized>(
    value: &T,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    cbor2::to_writer(value, out).map_err(|e| Error::Encode(e.to_string()))
}

/// Decode exactly one well-formed CBOR item from `bytes` into `T`.
///
/// Trailing bytes are rejected. Non-canonical encodings are accepted; re-encode
/// with [`encode`] before hashing.
pub fn decode<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, Error> {
    cbor2::validate_slice(bytes).map_err(|e| Error::Decode(e.to_string()))?;
    cbor2::from_slice(bytes).map_err(|e| Error::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Named {
        kind: String,
        lang: String,
    }

    #[test]
    fn struct_map_keys_are_sorted() {
        let value = Named {
            lang: "rust".into(),
            kind: "fn_item".into(),
        };
        // "kind" (0x64…) sorts before "lang" (0x64… with a later first-diff).
        let bytes = encode(&value).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "a2646b696e6467666e5f6974656d646c616e676472757374"
        );
        assert_eq!(decode::<Named>(&bytes).unwrap(), value);
    }

    #[test]
    fn hashmap_insertion_order_is_irrelevant() {
        let mut a = HashMap::new();
        a.insert("z", 1i64);
        a.insert("aa", 2);
        a.insert("b", 3);
        let mut b = HashMap::new();
        b.insert("aa", 2i64);
        b.insert("b", 3);
        b.insert("z", 1);
        assert_eq!(encode(&a).unwrap(), encode(&b).unwrap());
        assert_eq!(hex::encode(encode(&a).unwrap()), "a3616203617a0162616102");
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode(&42i64).unwrap();
        bytes.push(0x00);
        assert!(decode::<i64>(&bytes).is_err());
    }

    #[test]
    fn object_id_matches_hash_of_encoding() {
        let value = 0u8;
        let encoded = encode(&value).unwrap();
        assert_eq!(
            ObjectId::of(&value).unwrap(),
            ObjectId::from_canonical(&encoded)
        );
    }
}
