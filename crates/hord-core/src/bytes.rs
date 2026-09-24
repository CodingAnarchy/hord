//! CBOR byte strings (major type 2).

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Exact bytes of a blob or node subtree.
///
/// Serializes as a CBOR byte string (major type 2). Serde's default `Vec<u8>`
/// encoding is an array of integers and must not be used for hashed objects.
///
/// Stored as [`Arc<[u8]>`] so cloning a [`crate::Node`] does not copy `raw`.
/// The wire form is unchanged.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Bytes(Arc<[u8]>);

impl Default for Bytes {
    fn default() -> Self {
        Self(Arc::from([] as [u8; 0]))
    }
}

impl Bytes {
    /// Wrap owned bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(Arc::from(bytes.into().into_boxed_slice()))
    }

    /// Borrow the bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume and return the inner bytes as a vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Arc::from(bytes.into_boxed_slice()))
    }
}

impl From<&[u8]> for Bytes {
    fn from(bytes: &[u8]) -> Self {
        Self(Arc::from(bytes))
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Bytes").field(&HexBytes(&self.0)).finish()
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;

        impl Visitor<'_> for BytesVisitor {
            type Value = Bytes;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a CBOR byte string (major type 2)")
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(Bytes(Arc::from(v)))
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(Bytes(Arc::from(v.into_boxed_slice())))
            }
        }

        deserializer.deserialize_bytes(BytesVisitor)
    }
}

struct HexBytes<'a>(&'a [u8]);

impl fmt::Debug for HexBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hord_encoding::{decode, encode};

    #[test]
    fn encodes_as_cbor_major_type_2() -> Result<(), Box<dyn std::error::Error>> {
        let bytes = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);
        let encoded = encode(&bytes)?;
        assert_eq!(encoded[0], 0x44, "major type 2, 4-byte payload");
        assert_eq!(&encoded[1..], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(decode::<Bytes>(&encoded)?, bytes);
        Ok(())
    }

    #[test]
    fn empty_is_one_byte() -> Result<(), Box<dyn std::error::Error>> {
        let encoded = encode(&Bytes::new(Vec::new()))?;
        assert_eq!(encoded, [0x40]);
        Ok(())
    }

    #[test]
    fn clone_does_not_copy_payload() {
        let a = Bytes::from(vec![1, 2, 3, 4]);
        let b = a.clone();
        assert!(std::ptr::eq(a.as_slice(), b.as_slice()));
    }
}
