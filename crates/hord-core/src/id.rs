//! Content and identity identifiers (spec §3.1).

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

use crate::ObjectId;
use crate::error::Error;

/// A snapshot is identified by the [`ObjectId`] of its root tree object.
pub type SnapshotId = ObjectId;

/// A change record is identified by its own [`ObjectId`].
pub type ChangeId = ObjectId;

/// Stable identity of a code node across edits.
///
/// Assigned once and carried forward. Encoded as a ULID (26-character Crockford
/// Base32). The timestamp component is informational only (spec §3.1).
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NodeId(Ulid);

impl NodeId {
    /// Size of a [`NodeId`] in bytes (ULID / 128-bit).
    pub const LEN: usize = 16;

    /// Generate a new ULID using the current time and random bits.
    ///
    /// This is non-deterministic by design. Do not call it while constructing a
    /// value whose [`ObjectId`](crate::ObjectId) must be stable.
    #[must_use]
    pub fn generate() -> Self {
        Self(Ulid::generate())
    }

    /// The nil ULID (all bits zero).
    #[must_use]
    pub const fn nil() -> Self {
        Self(Ulid::nil())
    }

    /// Wrap a raw 128-bit ULID.
    #[must_use]
    pub const fn from_u128(id: u128) -> Self {
        Self(Ulid(id))
    }

    /// The inner 128-bit value.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0.0
    }

    /// Wrap a ULID.
    #[must_use]
    pub const fn from_ulid(ulid: Ulid) -> Self {
        Self(ulid)
    }

    /// The inner ULID.
    #[must_use]
    pub const fn as_ulid(self) -> Ulid {
        self.0
    }

    /// Wrap 16 big-endian bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(Ulid::from_bytes(bytes))
    }

    /// 16 big-endian bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; Self::LEN] {
        self.0.to_bytes()
    }

    /// ULID timestamp in milliseconds since the Unix epoch.
    ///
    /// Informational only; identity does not depend on this value being a real
    /// birth time.
    #[must_use]
    pub const fn timestamp_ms(self) -> u64 {
        self.0.timestamp_ms()
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::nil()
    }
}

impl From<Ulid> for NodeId {
    fn from(ulid: Ulid) -> Self {
        Self(ulid)
    }
}

impl From<NodeId> for Ulid {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

impl From<u128> for NodeId {
    fn from(id: u128) -> Self {
        Self::from_u128(id)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("NodeId").field(&self.to_string()).finish()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; ulid::ULID_LEN];
        f.write_str(self.0.array_to_str(&mut buf))
    }
}

impl FromStr for NodeId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s)
            .map(Self)
            .map_err(|e| Error::NodeId(format!("{s}: {e}")))
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; ulid::ULID_LEN];
        serializer.serialize_str(self.0.array_to_str(&mut buf))
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NodeIdVisitor;

        impl Visitor<'_> for NodeIdVisitor {
            type Value = NodeId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 26-character ULID string")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(E::custom)
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                self.visit_str(&v)
            }
        }

        deserializer.deserialize_str(NodeIdVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hord_encoding::{decode, encode};

    const EXAMPLE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn parse_display_round_trip() {
        let id: NodeId = EXAMPLE.parse().unwrap();
        assert_eq!(id.to_string(), EXAMPLE);
        assert_eq!(format!("{id}"), EXAMPLE);
    }

    #[test]
    fn nil_is_all_zeros() {
        let id = NodeId::nil();
        assert_eq!(id.to_string(), "00000000000000000000000000");
        assert_eq!(id.as_u128(), 0);
        assert_eq!(id.timestamp_ms(), 0);
    }

    #[test]
    fn rejects_invalid_ulid() {
        let err = "not-a-ulid".parse::<NodeId>().unwrap_err();
        assert!(matches!(err, Error::NodeId(_)));
        assert!("01ARZ3NDEKTSV4RRFFQ69G5FA".parse::<NodeId>().is_err());
    }

    #[test]
    fn serde_is_ulid_text() {
        let id: NodeId = EXAMPLE.parse().unwrap();
        let bytes = encode(&id).unwrap();
        assert_eq!(bytes[0], 0x78, "major type 3, one-byte length");
        assert_eq!(bytes[1], ulid::ULID_LEN as u8);
        assert_eq!(&bytes[2..], EXAMPLE.as_bytes());
        assert_eq!(decode::<NodeId>(&bytes).unwrap(), id);
    }

    #[test]
    fn generate_is_parseable() {
        let id = NodeId::generate();
        let parsed: NodeId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
        assert_ne!(id, NodeId::nil());
    }
}
