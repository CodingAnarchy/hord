//! [`Store`]: content-addressed objects, log, refs, and workspaces.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use hord_core::{ChangeId, ObjectId, SnapshotId};
use redb::{Database, Durability, ReadableTable, TableDefinition, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::pack::{PackWriter, PackedLocation, pack_path, read_packed};
use crate::workspace::WorkspaceRow;
use crate::{Error, Result, WorkspaceId, WorkspaceMeta};

/// Directory name of a Hord store, next to the repository root (spec §8.1).
pub const HORD_DIR: &str = ".hord";

/// Persist buffered refs after this many `set_ref` calls without an intervening
/// [`Store::append_log`], [`Store::set_head`], [`Store::pack`], or [`Store::flush`].
const REF_FLUSH_BATCH: usize = 4096;
/// Persist buffered log entries in one redb transaction. Git import is one
/// `append_log` per commit; fsyncing each one caps throughput well below 200/s.
const LOG_FLUSH_BATCH: usize = 4096;

const LOG: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("log");
const REFS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("refs");
const WORKSPACES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("workspaces");
const OBJECTS: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("objects");
const META: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("meta");
const NODE_HISTORY: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("node_history");
const EVIDENCE_BY_SNAPSHOT: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("evidence_by_snapshot");

const META_HEAD: &str = "head";
const META_NEXT_PACK: &str = "next_pack";

/// Local content-addressed object store (spec §8.1).
///
/// Layout under `<repo>/.hord/`:
/// - `index.redb` — log, refs, workspaces, packed-object locations
/// - `objects/<ab>/<rest>` — uncompressed loose objects
/// - `objects/pack/pack-<id>.pack` + `.idx` — zstd-compressed packs
/// - `ws/<ulid>/` — workspace materialization directories
///
/// [`Store::put`] writes loose objects and does not pack. Spec §8.1 packs
/// recently written data with a background job; call [`Store::pack`] for that.
pub struct Store {
    repo_root: PathBuf,
    hord_dir: PathBuf,
    objects_dir: PathBuf,
    db: Database,
    pack_lock: Mutex<()>,
    /// Refs waiting for the next write transaction (import does `set_ref` per
    /// tree; flushing on log batch / [`Store::set_head`] / [`Store::flush`]).
    pending_refs: Mutex<HashMap<String, ObjectId>>,
    /// Landed changes not yet written to the redb `log` table.
    pending_log: Mutex<Vec<ChangeId>>,
    has_packs: AtomicBool,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("repo_root", &self.repo_root)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Create `<repo>/.hord/` and an empty store.
    ///
    /// Fails if `.hord/` already exists.
    pub fn create(repo_root: impl AsRef<Path>) -> Result<Self> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let hord_dir = repo_root.join(HORD_DIR);
        if hord_dir.exists() {
            return Err(Error::AlreadyExists(hord_dir));
        }
        fs::create_dir_all(&hord_dir)?;
        let objects_dir = hord_dir.join("objects");
        create_shard_dirs(&objects_dir)?;
        fs::create_dir_all(hord_dir.join("ws"))?;
        let db = Database::create(hord_dir.join("index.redb")).map_err(Error::index)?;
        init_tables(&db)?;
        Ok(Self {
            repo_root,
            hord_dir,
            objects_dir,
            db,
            pack_lock: Mutex::new(()),
            pending_refs: Mutex::new(HashMap::new()),
            pending_log: Mutex::new(Vec::new()),
            has_packs: AtomicBool::new(false),
        })
    }

    /// Open an existing store at `<repo>/.hord/`.
    pub fn open(repo_root: impl AsRef<Path>) -> Result<Self> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let hord_dir = repo_root.join(HORD_DIR);
        let index = hord_dir.join("index.redb");
        if !hord_dir.is_dir() || !index.is_file() {
            return Err(Error::MissingStore(hord_dir));
        }
        let db = Database::open(index).map_err(Error::index)?;
        let objects_dir = hord_dir.join("objects");
        let has_packs = pack_dir_has_packs(&objects_dir.join("pack"));
        Ok(Self {
            repo_root,
            hord_dir,
            objects_dir,
            db,
            pack_lock: Mutex::new(()),
            pending_refs: Mutex::new(HashMap::new()),
            pending_log: Mutex::new(Vec::new()),
            has_packs: AtomicBool::new(has_packs),
        })
    }

    /// Repository root this store was opened against.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Absolute path of `.hord/`.
    #[must_use]
    pub fn hord_dir(&self) -> &Path {
        &self.hord_dir
    }

    /// Store already-canonical CBOR bytes. The [`ObjectId`] is BLAKE3-256 of
    /// `canonical_cbor` (spec §3.1, §3.9).
    ///
    /// Writes a loose object. Identical bytes are idempotent (the file is
    /// rewritten with the same content; there is no existence check, since a
    /// stat costs about as much as the write). Does not pack; call
    /// [`Store::pack`] (spec §8.1: packing is a background job).
    pub fn put(&self, canonical_cbor: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::from_canonical(canonical_cbor);
        self.write_loose(id, canonical_cbor)?;
        Ok(id)
    }

    /// Load the canonical bytes for `id`.
    pub fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        if let Some(bytes) = self.read_loose(id)? {
            return ensure_id(id, bytes);
        }
        if self.has_packs.load(Ordering::Relaxed)
            && let Some(loc) = self.packed_location(id)?
        {
            let bytes = read_packed(&pack_path(&self.pack_dir(), loc.pack), &loc)?;
            return ensure_id(id, bytes);
        }
        Err(Error::MissingObject(id))
    }

    /// Whether `id` is present as a loose or packed object.
    pub fn contains(&self, id: ObjectId) -> Result<bool> {
        if self.loose_path(id).is_file() {
            return Ok(true);
        }
        Ok(self.has_packs.load(Ordering::Relaxed) && self.packed_location(id)?.is_some())
    }

    /// Canonical-encode `value` and [`put`](Self::put) the bytes.
    pub fn put_object<T: Serialize>(&self, value: &T) -> Result<ObjectId> {
        let bytes = hord_encoding::encode(value)?;
        self.put(&bytes)
    }

    /// [`get`](Self::get) and decode a typed object.
    pub fn get_object<T: DeserializeOwned>(&self, id: ObjectId) -> Result<T> {
        let bytes = self.get(id)?;
        Ok(hord_encoding::decode(&bytes)?)
    }

    /// Append `change` to the landing log (spec §3.7). Order is landing order.
    ///
    /// Buffered until a few thousand entries, [`Store::set_head`],
    /// [`Store::pack`], [`Store::flush`], or drop. [`Store::log`] includes
    /// entries that have not been flushed yet.
    pub fn append_log(&self, change: ChangeId) -> Result<()> {
        let flush = {
            let mut log = lock_vec(&self.pending_log);
            log.push(change);
            log.len() >= LOG_FLUSH_BATCH
        };
        if flush {
            self.persist_pending(Durability::None)?;
        }
        Ok(())
    }

    /// Landed [`ChangeId`]s in landing order.
    pub fn log(&self) -> Result<Vec<ChangeId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(LOG).map_err(Error::index)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(Error::index)? {
            let (_, v) = entry.map_err(Error::index)?;
            out.push(object_id_from_value(v.value())?);
        }
        out.extend_from_slice(&lock_vec(&self.pending_log));
        Ok(out)
    }

    /// Set `head` to the latest landed change (spec §3.7).
    ///
    /// Flushes buffered refs and log entries in the same durable write.
    pub fn set_head(&self, change: ChangeId) -> Result<()> {
        let mut refs = lock_map(&self.pending_refs);
        let mut log = lock_vec(&self.pending_log);
        self.persist_locked(&mut refs, &mut log, Durability::Immediate, Some(change))
    }

    /// Current `head`, if any change has landed.
    pub fn head(&self) -> Result<Option<ChangeId>> {
        match self.meta_get(META_HEAD)? {
            Some(bytes) => Ok(Some(object_id_from_value(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Point a named ref at `id` (spec §3.7). Names such as `main` and
    /// `release/1.2` are allowed; the empty string and names containing NUL
    /// are not.
    pub fn set_ref(&self, name: &str, id: ObjectId) -> Result<()> {
        validate_ref_name(name)?;
        let mut pending = lock_map(&self.pending_refs);
        pending.insert(name.to_owned(), id);
        let flush = pending.len() >= REF_FLUSH_BATCH;
        drop(pending);
        if flush {
            self.persist_pending(Durability::None)?;
        }
        Ok(())
    }

    /// Resolve a named ref.
    pub fn get_ref(&self, name: &str) -> Result<Option<ObjectId>> {
        validate_ref_name(name)?;
        {
            let pending = lock_map(&self.pending_refs);
            if let Some(id) = pending.get(name) {
                return Ok(Some(*id));
            }
        }
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(REFS).map_err(Error::index)?;
        match table.get(name).map_err(Error::index)? {
            Some(v) => Ok(Some(object_id_from_value(v.value())?)),
            None => Ok(None),
        }
    }

    /// Persist buffered refs and log entries to the redb index.
    ///
    /// [`Store::set_ref`] and [`Store::append_log`] batch into the next
    /// [`Store::set_head`], [`Store::pack`], this call, or drop. Drop and this
    /// method use durable commits; ingest batches do not fsync.
    pub fn flush(&self) -> Result<()> {
        self.persist_pending(Durability::Immediate)
    }

    fn persist_pending(&self, durability: Durability) -> Result<()> {
        let mut refs = lock_map(&self.pending_refs);
        let mut log = lock_vec(&self.pending_log);
        self.persist_locked(&mut refs, &mut log, durability, None)
    }

    fn persist_locked(
        &self,
        refs: &mut HashMap<String, ObjectId>,
        log: &mut Vec<ChangeId>,
        durability: Durability,
        head: Option<ChangeId>,
    ) -> Result<()> {
        if refs.is_empty() && log.is_empty() && head.is_none() {
            return Ok(());
        }
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(durability);
        write_pending_refs(&txn, refs)?;
        write_pending_log(&txn, log)?;
        if let Some(change) = head {
            let mut meta = txn.open_table(META).map_err(Error::index)?;
            meta.insert(META_HEAD, change.as_bytes().as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        refs.clear();
        log.clear();
        Ok(())
    }

    /// Create a workspace overlay on `base` and its materialization directory
    /// at `.hord/ws/<id>/`.
    pub fn create_workspace(&self, base: SnapshotId) -> Result<WorkspaceMeta> {
        let id = WorkspaceId::generate();
        let path = self.workspace_dir(id);
        fs::create_dir_all(&path)?;
        let row = WorkspaceRow { id, base };
        let bytes = hord_encoding::encode(&row)?;
        let key = id.to_string();
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(WORKSPACES).map_err(Error::index)?;
            table
                .insert(key.as_str(), bytes.as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        Ok(WorkspaceMeta { id, base, path })
    }

    /// Workspaces in ULID (creation-time) order.
    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceMeta>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(WORKSPACES).map_err(Error::index)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(Error::index)? {
            let (_, v) = entry.map_err(Error::index)?;
            let row: WorkspaceRow = hord_encoding::decode(v.value())?;
            let path = self.workspace_dir(row.id);
            out.push(WorkspaceMeta {
                id: row.id,
                base: row.base,
                path,
            });
        }
        Ok(out)
    }

    /// Look up one workspace by id.
    pub fn get_workspace(&self, id: WorkspaceId) -> Result<Option<WorkspaceMeta>> {
        let key = id.to_string();
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(WORKSPACES).map_err(Error::index)?;
        match table.get(key.as_str()).map_err(Error::index)? {
            Some(v) => {
                let row: WorkspaceRow = hord_encoding::decode(v.value())?;
                Ok(Some(WorkspaceMeta {
                    id: row.id,
                    base: row.base,
                    path: self.workspace_dir(row.id),
                }))
            }
            None => Ok(None),
        }
    }

    /// Pack all current loose objects into a new zstd pack file with a sidecar
    /// offset index. Returns the number of objects packed.
    ///
    /// Also persists any buffered [`Store::set_ref`] calls.
    pub fn pack(&self) -> Result<usize> {
        self.flush()?;
        let _guard = self.pack_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.pack_inner()
    }

    fn pack_inner(&self) -> Result<usize> {
        let loose = self.list_loose()?;
        if loose.is_empty() {
            return Ok(0);
        }
        let pack_id = self.next_pack_id()?;
        let pack_dir = self.pack_dir();
        let mut writer = PackWriter::create(&pack_dir, pack_id)?;
        let mut locations: Vec<(ObjectId, PackedLocation)> = Vec::new();
        let mut packed_paths: Vec<PathBuf> = Vec::new();
        for (id, path) in &loose {
            let bytes = fs::read(path)?;
            let loc = writer.add(*id, &bytes)?;
            locations.push((*id, loc));
            packed_paths.push(path.clone());
        }
        if writer.is_empty() {
            return Ok(0);
        }
        writer.finish(&pack_dir)?;
        let n = locations.len();
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut objects = txn.open_table(OBJECTS).map_err(Error::index)?;
            for (id, loc) in &locations {
                let encoded = hord_encoding::encode(loc)?;
                objects
                    .insert(id.as_bytes().as_slice(), encoded.as_slice())
                    .map_err(Error::index)?;
            }
            let mut meta = txn.open_table(META).map_err(Error::index)?;
            let next = (pack_id + 1).to_be_bytes();
            meta.insert(META_NEXT_PACK, next.as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        for path in packed_paths {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        self.has_packs.store(true, Ordering::Relaxed);
        Ok(n)
    }

    fn write_loose(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        let path = self.loose_path(id);
        match fs::write(&path, bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, bytes)?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn read_loose(&self, id: ObjectId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.loose_path(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn loose_path(&self, id: ObjectId) -> PathBuf {
        let hex = hex_encode(id.as_bytes());
        let mut path = PathBuf::with_capacity(self.objects_dir.as_os_str().len() + hex.len() + 2);
        path.push(&self.objects_dir);
        path.push(std::str::from_utf8(&hex[..2]).expect("hex is ascii"));
        path.push(std::str::from_utf8(&hex[2..]).expect("hex is ascii"));
        path
    }

    fn pack_dir(&self) -> PathBuf {
        self.objects_dir.join("pack")
    }

    fn workspace_dir(&self, id: WorkspaceId) -> PathBuf {
        self.hord_dir.join("ws").join(id.to_string())
    }

    fn packed_location(&self, id: ObjectId) -> Result<Option<PackedLocation>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(OBJECTS).map_err(Error::index)?;
        match table.get(id.as_bytes().as_slice()).map_err(Error::index)? {
            Some(v) => Ok(Some(hord_encoding::decode(v.value())?)),
            None => Ok(None),
        }
    }

    fn next_pack_id(&self) -> Result<u64> {
        match self.meta_get(META_NEXT_PACK)? {
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::CorruptIndex("next_pack"))?;
                Ok(u64::from_be_bytes(arr))
            }
            None => Ok(1),
        }
    }

    fn meta_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(META).map_err(Error::index)?;
        match table.get(key).map_err(Error::index)? {
            Some(v) => Ok(Some(v.value().to_vec())),
            None => Ok(None),
        }
    }

    fn list_loose(&self) -> Result<Vec<(ObjectId, PathBuf)>> {
        let root = &self.objects_dir;
        let mut out = Vec::new();
        let shards = match fs::read_dir(root) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for shard in shards {
            let shard = shard?;
            if shard.file_name() == "pack" {
                continue;
            }
            if !shard.file_type()?.is_dir() {
                continue;
            }
            let shard_name = shard.file_name();
            let Some(shard_str) = shard_name.to_str() else {
                continue;
            };
            if shard_str.len() != 2 {
                continue;
            }
            for file in fs::read_dir(shard.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let name = file.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if name_str.starts_with('.') || name_str.ends_with(".tmp") {
                    continue;
                }
                let hex = format!("{shard_str}{name_str}");
                let id: ObjectId = hex.parse()?;
                out.push((id, file.path()));
            }
        }
        Ok(out)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn init_tables(db: &Database) -> Result<()> {
    let txn = db.begin_write().map_err(Error::index)?;
    txn.open_table(LOG).map_err(Error::index)?;
    txn.open_table(REFS).map_err(Error::index)?;
    txn.open_table(WORKSPACES).map_err(Error::index)?;
    txn.open_table(OBJECTS).map_err(Error::index)?;
    txn.open_table(META).map_err(Error::index)?;
    txn.open_table(NODE_HISTORY).map_err(Error::index)?;
    txn.open_table(EVIDENCE_BY_SNAPSHOT).map_err(Error::index)?;
    txn.commit().map_err(Error::index)?;
    Ok(())
}

fn object_id_from_value(bytes: &[u8]) -> Result<ObjectId> {
    ObjectId::try_from(bytes).map_err(Error::from)
}

fn ensure_id(id: ObjectId, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let got = ObjectId::from_canonical(&bytes);
    if got != id {
        return Err(Error::Corrupt {
            id,
            reason: format!("hash is {got}"),
        });
    }
    Ok(bytes)
}

fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('\0') {
        return Err(Error::InvalidRef(name.to_owned()));
    }
    Ok(())
}

fn lock_map(
    mutex: &Mutex<HashMap<String, ObjectId>>,
) -> std::sync::MutexGuard<'_, HashMap<String, ObjectId>> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_pending_refs(txn: &WriteTransaction, pending: &HashMap<String, ObjectId>) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let mut table = txn.open_table(REFS).map_err(Error::index)?;
    for (name, id) in pending {
        table
            .insert(name.as_str(), id.as_bytes().as_slice())
            .map_err(Error::index)?;
    }
    Ok(())
}

fn write_pending_log(txn: &WriteTransaction, pending: &[ChangeId]) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let mut table = txn.open_table(LOG).map_err(Error::index)?;
    let start = match table.last().map_err(Error::index)? {
        Some((k, _)) => k.value() + 1,
        None => 0,
    };
    for (offset, id) in pending.iter().enumerate() {
        let key = start + offset as u64;
        table
            .insert(&key, id.as_bytes().as_slice())
            .map_err(Error::index)?;
    }
    Ok(())
}

fn lock_vec(mutex: &Mutex<Vec<ChangeId>>) -> std::sync::MutexGuard<'_, Vec<ChangeId>> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn create_shard_dirs(objects_dir: &Path) -> Result<()> {
    fs::create_dir_all(objects_dir.join("pack"))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &a in HEX {
        for &b in HEX {
            let shard = [a, b];
            fs::create_dir_all(
                objects_dir.join(std::str::from_utf8(&shard).expect("hex is ascii")),
            )?;
        }
    }
    Ok(())
}

fn pack_dir_has_packs(pack_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return false;
    };
    entries.filter_map(std::result::Result::ok).any(|e| {
        e.path()
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pack"))
    })
}

fn hex_encode(bytes: &[u8; ObjectId::LEN]) -> [u8; ObjectId::LEN * 2] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; ObjectId::LEN * 2];
    for (i, &b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    out
}
