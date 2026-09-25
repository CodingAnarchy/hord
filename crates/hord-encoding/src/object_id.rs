//! Content-addressed object identity.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, encode};

/// Content hash: BLAKE3-256 over the canonical encoding of an object (spec §3.1).
///
/// The same inputs produce the same [`ObjectId`] on every machine and run.
/// Non-determinism here is a P0 bug.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ObjectId([u8; Self::LEN]);

impl ObjectId {
    /// Size of an [`ObjectId`] in bytes (BLAKE3-256).
    pub const LEN: usize = 32;

    /// Hash already-canonical CBOR bytes.
    ///
    /// Callers must pass the output of [`encode`](crate::encode) (or equivalent
    /// RFC 8949 §4.2.1 bytes). This does not re-canonicalize.
    #[must_use]
    pub fn from_canonical(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Canonical-encode `value` and hash the result.
    pub fn of<T: Serialize + ?Sized>(value: &T) -> Result<Self, Error> {
        Ok(Self::from_canonical(&encode(value)?))
    }

    /// Hash `value`, whose `Serialize` already emits canonical form, without
    /// buffering it as a CBOR value and sorting its keys.
    ///
    /// Equal to [`Self::of`] under the conditions of
    /// [`encode_ordered_into`](crate::encode_ordered_into). The encoder
    /// streams into BLAKE3, so nothing is allocated.
    pub fn of_ordered<T: Serialize + ?Sized>(value: &T) -> Result<Self, Error> {
        let mut hasher = blake3::Hasher::new();
        cbor2::to_writer(value, &mut hasher).map_err(|e| Error::Encode(e.to_string()))?;
        Ok(Self(*hasher.finalize().as_bytes()))
    }

    /// Hash `bytes` as a CBOR byte string (major type 2), without copying
    /// them into an owned value first. Equal to `ObjectId::of` of any
    /// `Serialize` type that calls `serialize_bytes` with the same slice.
    #[must_use]
    pub fn of_byte_string(bytes: &[u8]) -> Self {
        struct ByteString<'a>(&'a [u8]);
        impl Serialize for ByteString<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_bytes(self.0)
            }
        }
        Self::of_ordered(&ByteString(bytes)).expect("a byte string always encodes")
    }

    /// Wrap a raw 32-byte digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    /// Lowercase hex encoding (64 characters).
    #[must_use]
    pub fn to_hex(&self) -> String {
        let hex = hex_encode(&self.0);
        hex_str(&hex).to_owned()
    }
}

impl AsRef<[u8]> for ObjectId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8; ObjectId::LEN]> for ObjectId {
    fn as_ref(&self) -> &[u8; ObjectId::LEN] {
        &self.0
    }
}

impl From<[u8; ObjectId::LEN]> for ObjectId {
    fn from(bytes: [u8; ObjectId::LEN]) -> Self {
        Self(bytes)
    }
}

impl TryFrom<&[u8]> for ObjectId {
    type Error = Error;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let bytes: [u8; Self::LEN] = bytes.try_into().map_err(|_| Error::ObjectIdLength {
            expected: Self::LEN,
            actual: bytes.len(),
        })?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = hex_encode(&self.0);
        f.debug_tuple("ObjectId").field(&hex_str(&hex)).finish()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(hex_str(&hex_encode(&self.0)))
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != Self::LEN * 2 {
            return Err(Error::ObjectIdHex(format!(
                "expected {} hex chars, got {}",
                Self::LEN * 2,
                s.len()
            )));
        }
        let mut bytes = [0u8; Self::LEN];
        let (chunks, rest) = s.as_bytes().as_chunks::<2>();
        debug_assert!(rest.is_empty());
        for (i, chunk) in chunks.iter().enumerate() {
            bytes[i] = hex_byte(chunk[0], chunk[1])
                .map_err(|c| Error::ObjectIdHex(format!("invalid hex digit {c:?} in {s:?}")))?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectIdVisitor;

        impl Visitor<'_> for ObjectIdVisitor {
            type Value = ObjectId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 32-byte CBOR byte string")
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                ObjectId::try_from(v).map_err(E::custom)
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                self.visit_bytes(&v)
            }
        }

        deserializer.deserialize_bytes(ObjectIdVisitor)
    }
}

/// Lowercase hex on the stack, so `Display` does not allocate.
fn hex_encode(bytes: &[u8; ObjectId::LEN]) -> [u8; ObjectId::LEN * 2] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; ObjectId::LEN * 2];
    for (i, &b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    out
}

fn hex_str(hex: &[u8; ObjectId::LEN * 2]) -> &str {
    std::str::from_utf8(hex).expect("hex digits are ascii")
}

fn hex_byte(hi: u8, lo: u8) -> Result<u8, char> {
    Ok((hex_nibble(hi)? << 4) | hex_nibble(lo)?)
}

fn hex_nibble(c: u8) -> Result<u8, char> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(c as char),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let id = ObjectId::from_canonical(&[0x00]);
        let parsed: ObjectId = id.to_hex().parse()?;
        assert_eq!(id, parsed);
        Ok(())
    }

    #[test]
    fn rejects_wrong_hex_length() {
        assert!("deadbeef".parse::<ObjectId>().is_err());
    }

    #[test]
    fn serializes_as_32_byte_cbor_string() -> Result<(), Box<dyn std::error::Error>> {
        let id = ObjectId::from_canonical(&[0x00]);
        let bytes = crate::encode(&id)?;
        assert_eq!(bytes[0], 0x58, "major type 2, one-byte length");
        assert_eq!(bytes[1], ObjectId::LEN as u8);
        assert_eq!(&bytes[2..], id.as_bytes().as_slice());
        assert_eq!(crate::decode::<ObjectId>(&bytes)?, id);
        Ok(())
    }
}
