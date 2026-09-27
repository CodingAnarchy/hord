//! Raw-byte objects and file modes (spec §3.2, ADR 0042).

use serde::{Deserialize, Serialize};

use crate::{Bytes, ObjectId};

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

/// How a file is checked out, as git records it (ADR 0042).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum FileMode {
    /// A regular file (`100644`).
    #[default]
    Regular,
    /// An executable file (`100755`).
    Executable,
    /// A symbolic link (`120000`): the blob holds the link target.
    Symlink,
    /// A submodule (`160000`): the blob holds the target commit's hex SHA.
    Gitlink,
}

impl FileMode {
    /// Git's octal for this mode.
    #[must_use]
    pub fn octal(self) -> &'static str {
        match self {
            Self::Regular => "100644",
            Self::Executable => "100755",
            Self::Symlink => "120000",
            Self::Gitlink => "160000",
        }
    }

    /// Whether the blob holds the file's contents: a regular or an
    /// executable file, not a symlink's target or a gitlink's commit.
    #[must_use]
    pub fn holds_contents(self) -> bool {
        matches!(self, Self::Regular | Self::Executable)
    }

    /// The mode git's octal `octal` names, if it is one of these.
    #[must_use]
    pub fn from_octal(octal: &str) -> Option<Self> {
        [
            Self::Regular,
            Self::Executable,
            Self::Symlink,
            Self::Gitlink,
        ]
        .into_iter()
        .find(|mode| mode.octal() == octal)
    }
}

/// A file whose mode is not [`FileMode::Regular`]: what a
/// [`TreeEntry::Blob`](crate::TreeEntry::Blob) names for an executable, a
/// symlink, or a gitlink (ADR 0042). A regular file's entry names its
/// [`Blob`] directly, so a mode change changes the entry's id.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ModedBlob {
    /// Git's octal for the mode ([`FileMode::octal`]), as git writes it in
    /// a tree.
    pub mode: String,
    /// The [`Blob`] holding the file's bytes (a symlink's target, a
    /// gitlink's hex SHA).
    pub blob: ObjectId,
}

impl ModedBlob {
    /// Wrap `blob` with `mode`. `None` for [`FileMode::Regular`], whose
    /// entry is the blob itself.
    #[must_use]
    pub fn new(mode: FileMode, blob: ObjectId) -> Option<Self> {
        (mode != FileMode::Regular).then(|| Self {
            mode: mode.octal().to_owned(),
            blob,
        })
    }

    /// The stored mode, if it is a known one.
    #[must_use]
    pub fn file_mode(&self) -> Option<FileMode> {
        FileMode::from_octal(&self.mode)
    }
}

/// What a [`TreeEntry::Blob`](crate::TreeEntry::Blob) names, decoded: a
/// regular file's [`Blob`], or a [`ModedBlob`] (ADR 0042).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileEntry {
    /// A regular file.
    Blob(Blob),
    /// Any other mode, and the id of the blob with its bytes.
    Moded(ModedBlob),
}

impl FileEntry {
    /// Decode the object bytes a file entry names. `Err` when they are
    /// neither a [`Blob`] nor a [`ModedBlob`] (the [`Blob`] error is
    /// returned).
    pub fn decode(bytes: &[u8]) -> Result<Self, hord_encoding::Error> {
        match hord_encoding::decode::<Blob>(bytes) {
            Ok(blob) => Ok(Self::Blob(blob)),
            Err(err) => match hord_encoding::decode::<ModedBlob>(bytes) {
                Ok(moded) => Ok(Self::Moded(moded)),
                Err(_) => Err(err),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octals_round_trip() {
        for mode in [
            FileMode::Regular,
            FileMode::Executable,
            FileMode::Symlink,
            FileMode::Gitlink,
        ] {
            assert_eq!(FileMode::from_octal(mode.octal()), Some(mode));
        }
        assert_eq!(FileMode::from_octal("040000"), None);
    }

    #[test]
    fn a_file_entry_decodes_either_shape() -> Result<(), Box<dyn std::error::Error>> {
        let blob = Blob::new(b"target".to_vec());
        let id = ObjectId::of(&blob)?;
        let moded = ModedBlob::new(FileMode::Symlink, id).ok_or("symlink is moded")?;
        assert_eq!(
            FileEntry::decode(&hord_encoding::encode(&blob)?)?,
            FileEntry::Blob(blob)
        );
        assert_eq!(
            FileEntry::decode(&hord_encoding::encode(&moded)?)?,
            FileEntry::Moded(moded)
        );
        assert!(ModedBlob::new(FileMode::Regular, id).is_none());
        assert!(FileEntry::decode(&hord_encoding::encode(&id)?).is_err());
        Ok(())
    }
}
