//! Append-only pack files with a CBOR offset index (spec §8.1).
//!
//! Each pack is a magic header followed by concatenated per-object zstd frames.
//! A sidecar `.idx` file stores [`PackIndex`] as canonical CBOR so the objects
//! table in redb is rebuildable.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use hord_core::ObjectId;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Eight-byte magic + version tag written at the start of every pack file.
pub(crate) const MAGIC: &[u8; 8] = b"HORDPAK1";

/// zstd compression level (default). Per-kind dictionaries are deferred.
const COMPRESSION_LEVEL: i32 = 3;

/// One object's location inside a pack.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackEntry {
    pub id: ObjectId,
    pub offset: u64,
    pub compressed_len: u32,
}

/// Sidecar offset index for one pack file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackIndex {
    pub pack: u64,
    pub entries: Vec<PackEntry>,
}

/// Location of a packed object, stored in the `objects` redb table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackedLocation {
    pub pack: u64,
    pub offset: u64,
    pub compressed_len: u32,
}

pub(crate) fn compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    zstd::encode_all(bytes, COMPRESSION_LEVEL)
}

pub(crate) fn decompress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    zstd::decode_all(bytes)
}

pub(crate) fn pack_file_name(pack_id: u64) -> String {
    format!("pack-{pack_id:016x}.pack")
}

pub(crate) fn index_file_name(pack_id: u64) -> String {
    format!("pack-{pack_id:016x}.idx")
}

pub(crate) fn pack_path(pack_dir: &Path, pack_id: u64) -> PathBuf {
    pack_dir.join(pack_file_name(pack_id))
}

pub(crate) fn index_path(pack_dir: &Path, pack_id: u64) -> PathBuf {
    pack_dir.join(index_file_name(pack_id))
}

/// Write `bytes` to `path` via a same-directory rename.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} has no parent", path.display()),
        )
    })?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        ulid::Ulid::generate()
    ));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Streaming writer for a new pack file and its sidecar index.
pub(crate) struct PackWriter {
    file: Option<File>,
    tmp_path: PathBuf,
    final_path: PathBuf,
    pack_id: u64,
    entries: Vec<PackEntry>,
}

impl PackWriter {
    pub(crate) fn create(pack_dir: &Path, pack_id: u64) -> Result<Self, Error> {
        fs::create_dir_all(pack_dir)?;
        let final_path = pack_path(pack_dir, pack_id);
        let tmp_path = pack_dir.join(format!(
            ".{}.{}.tmp",
            pack_file_name(pack_id),
            ulid::Ulid::generate()
        ));
        let mut file = File::create(&tmp_path)?;
        file.write_all(MAGIC)?;
        Ok(Self {
            file: Some(file),
            tmp_path,
            final_path,
            pack_id,
            entries: Vec::new(),
        })
    }

    fn file(&mut self) -> Result<&mut File, Error> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("pack writer is closed").into())
    }

    pub(crate) fn add(
        &mut self,
        id: ObjectId,
        uncompressed: &[u8],
    ) -> Result<PackedLocation, Error> {
        let compressed = compress(uncompressed)?;
        let compressed_len = u32::try_from(compressed.len()).map_err(|_| Error::ObjectTooLarge)?;
        let offset = self.file()?.stream_position()?;
        self.file()?.write_all(&compressed)?;
        self.entries.push(PackEntry {
            id,
            offset,
            compressed_len,
        });
        Ok(PackedLocation {
            pack: self.pack_id,
            offset,
            compressed_len,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fsync, rename into place, and write the sidecar index. Returns the index.
    pub(crate) fn finish(mut self, pack_dir: &Path) -> Result<PackIndex, Error> {
        if let Some(file) = self.file.as_mut() {
            file.sync_all()?;
        }
        self.file.take();
        fs::rename(&self.tmp_path, &self.final_path)?;
        let index = PackIndex {
            pack: self.pack_id,
            entries: std::mem::take(&mut self.entries),
        };
        let bytes = hord_encoding::encode(&index)?;
        atomic_write(&index_path(pack_dir, self.pack_id), &bytes)?;
        Ok(index)
    }
}

impl Drop for PackWriter {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.tmp_path);
        }
    }
}

pub(crate) fn read_packed(pack_path: &Path, loc: &PackedLocation) -> Result<Vec<u8>, Error> {
    let mut file = File::open(pack_path)?;
    file.seek(SeekFrom::Start(loc.offset))?;
    let mut buf = vec![0u8; loc.compressed_len as usize];
    file.read_exact(&mut buf)?;
    Ok(decompress(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn compress_round_trip() {
        let src = b"the same inputs produce the same ObjectId";
        let c = compress(src).unwrap();
        assert_ne!(c, src);
        assert_eq!(decompress(&c).unwrap(), src);
    }

    #[test]
    fn pack_writer_round_trip() {
        let dir = env::temp_dir().join(format!(
            "hord-pack-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        fs::create_dir_all(&dir).unwrap();
        let id = ObjectId::from_canonical(b"abc");
        let mut writer = PackWriter::create(&dir, 1).unwrap();
        let loc = writer.add(id, b"abc").unwrap();
        writer.finish(&dir).unwrap();
        let got = read_packed(&pack_path(&dir, 1), &loc).unwrap();
        assert_eq!(got, b"abc");
        let idx_bytes = fs::read(index_path(&dir, 1)).unwrap();
        let idx: PackIndex = hord_encoding::decode(&idx_bytes).unwrap();
        assert_eq!(idx.pack, 1);
        assert_eq!(idx.entries.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
