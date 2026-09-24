//! Git-bridge leaf encoding: a Blob wrapper that carries the git mode.
//!
//! Hord [`TreeEntry`](hord_core::TreeEntry) has no mode field. Two git trees
//! with the same file bytes but different modes (`chmod +x`, symlink, gitlink)
//! would otherwise share a [`ObjectId`](hord_core::ObjectId). Export then
//! cannot tell them apart. Non-default modes are stored as [`GitLeaf`] so the
//! tree id itself changes. `100644` blobs stay plain [`Blob`](hord_core::Blob)s.

use gix::objs::tree::EntryMode;
use hord_core::ObjectId;
use serde::{Deserialize, Serialize};

/// Git mode octal for a regular non-executable file.
pub(crate) const MODE_BLOB: &str = "100644";

/// A git tree leaf whose mode is not [`MODE_BLOB`].
///
/// `blob` is the [`ObjectId`](hord_core::ObjectId) of a [`hord_core::Blob`]
/// holding the file bytes (or, for gitlinks, the hex SHA of the target commit).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) struct GitLeaf {
    /// Git tree-entry mode in the on-wire octal form (`100755`, `120000`, `160000`, …).
    pub mode: String,
    /// Inner [`hord_core::Blob`] object.
    pub blob: ObjectId,
}

/// Parse a git mode octal as stored in [`GitLeaf::mode`].
pub(crate) fn parse_mode(octal: &str) -> Option<EntryMode> {
    // `EntryMode::from_bytes` is built to parse the tree encoding, which has a
    // trailing space after the digits.
    let mut buf = Vec::with_capacity(octal.len() + 1);
    buf.extend_from_slice(octal.as_bytes());
    buf.push(b' ');
    EntryMode::from_bytes(&buf)
}

#[cfg(test)]
mod tests {
    use gix::objs::tree::EntryKind;

    use super::*;

    fn octal(kind: EntryKind) -> String {
        let mode: EntryMode = kind.into();
        let mut buf = [0u8; 6];
        let bytes: &[u8] = mode.as_bytes(&mut buf).as_ref();
        std::str::from_utf8(bytes).unwrap().to_owned()
    }

    #[test]
    fn parse_round_trips_kind_octals() {
        for kind in [
            EntryKind::Tree,
            EntryKind::Blob,
            EntryKind::BlobExecutable,
            EntryKind::Link,
            EntryKind::Commit,
        ] {
            let s = octal(kind);
            let parsed = parse_mode(&s).unwrap_or_else(|| panic!("parse {s}"));
            assert_eq!(parsed.kind(), kind, "{s}");
        }
    }
}
