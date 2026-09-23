//! Append-only pack files with a CBOR offset index (spec §8.1).
//!
//! Each pack is a magic header followed by concatenated per-object zstd frames.
//! A sidecar `.idx` file stores [`PackIndex`] as canonical CBOR so the objects
//! table in redb is rebuildable.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use hord_core::ObjectId;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Eight-byte magic + version tag written at the start of every pack file.
pub(crate) const MAGIC: &[u8; 8] = b"HORDPAK1";

/// zstd compression level (default). Per-kind dictionaries are deferred.
const COMPRESSION_LEVEL: i32 = 3;

/// Give up growing the decompress buffer past this. Matches the 4 GiB object cap.
const MAX_DECOMPRESSED: usize = u32::MAX as usize;

thread_local! {
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackedLocation {
    pub pack: u64,
    pub offset: u64,
    pub compressed_len: u32,
}

pub(crate) fn compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    zstd::encode_all(bytes, COMPRESSION_LEVEL)
}

pub(crate) fn decompress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    DECOMPRESSOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new()?);
        }
        let decompressor = slot.as_mut().expect("decompressor was just installed");
        // Each frame is independent. Guess from the compressed size and grow
        // until the frame fits, reusing the same zstd context.
        let mut capacity = bytes.len().saturating_mul(3).max(256);
        loop {
            match decompressor.decompress(bytes, capacity) {
                Ok(out) => return Ok(out),
                Err(err) if output_too_small(&err) => {
                    let next = capacity.saturating_mul(2);
                    if next <= capacity || next > MAX_DECOMPRESSED {
                        return Err(err);
                    }
                    capacity = next;
                }
                Err(err) => return Err(err),
            }
        }
    })
}

fn output_too_small(err: &io::Error) -> bool {
    err.to_string().contains("too small")
}

/// Read `buf.len()` bytes at `offset` without changing the file cursor.
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut read = 0;
        while read < buf.len() {
            let n = file.seek_read(
                &mut buf[read..],
                offset + u64::try_from(read).unwrap_or(u64::MAX),
            )?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short pack read",
                ));
            }
            read += n;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional pack reads are unavailable on this platform",
        ))
    }
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
    file: Option<BufWriter<File>>,
    /// Next byte offset. Tracked here so buffering does not require a seek.
    offset: u64,
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
        let mut file = BufWriter::with_capacity(256 * 1024, File::create(&tmp_path)?);
        file.write_all(MAGIC)?;
        Ok(Self {
            file: Some(file),
            offset: MAGIC.len() as u64,
            tmp_path,
            final_path,
            pack_id,
            entries: Vec::new(),
        })
    }

    fn file(&mut self) -> Result<&mut BufWriter<File>, Error> {
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
        let offset = self.offset;
        self.file()?.write_all(&compressed)?;
        self.offset = self
            .offset
            .checked_add(u64::from(compressed_len))
            .ok_or(Error::ObjectTooLarge)?;
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
            file.flush()?;
            file.get_mut().sync_all()?;
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

pub(crate) fn read_packed_file(file: &File, loc: &PackedLocation) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; loc.compressed_len as usize];
    read_exact_at(file, &mut buf, loc.offset)?;
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
        assert_eq!(compress(src).unwrap(), c);
        assert_eq!(decompress(&c).unwrap(), src);

        let zeros = vec![0u8; 100_000];
        let compressed = compress(&zeros).unwrap();
        assert!(compressed.len() < zeros.len());
        assert_eq!(decompress(&compressed).unwrap(), zeros);
        assert_eq!(compress(&zeros).unwrap(), compressed);

        let empty = compress(b"").unwrap();
        assert_eq!(decompress(&empty).unwrap(), b"");
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
        let file = File::open(pack_path(&dir, 1)).unwrap();
        let got = read_packed_file(&file, &loc).unwrap();
        assert_eq!(got, b"abc");
        let idx_bytes = fs::read(index_path(&dir, 1)).unwrap();
        let idx: PackIndex = hord_encoding::decode(&idx_bytes).unwrap();
        assert_eq!(idx.pack, 1);
        assert_eq!(idx.entries.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
