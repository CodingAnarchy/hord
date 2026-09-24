//! Content and identity identifiers (spec §3.1).

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

use crate::error::Error;
use crate::{ObjectId, RepoPath};

/// A snapshot is identified by the [`ObjectId`] of its [`crate::Snapshot`]
/// object, which names the root tree and the identity tree (ADR 0017,
/// superseding spec §3.1's root-tree id). "Same content" is equal
/// [`crate::Snapshot::tree`]s.
pub type SnapshotId = ObjectId;

/// A change record is identified by its own [`ObjectId`].
pub type ChangeId = ObjectId;

/// Stable identity of a code node across edits.
///
/// Assigned once and carried forward. A birth id is derived from the
/// definition's content, file, site, and the base snapshot of the change that
/// creates it (ADR 0019, superseding spec §3.1's random ULID), so it has no
/// timestamp component. It is displayed and encoded in ULID text
/// (26-character Crockford Base32).
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NodeId(u128);

impl NodeId {
    /// Generate a new ULID using the current time and random bits.
    ///
    /// This is non-deterministic by design. Do not call it while constructing a
    /// value whose [`ObjectId`](crate::ObjectId) must be stable.
    #[must_use]
    pub fn generate() -> Self {
        Self(Ulid::generate().0)
    }

    /// The nil ULID (all bits zero).
    #[must_use]
    pub const fn nil() -> Self {
        Self(0)
    }

    /// Wrap a raw 128-bit ULID.
    #[must_use]
    pub const fn from_u128(id: u128) -> Self {
        Self(id)
    }

    /// The inner 128-bit value.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    /// Id of the CST root of the file at `path` (ADR 0015).
    ///
    /// The root is not a definition, but top-level `Insert`/`Move` ops need a
    /// parent and a `Replace` of file-level glue needs a node. Blob-tier files
    /// and whole-file reads and writes use the same id. It is derived from
    /// canonical CBOR of `("hord/file", path components)`, so every file has
    /// its own and the same path always gets the same one. It never equals
    /// [`NodeId::nil`]. A renamed file gets a new root id.
    #[must_use]
    pub fn file_root(path: &RepoPath) -> Self {
        // Canonical CBOR of (domain, components) is injective, so distinct
        // paths hash distinct inputs. Encoding a tuple of strings cannot fail.
        let id = ObjectId::of(&("hord/file", path.components()))
            .unwrap_or_else(|_| ObjectId::from_canonical(b"hord/file"));
        let mut high = [0u8; 16];
        high.copy_from_slice(&id.as_bytes()[..16]);
        let value = u128::from_be_bytes(high);
        Self(if value == 0 { 1 } else { value })
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
        f.write_str(Ulid(self.0).array_to_str(&mut buf))
    }
}

impl FromStr for NodeId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s)
            .map(|id| Self(id.0))
            .map_err(|e| Error::NodeId(format!("{s}: {e}")))
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; ulid::ULID_LEN];
        serializer.serialize_str(Ulid(self.0).array_to_str(&mut buf))
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
    fn file_roots_are_stable_distinct_and_not_nil() {
        let a: RepoPath = "src/lib.rs".parse().unwrap();
        let b: RepoPath = "src/main.rs".parse().unwrap();
        assert_eq!(NodeId::file_root(&a), NodeId::file_root(&a.clone()));
        assert_ne!(NodeId::file_root(&a), NodeId::file_root(&b));
        assert_ne!(NodeId::file_root(&a), NodeId::nil());
        assert_ne!(NodeId::file_root(&RepoPath::default()), NodeId::nil());
    }

    #[test]
    fn nil_is_all_zeros() {
        let id = NodeId::nil();
        assert_eq!(id.to_string(), "00000000000000000000000000");
        assert_eq!(id.as_u128(), 0);
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
