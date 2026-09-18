//! Workspace identifiers and on-disk metadata (spec §6.1, M0 subset).

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

use crate::Error;
use hord_core::SnapshotId;

/// Stable identifier of a workspace. Encoded as a ULID (26-character Crockford
/// Base32).
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WorkspaceId(Ulid);

impl WorkspaceId {
    /// Generate a new ULID using the current time and random bits.
    ///
    /// This is non-deterministic by design. Workspace records are not hashed
    /// into an [`hord_core::ObjectId`].
    #[must_use]
    pub fn generate() -> Self {
        Self(Ulid::generate())
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
}

impl fmt::Debug for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WorkspaceId")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for WorkspaceId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s)
            .map(Self)
            .map_err(|e| Error::WorkspaceId(format!("{s}: {e}")))
    }
}

impl Serialize for WorkspaceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct WorkspaceIdVisitor;

        impl Visitor<'_> for WorkspaceIdVisitor {
            type Value = WorkspaceId;

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

        deserializer.deserialize_str(WorkspaceIdVisitor)
    }
}

/// A working overlay: snapshot pointer plus a materialization directory.
///
/// Full workspace state (actor, access log) lives in `hord-txn` (M3). This is
/// the M0 subset CLI `ws new` / `ws list` need.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMeta {
    /// Workspace id (ULID).
    pub id: WorkspaceId,
    /// Snapshot this workspace was created against.
    pub base: SnapshotId,
    /// Absolute path of the materialization directory (`.hord/ws/<id>/`).
    pub path: PathBuf,
}

/// Row stored in the `workspaces` redb table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkspaceRow {
    pub id: WorkspaceId,
    pub base: SnapshotId,
}
