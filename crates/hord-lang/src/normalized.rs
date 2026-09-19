//! Trivia-stripped semantic identity (spec §3.3 `normalized`).

use hord_core::{Bytes, ObjectId};

use crate::ParseError;

/// Hash of the trivia-stripped canonical form of a subtree (spec §3.3).
///
/// **Strip:** drop every attached leading and trailing trivia byte (comments,
/// whitespace, blank lines) as produced by [`crate::attach_trivia`]. What
/// remains is the concatenation of leaf token `text` bytes in tree order.
/// Internal nodes use the concatenation of their children's stripped bytes,
/// which is the same sequence.
///
/// **Hash:** the stripped byte string is wrapped as [`Bytes`] (CBOR major
/// type 2) and hashed with [`ObjectId::of`] — BLAKE3-256 of canonical CBOR
/// (RFC 8949 §4.2.1). Kind, language, `name`, content [`ObjectId`]s, and
/// `raw` are not part of this hash.
///
/// A whitespace- or comment-only change therefore alters `raw` and the node's
/// content [`ObjectId`], but not [`hord_core::Node::normalized`].
///
/// This definition is the stable identity for this crate; do not change it
/// without an ADR.
pub fn normalized_hash(stripped: &[u8]) -> Result<ObjectId, ParseError> {
    Ok(ObjectId::of(&Bytes::from(stripped))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_stripped_bytes_same_hash() {
        let a = normalized_hash(b"fn foo").unwrap();
        let b = normalized_hash(b"fn foo").unwrap();
        let c = normalized_hash(b"fn bar").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn hash_is_object_id_of_cbor_bytes() {
        let stripped = b"hello";
        let expected = ObjectId::of(&Bytes::from(stripped.as_slice())).unwrap();
        assert_eq!(normalized_hash(stripped).unwrap(), expected);
    }

    #[test]
    fn empty_stripped_is_stable() {
        let a = normalized_hash(b"").unwrap();
        let b = normalized_hash(&[]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, ObjectId::of(&Bytes::default()).unwrap());
    }
}
