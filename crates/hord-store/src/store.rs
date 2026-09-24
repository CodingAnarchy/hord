//! [`Store`]: content-addressed objects, log, refs, and workspaces.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hord_core::{ChangeId, ObjectId, SnapshotId};
use redb::{Database, Durability, ReadableTable, TableDefinition, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::pack::{self, PackWriter, PackedLocation, pack_path};
use crate::workspace::WorkspaceRow;
use crate::{Error, Result, WorkspaceId, WorkspaceMeta};

#[path = "index.rs"]
mod index;
#[path = "lock.rs"]
mod lock;
#[path = "queue.rs"]
mod queue;

pub use index::{EdgeKind, touched_nodes};
pub use queue::Landing;

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
const EVIDENCE_BY_SNAPSHOT: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("evidence_by_snapshot");

const META_HEAD: &str = "head";
const META_NEXT_PACK: &str = "next_pack";

/// Where this process last observed an object. Not stored on disk.
#[derive(Clone, Copy)]
enum Resident {
    Loose,
    Packed(PackedLocation),
}

/// In-memory copy of the landing log.
///
/// `loaded` is false until the first [`Store::log`] or index read. Appends
/// update it only after that, under the same lock as `pending_log` (acquire
/// `pending_log` first). Callers that hold this lock must not take `pending_log`.
struct LandingLog {
    loaded: bool,
    order: Vec<ChangeId>,
    /// First landing index of each id. A repeated id keeps the earlier index.
    first_pos: HashMap<ChangeId, usize>,
    /// Last landing index of each id.
    last_pos: HashMap<ChangeId, usize>,
}

impl LandingLog {
    fn empty() -> Self {
        Self {
            loaded: false,
            order: Vec::new(),
            first_pos: HashMap::new(),
            last_pos: HashMap::new(),
        }
    }

    fn note(&mut self, change: ChangeId) {
        if !self.loaded {
            return;
        }
        let pos = self.order.len();
        self.first_pos.entry(change).or_insert(pos);
        self.last_pos.insert(change, pos);
        self.order.push(change);
    }
}

/// Local content-addressed object store (spec §8.1).
///
/// Layout under `<repo>/.hord/`:
/// - `index.redb` — format version, log, refs, workspaces, packed-object
///   locations, and the rebuildable `node_history` / `edges` / `rebased`
///   indexes
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
    /// Serializes index updates (edges, history, landings, rebuilds).
    index_lock: Mutex<()>,
    /// Objects this process has stored or fetched. Duplicate `put`s hit this
    /// instead of rewriting the loose file.
    resident: Mutex<HashMap<ObjectId, Resident>>,
    /// Open pack files for positional reads. Pack ids are append-only. Reads
    /// clone the handle and release the lock before touching the file.
    pack_files: Mutex<HashMap<u64, Arc<File>>>,
    landing_log: Mutex<LandingLog>,
    /// Edge keys inserted by this process. Duplicate `put_edge` skips the write.
    seen_edges: Mutex<HashSet<[u8; index::EDGE_KEY_LEN]>>,
    /// Set once the queue name index is known to be complete.
    queue_names_ready: AtomicBool,
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
    /// Fails if `.hord/` already exists. Waits for the index lock as
    /// [`Store::open`] does.
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
        let index = hord_dir.join("index.redb");
        let db = lock::acquire(&hord_dir, &index, lock::timeout_from_env()?, || {
            Database::create(&index)
        })?;
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
            index_lock: Mutex::new(()),
            resident: Mutex::new(HashMap::new()),
            pack_files: Mutex::new(HashMap::new()),
            landing_log: Mutex::new(LandingLog::empty()),
            seen_edges: Mutex::new(HashSet::new()),
            queue_names_ready: AtomicBool::new(false),
        })
    }

    /// Open an existing store at `<repo>/.hord/`.
    ///
    /// One process at a time can hold a store (ADR 0021). If another process
    /// holds it, this retries with backoff for `HORD_LOCK_TIMEOUT` seconds
    /// (default 30; `0` fails at once), then returns [`Error::Locked`].
    pub fn open(repo_root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_lock_timeout(repo_root, lock::timeout_from_env()?)
    }

    /// [`Store::open`], waiting at most `timeout` for another process to
    /// release the store instead of reading `HORD_LOCK_TIMEOUT`.
    pub fn open_with_lock_timeout(repo_root: impl AsRef<Path>, timeout: Duration) -> Result<Self> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let hord_dir = repo_root.join(HORD_DIR);
        let index = hord_dir.join("index.redb");
        if !hord_dir.is_dir() || !index.is_file() {
            return Err(Error::MissingStore(hord_dir));
        }
        let db = lock::acquire(&hord_dir, &index, timeout, || Database::open(&index))?;
        index::ensure_tables(&db)?;
        queue::ensure_tables(&db)?;
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
            index_lock: Mutex::new(()),
            resident: Mutex::new(HashMap::new()),
            pack_files: Mutex::new(HashMap::new()),
            landing_log: Mutex::new(LandingLog::empty()),
            seen_edges: Mutex::new(HashSet::new()),
            queue_names_ready: AtomicBool::new(false),
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
    /// Writes a loose object. Identical bytes are idempotent. An object this
    /// process has already stored, loose or packed, is not written again.
    /// Does not pack; call [`Store::pack`] (spec §8.1: packing is a background
    /// job).
    pub fn put(&self, canonical_cbor: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::from_canonical(canonical_cbor);
        if self.remembered(id).is_some() {
            return Ok(id);
        }
        self.write_loose(id, canonical_cbor)?;
        self.remember(id, Resident::Loose);
        Ok(id)
    }

    /// Load the canonical bytes for `id`.
    pub fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        match self.remembered(id) {
            Some(Resident::Loose) => {
                if let Some(bytes) = self.read_loose(id)? {
                    return ensure_id(id, bytes);
                }
                // Packed since we last saw it. The loose file is gone, so go
                // straight to the pack index.
                self.forget(id);
            }
            Some(Resident::Packed(loc)) => {
                return ensure_id(id, self.read_pack_bytes(&loc)?);
            }
            None => {
                if let Some(bytes) = self.read_loose(id)? {
                    let bytes = ensure_id(id, bytes)?;
                    self.remember(id, Resident::Loose);
                    return Ok(bytes);
                }
            }
        }
        if self.has_packs.load(Ordering::Relaxed)
            && let Some(loc) = self.packed_location(id)?
        {
            let bytes = ensure_id(id, self.read_pack_bytes(&loc)?)?;
            self.remember(id, Resident::Packed(loc));
            return Ok(bytes);
        }
        Err(Error::MissingObject(id))
    }

    /// Whether `id` is present as a loose or packed object.
    pub fn contains(&self, id: ObjectId) -> Result<bool> {
        if self.remembered(id).is_some() {
            return Ok(true);
        }
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
            lock(&self.landing_log).note(change);
            log.len() >= LOG_FLUSH_BATCH
        };
        if flush {
            self.persist_pending(Durability::None)?;
        }
        Ok(())
    }

    /// Landed [`ChangeId`]s in landing order.
    pub fn log(&self) -> Result<Vec<ChangeId>> {
        let log = self.ensure_landing_log()?;
        Ok(log.order.clone())
    }

    /// Number of entries in the landing log.
    pub fn log_len(&self) -> Result<usize> {
        Ok(self.ensure_landing_log()?.order.len())
    }

    /// Landing-log entries from index `start` on (empty past the end).
    pub fn log_since(&self, start: usize) -> Result<Vec<ChangeId>> {
        let log = self.ensure_landing_log()?;
        Ok(log
            .order
            .get(start..)
            .map(<[_]>::to_vec)
            .unwrap_or_default())
    }

    /// Index of the last landing-log entry equal to `change`, if any.
    pub fn log_position(&self, change: ChangeId) -> Result<Option<usize>> {
        Ok(self.ensure_landing_log()?.last_pos.get(&change).copied())
    }

    /// Whether `change` is in the landing log.
    pub fn log_contains(&self, change: ChangeId) -> Result<bool> {
        Ok(self.ensure_landing_log()?.first_pos.contains_key(&change))
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

    /// Delete a workspace's row and its materialization directory. Returns
    /// whether the row existed. Files next to the directory that share its
    /// name plus an extension (`<id>.stat`) are removed too.
    pub fn remove_workspace(&self, id: WorkspaceId) -> Result<bool> {
        let key = id.to_string();
        let txn = self.db.begin_write().map_err(Error::index)?;
        let existed = {
            let mut table = txn.open_table(WORKSPACES).map_err(Error::index)?;
            table.remove(key.as_str()).map_err(Error::index)?.is_some()
        };
        txn.commit().map_err(Error::index)?;
        let dir = self.workspace_dir(id);
        remove_tree(&dir)?;
        let ws_root = self.hord_dir.join("ws");
        if let Ok(entries) = fs::read_dir(&ws_root) {
            let prefix = format!("{key}.");
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    remove_tree(&entry.path())?;
                }
            }
        }
        Ok(existed)
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
        let mut locations: Vec<(ObjectId, PackedLocation)> = Vec::with_capacity(loose.len());
        let mut packed_paths: Vec<PathBuf> = Vec::with_capacity(loose.len());
        for (id, path) in loose {
            // A pack is immutable and replaces the loose file, so never pack
            // bytes that do not hash to their name (e.g. a torn write).
            let bytes = ensure_id(id, fs::read(&path)?)?;
            let loc = writer.add(id, &bytes)?;
            locations.push((id, loc));
            packed_paths.push(path);
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
        self.has_packs.store(true, Ordering::Relaxed);
        {
            let mut resident = lock(&self.resident);
            for (id, loc) in &locations {
                resident.insert(*id, Resident::Packed(*loc));
            }
        }
        for path in packed_paths {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
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

    fn remembered(&self, id: ObjectId) -> Option<Resident> {
        lock(&self.resident).get(&id).copied()
    }

    fn remember(&self, id: ObjectId, resident: Resident) {
        lock(&self.resident).insert(id, resident);
    }

    fn forget(&self, id: ObjectId) {
        lock(&self.resident).remove(&id);
    }

    fn read_pack_bytes(&self, loc: &PackedLocation) -> Result<Vec<u8>> {
        let file = {
            let mut files = lock(&self.pack_files);
            match files.entry(loc.pack) {
                Entry::Vacant(slot) => Arc::clone(
                    slot.insert(Arc::new(File::open(pack_path(&self.pack_dir(), loc.pack))?)),
                ),
                Entry::Occupied(slot) => Arc::clone(slot.get()),
            }
        };
        // Positional reads do not share a cursor, so concurrent readers of the
        // same pack need no lock while they read and decompress.
        pack::read_packed_file(&file, loc)
    }

    fn read_persisted_log(&self) -> Result<Vec<ChangeId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(LOG).map_err(Error::index)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(Error::index)? {
            let (_, value) = entry.map_err(Error::index)?;
            out.push(object_id_from_value(value.value())?);
        }
        Ok(out)
    }

    fn ensure_landing_log(&self) -> Result<std::sync::MutexGuard<'_, LandingLog>> {
        {
            let log = lock(&self.landing_log);
            if log.loaded {
                return Ok(log);
            }
        }
        let pending = lock_vec(&self.pending_log);
        let mut log = lock(&self.landing_log);
        if !log.loaded {
            let mut order = self.read_persisted_log()?;
            order.extend_from_slice(&pending);
            let mut first_pos = HashMap::with_capacity(order.len());
            let mut last_pos = HashMap::with_capacity(order.len());
            for (index, id) in order.iter().enumerate() {
                first_pos.entry(*id).or_insert(index);
                last_pos.insert(*id, index);
            }
            *log = LandingLog {
                loaded: true,
                order,
                first_pos,
                last_pos,
            };
        }
        drop(pending);
        Ok(log)
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

    /// Visit every stored object. A loose file wins over a packed copy of the
    /// same id. Callers must not open a write transaction on this store from `f`.
    fn for_each_stored_object(
        &self,
        mut f: impl FnMut(ObjectId, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for (id, path) in self.list_loose()? {
            if !seen.insert(id) {
                continue;
            }
            let bytes = ensure_id(id, fs::read(&path)?)?;
            self.remember(id, Resident::Loose);
            f(id, &bytes)?;
        }
        // Drop the read transaction before `f` runs so a callback can write.
        for (id, loc) in self.list_packed_locations()? {
            if !seen.insert(id) {
                continue;
            }
            let bytes = ensure_id(id, self.read_pack_bytes(&loc)?)?;
            self.remember(id, Resident::Packed(loc));
            f(id, &bytes)?;
        }
        Ok(())
    }

    fn list_packed_locations(&self) -> Result<Vec<(ObjectId, PackedLocation)>> {
        if !self.has_packs.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(OBJECTS).map_err(Error::index)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(Error::index)? {
            let (key, value) = entry.map_err(Error::index)?;
            let id = object_id_from_value(key.value())?;
            let loc = hord_encoding::decode(value.value())?;
            out.push((id, loc));
        }
        Ok(out)
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
                let id = object_id_from_loose_name(shard_str, name_str)?;
                out.push((id, file.path()));
            }
        }
        Ok(out)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.flush();
        // Before `db` closes, so no later holder has written its pid yet.
        lock::release(&self.hord_dir);
    }
}

/// Remove a file or directory tree, making read-only entries writable first
/// (a copy-on-write checkout may carry read-only mode bits). Missing is fine.
fn remove_tree(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.is_dir() {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(path, perms)?;
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn init_tables(db: &Database) -> Result<()> {
    let txn = db.begin_write().map_err(Error::index)?;
    txn.open_table(LOG).map_err(Error::index)?;
    txn.open_table(REFS).map_err(Error::index)?;
    txn.open_table(WORKSPACES).map_err(Error::index)?;
    txn.open_table(OBJECTS).map_err(Error::index)?;
    txn.open_table(META).map_err(Error::index)?;
    txn.open_table(EVIDENCE_BY_SNAPSHOT).map_err(Error::index)?;
    index::open_tables(&txn)?;
    index::write_format(&txn)?;
    queue::open_tables(&txn)?;
    queue::mark_names_indexed(&txn)?;
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn object_id_from_loose_name(shard: &str, name: &str) -> Result<ObjectId> {
    let shard_bytes = shard.as_bytes();
    let name_bytes = name.as_bytes();
    let mut hex = [0u8; ObjectId::LEN * 2];
    if shard_bytes.len() + name_bytes.len() != hex.len() {
        let combined = format!("{shard}{name}");
        return combined.parse().map_err(Error::from);
    }
    hex[..shard_bytes.len()].copy_from_slice(shard_bytes);
    hex[shard_bytes.len()..].copy_from_slice(name_bytes);
    // Both pieces came from `OsStr::to_str`, so their concatenation is UTF-8.
    let text = std::str::from_utf8(&hex).expect("loose object name is utf-8");
    text.parse().map_err(Error::from)
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
