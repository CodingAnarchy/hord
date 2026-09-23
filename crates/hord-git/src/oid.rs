//! Git object ids (SHA-1 hex).

use std::fmt;
use std::str::FromStr;

use crate::Error;

/// Hex-encoded git object name (SHA-1).
///
/// This is a git SHA, not a Hord [`ObjectId`](hord_core::ObjectId). Import
/// records it as [`IntentRef::GitCommit`](hord_core::IntentRef::GitCommit).
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GitOid(gix::ObjectId);

impl GitOid {
    /// Wrap a gix object id.
    #[must_use]
    pub fn from_gix(id: gix::ObjectId) -> Self {
        Self(id)
    }

    /// The inner gix object id.
    #[must_use]
    pub fn as_gix(&self) -> gix::ObjectId {
        self.0
    }

    /// Lowercase hex encoding.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

impl From<gix::ObjectId> for GitOid {
    fn from(id: gix::ObjectId) -> Self {
        Self(id)
    }
}

impl From<GitOid> for gix::ObjectId {
    fn from(id: GitOid) -> Self {
        id.0
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("GitOid").field(&self.to_hex()).finish()
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.to_hex(), f)
    }
}

impl FromStr for GitOid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        gix::ObjectId::from_hex(s.as_bytes())
            .map(Self)
            .map_err(|e| Error::Git(format!("invalid git oid {s:?}: {e}")))
    }
}
