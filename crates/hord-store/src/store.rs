//! [`Store`]: content-addressed objects, log, refs, and workspaces.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_core::{ChangeId, ObjectId, SnapshotId};
use redb::{Database, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::pack::{PackWriter, PackedLocation, atomic_write, pack_path, read_packed};
use crate::workspace::WorkspaceRow;
use crate::{Error, Result, WorkspaceId, WorkspaceMeta};

/// Directory name of a Hord store, next to the repository root (spec §8.1).
pub const HORD_DIR: &str = ".hord";

/// Pack loose objects once this many have been written since the last pack.
const PACK_THRESHOLD: u64 = 512;

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
pub struct Store {
    repo_root: PathBuf,
    hord_dir: PathBuf,
    db: Database,
    pack_lock: Mutex<()>,
    loose_since_pack: AtomicU64,
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
        fs::create_dir_all(hord_dir.join("objects").join("pack"))?;
        fs::create_dir_all(hord_dir.join("ws"))?;
        let db = Database::create(hord_dir.join("index.redb")).map_err(Error::index)?;
        init_tables(&db)?;
        Ok(Self {
            repo_root,
            hord_dir,
            db,
            pack_lock: Mutex::new(()),
            loose_since_pack: AtomicU64::new(0),
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
        Ok(Self {
            repo_root,
            hord_dir,
            db,
            pack_lock: Mutex::new(()),
            loose_since_pack: AtomicU64::new(0),
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
    /// Writes a loose object. Identical bytes are idempotent.
    pub fn put(&self, canonical_cbor: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::from_canonical(canonical_cbor);
        if self.contains(id)? {
            return Ok(id);
        }
        self.write_loose(id, canonical_cbor)?;
        self.loose_since_pack.fetch_add(1, Ordering::Relaxed);
        self.maybe_pack()?;
        Ok(id)
    }

    /// Load the canonical bytes for `id`.
    pub fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        if let Some(bytes) = self.read_loose(id)? {
            return ensure_id(id, bytes);
        }
        if let Some(loc) = self.packed_location(id)? {
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
        Ok(self.packed_location(id)?.is_some())
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
    pub fn append_log(&self, change: ChangeId) -> Result<()> {
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(LOG).map_err(Error::index)?;
            let next = match table.last().map_err(Error::index)? {
                Some((k, _)) => k.value() + 1,
                None => 0,
            };
            table
                .insert(&next, change.as_bytes().as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
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
        Ok(out)
    }

    /// Set `head` to the latest landed change (spec §3.7).
    pub fn set_head(&self, change: ChangeId) -> Result<()> {
        self.meta_set(META_HEAD, change.as_bytes().as_slice())
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
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(REFS).map_err(Error::index)?;
            table
                .insert(name, id.as_bytes().as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    /// Resolve a named ref.
    pub fn get_ref(&self, name: &str) -> Result<Option<ObjectId>> {
        validate_ref_name(name)?;
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(REFS).map_err(Error::index)?;
        match table.get(name).map_err(Error::index)? {
            Some(v) => Ok(Some(object_id_from_value(v.value())?)),
            None => Ok(None),
        }
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
    pub fn pack(&self) -> Result<usize> {
        let _guard = self.pack_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.pack_inner()
    }

    fn maybe_pack(&self) -> Result<()> {
        if self.loose_since_pack.load(Ordering::Relaxed) < PACK_THRESHOLD {
            return Ok(());
        }
        if let Ok(_guard) = self.pack_lock.try_lock() {
            self.pack_inner()?;
        }
        Ok(())
    }

    fn pack_inner(&self) -> Result<usize> {
        let loose = self.list_loose()?;
        if loose.is_empty() {
            self.loose_since_pack.store(0, Ordering::Relaxed);
            return Ok(0);
        }
        let pack_id = self.next_pack_id()?;
        let pack_dir = self.pack_dir();
        let mut writer = PackWriter::create(&pack_dir, pack_id)?;
        let mut locations: Vec<(ObjectId, PackedLocation)> = Vec::new();
        let mut packed_paths: Vec<PathBuf> = Vec::new();
        for (id, path) in &loose {
            if self.packed_location(*id)?.is_some() {
                let _ = fs::remove_file(path);
                continue;
            }
            let bytes = fs::read(path)?;
            let loc = writer.add(*id, &bytes)?;
            locations.push((*id, loc));
            packed_paths.push(path.clone());
        }
        if writer.is_empty() {
            self.loose_since_pack.store(0, Ordering::Relaxed);
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
        self.loose_since_pack.store(0, Ordering::Relaxed);
        Ok(n)
    }

    fn write_loose(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        let path = self.loose_path(id);
        if path.is_file() {
            return Ok(());
        }
        atomic_write(&path, bytes)
    }

    fn read_loose(&self, id: ObjectId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.loose_path(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn loose_path(&self, id: ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.hord_dir
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
    }

    fn pack_dir(&self) -> PathBuf {
        self.hord_dir.join("objects").join("pack")
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

    fn meta_set(&self, key: &str, value: &[u8]) -> Result<()> {
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(META).map_err(Error::index)?;
            table.insert(key, value).map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    fn list_loose(&self) -> Result<Vec<(ObjectId, PathBuf)>> {
        let root = self.hord_dir.join("objects");
        let mut out = Vec::new();
        let shards = match fs::read_dir(&root) {
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
