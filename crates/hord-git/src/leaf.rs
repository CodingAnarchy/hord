//! Git modes of [`ModedBlob`](hord_core::ModedBlob) leaves (ADR 0042).
//!
//! Hord [`TreeEntry`](hord_core::TreeEntry) has no mode field. Two git trees
//! with the same file bytes but different modes (`chmod +x`, symlink, gitlink)
//! would otherwise share a [`ObjectId`](hord_core::ObjectId). Export then
//! cannot tell them apart. Non-default modes are stored as a
//! [`ModedBlob`](hord_core::ModedBlob) so the tree id itself changes. `100644`
//! blobs stay plain [`Blob`](hord_core::Blob)s.

use gix::objs::tree::{EntryKind, EntryMode};
use hord_core::FileMode;

/// The [`FileMode`] of a git leaf of kind `kind`; `None` for a tree.
pub(crate) fn file_mode(kind: EntryKind) -> Option<FileMode> {
    match kind {
        EntryKind::Blob => Some(FileMode::Regular),
        EntryKind::BlobExecutable => Some(FileMode::Executable),
        EntryKind::Link => Some(FileMode::Symlink),
        EntryKind::Commit => Some(FileMode::Gitlink),
        EntryKind::Tree => None,
    }
}

/// Parse a git mode octal as stored in [`ModedBlob::mode`](hord_core::ModedBlob::mode).
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
    use super::*;

    fn octal(kind: EntryKind) -> String {
        let mode: EntryMode = kind.into();
        let mut buf = [0u8; 6];
        let bytes: &[u8] = mode.as_bytes(&mut buf).as_ref();
        std::str::from_utf8(bytes)
            .expect("an entry mode is ASCII octal")
            .to_owned()
    }

    #[test]
    fn parse_round_trips_kind_octals() -> Result<(), Box<dyn std::error::Error>> {
        for kind in [
            EntryKind::Tree,
            EntryKind::Blob,
            EntryKind::BlobExecutable,
            EntryKind::Link,
            EntryKind::Commit,
        ] {
            let s = octal(kind);
            let parsed = parse_mode(&s).ok_or(format!("parse {s}"))?;
            assert_eq!(parsed.kind(), kind, "{s}");
        }
        Ok(())
    }
}
