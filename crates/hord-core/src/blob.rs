//! Raw-byte objects (spec §3.2).

use serde::{Deserialize, Serialize};

use crate::Bytes;

/// Raw bytes for files with no adapter (images, lockfiles, unknown languages).
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Blob {
    /// File contents. Encoded as a CBOR byte string.
    pub bytes: Bytes,
}

impl Blob {
    /// Wrap raw file bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }
}
