//! Trivia-stripped semantic identity (spec §3.3 `normalized`).

use hord_core::ObjectId;

use crate::ParseError;

/// Hash of a leaf's trivia-stripped token text (spec §3.3).
///
/// **Strip:** drop every attached leading and trailing trivia byte (comments,
/// whitespace, blank lines) as produced by [`crate::attach_trivia`].
///
/// **Hash:** the stripped byte string is wrapped as [`hord_core::Bytes`] (CBOR major
/// type 2) and hashed with [`ObjectId::of`] — BLAKE3-256 of canonical CBOR
/// (RFC 8949 §4.2.1). Kind, language, `name`, and `raw` are not part of this
/// hash.
///
/// Internal nodes do not use this. Their `normalized` id hashes the
/// children's `normalized` ids, in order (ADR 0008). A whitespace-only change
/// therefore alters a leaf's `raw` and content [`ObjectId`], and every
/// ancestor's content id, but no `normalized` id.
#[must_use]
pub fn normalized_hash(stripped: &[u8]) -> ObjectId {
    ObjectId::of_byte_string(stripped)
}

/// `normalized` for an internal node: BLAKE3 of the canonical CBOR array of
/// `children`'s `normalized` ids, in child order (ADR 0008).
pub(crate) fn normalized_of_children(children: &[ObjectId]) -> Result<ObjectId, ParseError> {
    Ok(ObjectId::of_ordered(children)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hord_core::Bytes;

    #[test]
    fn same_stripped_bytes_same_hash() {
        let a = normalized_hash(b"fn foo");
        let b = normalized_hash(b"fn foo");
        let c = normalized_hash(b"fn bar");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn hash_is_object_id_of_cbor_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let stripped = b"hello";
        let expected = ObjectId::of(&Bytes::from(stripped.as_slice()))?;
        assert_eq!(normalized_hash(stripped), expected);
        Ok(())
    }

    #[test]
    fn empty_stripped_is_stable() -> Result<(), Box<dyn std::error::Error>> {
        let a = normalized_hash(b"");
        let b = normalized_hash(&[]);
        assert_eq!(a, b);
        assert_eq!(a, ObjectId::of(&Bytes::default())?);
        Ok(())
    }

    #[test]
    fn child_order_changes_internal_normalized() -> Result<(), Box<dyn std::error::Error>> {
        let a = ObjectId::from_bytes([1; 32]);
        let b = ObjectId::from_bytes([2; 32]);
        let left = normalized_of_children(&[a, b])?;
        let right = normalized_of_children(&[b, a])?;
        let again = normalized_of_children(&[a, b])?;
        assert_eq!(left, again);
        assert_ne!(left, right);
        Ok(())
    }
}
