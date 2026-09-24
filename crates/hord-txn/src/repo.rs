//! [`Repo`]: the shared handle every workspace and the lander hang off.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use hord_core::{
    Actor, ChangeId, ChangeRecord, IdentityTree, Intent, LangId, ObjectId, Op, Provenance,
    RepoPath, Snapshot, SnapshotId, Timestamp, Tree, TreeOpKind,
};
use hord_lang::{AdapterRegistry, IdentifiedTree, NodeTree};
use hord_store::{Store, WorkspaceId};

use crate::lander::{QueueEntry, Verdict, Verifier, VerifyFuture, VerifyRequest};
use crate::materialize::MaterializeMode;
use crate::semantic::RustCtx;
use crate::workspace::{Materialization, Workspace};
use crate::{Error, Result};

/// Byte budget for each of the parsed and identified tree caches, in
/// [`NodeTree::resident_bytes`] (about 3/4 of the allocator's real use).
/// Parsing every Rust file of cargo (~1,400 files, 10.5 MB) is ~320 MB by
/// that measure. Identified trees share their nodes with the parsed ones,
/// so the two budgets overlap in memory. Measured on 1,000 changes queued
/// on one cargo base (perf review `queue 1000`): 1 GiB re-parses evicted
/// base files while landing (38/s), 1.5 GiB does not (60/s, peak RSS
/// 2.6 GB against 4.5 GB with the old count cap).
const MAX_CACHED_TREE_BYTES: usize = 1536 << 20;
/// Cap on cached per-snapshot Rust resolution contexts.
const MAX_CACHED_CTX: usize = 8;
/// Cap on cached footprints of landed changes and on the ids of records
/// this process proposed. The lander needs the footprints of the changes
/// landed after a queued change's base; a miss recomputes one from its
/// record. Beyond this many, the least recently used are dropped (they grew
/// by one per landing forever before).
pub(crate) const MAX_TRACKED_CHANGES: usize = 16_384;

/// Repository-wide settings for the lander (spec §6.3, §7.2).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoConfig {
    /// Report write-read conflicts (a change writes what a landed change
    /// read). Off by default (spec §6.3: `strict_reads = false`).
    pub strict_reads: bool,
}

/// How to construct a [`Repo`] beyond the defaults.
#[derive(Clone, Default)]
pub struct RepoOptions {
    /// Lander settings.
    pub config: RepoConfig,
    /// Language adapters. `None` registers [`default_adapters`].
    pub adapters: Option<AdapterRegistry>,
    /// Verification step run by the lander. `None` uses
    /// [`FailClosedVerifier`]: until M4's verifier exists, only clean
    /// changes land. [`crate::StubVerifier`] (land everything) is opt-in.
    pub verifier: Option<Arc<dyn Verifier>>,
}

impl std::fmt::Debug for RepoOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoOptions")
            .field("config", &self.config)
            .field(
                "adapters",
                &self.adapters.as_ref().map(AdapterRegistry::len),
            )
            .field("verifier", &self.verifier.is_some())
            .finish()
    }
}

/// The default verifier until M4 (spec §15: never land an unverified merge).
///
/// Passes a change only when its report is clean: no set overlap with a
/// landed change (spec §6.3) and no soft merge conflict from the structural
/// rebase (§6.4 rung 1). One exemption (ADR 0013 amendment): overlaps whose
/// every node and path lies in files a purpose-built adapter merge resolved
/// with no conflict ([`crate::ConflictReport::only_adapter_merged`]).
/// Anything else fails, so the lander parks it as
/// [`crate::QueueStatus::Conflicted`] with the reason in
/// [`crate::ConflictReport::verification`]. Disjoint, clean changes land
/// without verification, as in M3.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailClosedVerifier;

impl Verifier for FailClosedVerifier {
    fn verify<'a>(&'a self, request: VerifyRequest<'a>) -> VerifyFuture<'a> {
        let report = request.report;
        // ADR 0013 amendment: overlaps confined to files an adapter merge
        // resolved (for example concurrent lockfile additions) land.
        let verdict = if report.only_adapter_merged() {
            Verdict::Pass
        } else {
            let mut kinds: Vec<&str> = report
                .conflicts
                .iter()
                .map(|c| match c.kind {
                    crate::ConflictKind::WriteWrite => "write-write",
                    crate::ConflictKind::ReadWrite => "read-write",
                    crate::ConflictKind::WriteRead => "write-read",
                })
                .collect();
            kinds.sort_unstable();
            kinds.dedup();
            Verdict::Fail {
                reason: format!(
                    "unverified overlap: {} set conflict(s) [{}] and {} soft merge \
                     conflict(s); no verifier is configured, so only clean changes land",
                    report.conflicts.len(),
                    kinds.join(", "),
                    report.merge.len(),
                ),
            }
        };
        Box::pin(async move { verdict })
    }
}

/// The adapters every repository gets unless [`RepoOptions::adapters`] says
/// otherwise: `Cargo.lock` (spec §4.4, ADR 0013; ahead of TOML so each
/// `[[package]]` gets its own `NodeId`), Rust (spec §4.2), and TOML.
#[must_use]
pub fn default_adapters() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry.register(Arc::new(hord_lang_rust::CargoLockAdapter));
    registry.register(Arc::new(hord_lang_rust::RustAdapter));
    registry.register(Arc::new(hord_lang_toml::TomlAdapter));
    registry
}

/// Where a new workspace starts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Base {
    /// The result snapshot of the current `head` (or the empty snapshot).
    #[default]
    Head,
    /// The result snapshot of a landed change. That change becomes the
    /// proposal's parent.
    Change(ChangeId),
    /// A snapshot id. The proposal has no parent unless the snapshot is the
    /// current head's.
    Snapshot(SnapshotId),
}

/// Arguments to [`Repo::begin`] and [`Repo::begin_directory`].
#[derive(Clone, Debug)]
pub struct BeginOptions {
    /// Snapshot to start from.
    pub base: Base,
    /// Who works in the workspace; recorded as `provenance.actor`.
    pub actor: Actor,
    /// Opaque harness session id, recorded as `provenance.session`.
    pub session: Option<String>,
}

impl BeginOptions {
    /// Start at `head` as `actor`.
    #[must_use]
    pub fn at_head(actor: Actor) -> Self {
        Self {
            base: Base::Head,
            actor,
            session: None,
        }
    }
}

/// The latest landed change and its result snapshot (spec §3.7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Head {
    /// Latest landed change; `None` before anything lands.
    pub change: Option<ChangeId>,
    /// Its result snapshot, or the empty snapshot ([`Snapshot::empty`]).
    pub snapshot: SnapshotId,
}

/// Handle to one repository: object store, adapters, caches, and the lander.
///
/// Cheap to clone. Every operation is async; blocking store and parse work
/// runs on tokio's blocking pool. The lander is single-process by
/// construction (the redb index admits one process) and single-task within
/// the process.
#[derive(Clone)]
pub struct Repo {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo")
            .field("root", &self.inner.store.repo_root())
            .finish_non_exhaustive()
    }
}

pub(crate) struct Inner {
    pub store: Store,
    pub adapters: AdapterRegistry,
    pub config: RepoConfig,
    pub verifier: Arc<dyn Verifier>,
    pub toolchain: ObjectId,
    /// Root [`Tree`] with no entries.
    pub empty_tree: ObjectId,
    /// [`Snapshot::empty`]: the base before anything lands.
    pub empty_snapshot: SnapshotId,
    pub head: Mutex<Option<Head>>,
    pub trees: Mutex<HashMap<ObjectId, Arc<Tree>>>,
    pub snapshots: Mutex<HashMap<SnapshotId, Arc<Snapshot>>>,
    pub identity_trees: Mutex<HashMap<ObjectId, Arc<IdentityTree>>>,
    pub parsed: Mutex<WeightedLru<(ObjectId, LangId), Arc<NodeTree>>>,
    pub identified: Mutex<IdentifiedCache>,
    pub rust_ctx: Mutex<CtxCache>,
    /// Footprints of landed changes, least recently used evicted.
    pub footprints: Mutex<WeightedLru<ChangeId, Arc<crate::conflict::Footprint>>>,
    /// Records built by this process's `propose`, whose ops were checked
    /// then (least recently proposed evicted; `submit` also records the
    /// check in the store).
    pub proposed: Mutex<WeightedLru<ChangeId, ()>>,
    /// Serializes the lander (spec §6.7: one lander per repository).
    pub lander: tokio::sync::Mutex<crate::lander::LanderState>,
}

/// Identified trees keyed by (path, blob, carried identity object). The path
/// matters: a fresh assignment depends on it, so two files with identical
/// content get different ids.
pub(crate) type IdentifiedCache =
    WeightedLru<(RepoPath, ObjectId, Option<ObjectId>), Arc<IdentifiedTree>>;

/// A cache bounded by total weight (bytes), evicting the least recently
/// used entry one at a time until it fits (perf review #3), rather than
/// clearing everything when a count cap is hit. The newest entry always
/// stays, even when it alone is over the budget.
pub(crate) struct WeightedLru<K, V> {
    entries: HashMap<K, LruEntry<V>>,
    /// Last use → key; the first entry is the eviction candidate.
    order: BTreeMap<u64, K>,
    tick: u64,
    weight: usize,
    budget: usize,
}

struct LruEntry<V> {
    value: V,
    weight: usize,
    used: u64,
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> WeightedLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            weight: 0,
            budget,
        }
    }

    /// The cached value, marked as most recently used.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.get_mut(key)?;
        self.order.remove(&entry.used);
        self.tick += 1;
        entry.used = self.tick;
        self.order.insert(self.tick, key.clone());
        Some(entry.value.clone())
    }

    pub fn insert(&mut self, key: K, value: V, weight: usize) {
        self.remove(&key);
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.weight += weight;
        self.entries.insert(
            key,
            LruEntry {
                value,
                weight,
                used: self.tick,
            },
        );
        while self.weight > self.budget && self.entries.len() > 1 {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.weight -= entry.weight;
            }
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(entry) = self.entries.remove(key) {
            self.order.remove(&entry.used);
            self.weight -= entry.weight;
        }
    }

    /// Total weight of the cached entries.
    #[cfg(test)]
    pub fn weight(&self) -> usize {
        self.weight
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

pub(crate) type CtxSlot = Arc<OnceLock<std::result::Result<Arc<RustCtx>, String>>>;

#[derive(Default)]
pub(crate) struct CtxCache {
    pub slots: HashMap<SnapshotId, CtxSlot>,
    pub order: Vec<SnapshotId>,
    /// The context built last: the next snapshot's build starts from it.
    pub latest: Option<Arc<RustCtx>>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

pub(crate) fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    Timestamp::from_millis(ms)
}

/// Toolchain descriptor hashed into `provenance.toolchain` (spec §3.5).
#[derive(serde::Serialize)]
struct Toolchain<'a> {
    tool: &'a str,
    version: &'a str,
    grammars: [&'a str; 2],
}

impl Inner {
    fn new(store: Store, options: RepoOptions) -> Result<Self> {
        let toolchain = store.put_object(&Toolchain {
            tool: "hord-txn",
            version: env!("CARGO_PKG_VERSION"),
            grammars: ["tree-sitter-rust 0.24", "tree-sitter-toml-ng 0.7"],
        })?;
        let empty_tree = store.put_object(&Tree::default())?;
        store.put_object(&IdentityTree::default())?;
        let empty_snapshot = store.put_object(&Snapshot::empty())?;
        Ok(Self {
            store,
            adapters: options.adapters.unwrap_or_else(default_adapters),
            config: options.config,
            verifier: options
                .verifier
                .unwrap_or_else(|| Arc::new(FailClosedVerifier)),
            toolchain,
            empty_tree,
            empty_snapshot,
            head: Mutex::new(None),
            trees: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            identity_trees: Mutex::new(HashMap::new()),
            parsed: Mutex::new(WeightedLru::new(MAX_CACHED_TREE_BYTES)),
            identified: Mutex::new(WeightedLru::new(MAX_CACHED_TREE_BYTES)),
            rust_ctx: Mutex::new(CtxCache::default()),
            footprints: Mutex::new(WeightedLru::new(MAX_TRACKED_CHANGES)),
            proposed: Mutex::new(WeightedLru::new(MAX_TRACKED_CHANGES)),
            lander: tokio::sync::Mutex::new(crate::lander::LanderState::default()),
        })
    }

    pub(crate) fn head(&self) -> Result<Head> {
        if let Some(head) = *lock(&self.head) {
            return Ok(head);
        }
        let head = match self.store.head()? {
            Some(change) => Head {
                change: Some(change),
                snapshot: self.change_record(change)?.result,
            },
            None => Head {
                change: None,
                snapshot: self.empty_snapshot,
            },
        };
        *lock(&self.head) = Some(head);
        Ok(head)
    }

    pub(crate) fn set_head_cache(&self, head: Head) {
        *lock(&self.head) = Some(head);
    }

    pub(crate) fn change_record(&self, id: ChangeId) -> Result<ChangeRecord> {
        match self.store.get_object::<ChangeRecord>(id) {
            Ok(record) => Ok(record),
            Err(hord_store::Error::MissingObject(_) | hord_store::Error::Encoding(_)) => {
                Err(Error::MissingChange(id))
            }
            Err(err) => Err(err.into()),
        }
    }

    pub(crate) fn cache_parsed(&self, key: (ObjectId, LangId), tree: Arc<NodeTree>) {
        let weight = tree.resident_bytes();
        lock(&self.parsed).insert(key, tree, weight);
    }

    pub(crate) fn cache_identified(
        &self,
        key: (RepoPath, ObjectId, Option<ObjectId>),
        tree: Arc<IdentifiedTree>,
    ) {
        let weight = tree.resident_bytes();
        lock(&self.identified).insert(key, tree, weight);
    }

    pub(crate) fn ctx_slot(&self, snapshot: SnapshotId) -> CtxSlot {
        let mut cache = lock(&self.rust_ctx);
        if let Some(slot) = cache.slots.get(&snapshot) {
            return Arc::clone(slot);
        }
        if cache.order.len() >= MAX_CACHED_CTX {
            let evict = cache.order.remove(0);
            cache.slots.remove(&evict);
        }
        let slot: CtxSlot = Arc::new(OnceLock::new());
        cache.slots.insert(snapshot, Arc::clone(&slot));
        cache.order.push(snapshot);
        slot
    }

    pub(crate) fn latest_rust_ctx(&self) -> Option<Arc<RustCtx>> {
        lock(&self.rust_ctx).latest.clone()
    }

    pub(crate) fn set_latest_rust_ctx(&self, ctx: Arc<RustCtx>) {
        lock(&self.rust_ctx).latest = Some(ctx);
    }

    /// Record that this process proposed `change` and checked its ops.
    pub(crate) fn note_proposed(&self, change: ChangeId) {
        lock(&self.proposed).insert(change, (), 1);
    }

    /// Whether this process proposed `change` (and checked its ops).
    pub(crate) fn was_proposed(&self, change: ChangeId) -> bool {
        lock(&self.proposed).get(&change).is_some()
    }

    /// Land `files` as the first change (Tier 0 ops, fresh identity).
    fn bootstrap(
        &self,
        files: Vec<(RepoPath, Vec<u8>)>,
        intent: Intent,
        actor: Actor,
    ) -> Result<ChangeId> {
        if let Some(head) = self.store.head()? {
            return Err(Error::NotEmpty(head));
        }
        let mut changes = BTreeMap::new();
        let mut ops = Vec::new();
        for (path, bytes) in files {
            let blob = self.put_blob(&bytes)?;
            ops.push(Op::Tree {
                path: path.clone(),
                kind: TreeOpKind::CreateFile,
            });
            ops.push(Op::Blob {
                path: path.clone(),
                from: None,
                to: Some(blob),
            });
            changes.insert(path, Some(blob));
        }
        // Tier 0: every file is a fresh assignment, so the identity tree is
        // empty (ADR 0017).
        let result = self.commit_snapshot(self.empty_snapshot, &changes, &BTreeMap::new())?;
        let record = ChangeRecord {
            base: self.empty_snapshot,
            result,
            parents: Vec::new(),
            ops,
            intent,
            provenance: Provenance {
                actor,
                toolchain: self.toolchain,
                created_at: now(),
                session: None,
                parent_intent: None,
            },
            read_set: Default::default(),
            write_set: Default::default(),
            identity_deltas: Vec::new(),
            evidence: Vec::new(),
            signature: None,
            rebased_from: None,
        };
        let change = self.store.put_object(&record)?;
        self.store.append_log(change)?;
        self.store.set_head(change)?;
        self.store.index_change(change)?;
        self.set_head_cache(Head {
            change: Some(change),
            snapshot: result,
        });
        Ok(change)
    }

    fn resolve_base(&self, base: Base) -> Result<(SnapshotId, Option<ChangeId>)> {
        match base {
            Base::Head => {
                let head = self.head()?;
                Ok((head.snapshot, head.change))
            }
            Base::Change(change) => Ok((self.change_record(change)?.result, Some(change))),
            Base::Snapshot(snapshot) => {
                let head = self.head()?;
                let parent = (head.snapshot == snapshot).then_some(head.change).flatten();
                Ok((snapshot, parent))
            }
        }
    }
}

/// Run `f` against the shared state on tokio's blocking pool.
pub(crate) async fn blocking<T, F>(inner: &Arc<Inner>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> Result<T> + Send + 'static,
{
    let inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || f(&inner))
        .await
        .map_err(|err| Error::Task(err.to_string()))?
}

impl Repo {
    /// Create `<root>/.hord/` and open it with default options.
    pub async fn create(root: impl AsRef<Path>) -> Result<Self> {
        Self::create_with(root, RepoOptions::default()).await
    }

    /// Create `<root>/.hord/` and open it with `options` (for example
    /// [`RepoConfig::strict_reads`], adapters, or a verifier).
    pub async fn create_with(root: impl AsRef<Path>, options: RepoOptions) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let store = tokio::task::spawn_blocking(move || Store::create(root))
            .await
            .map_err(|err| Error::Task(err.to_string()))??;
        Self::from_store(store, options).await
    }

    /// Open the store at `<root>/.hord/` with default options.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(root, RepoOptions::default()).await
    }

    /// Open the store at `<root>/.hord/` with `options`.
    pub async fn open_with(root: impl AsRef<Path>, options: RepoOptions) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let store = tokio::task::spawn_blocking(move || Store::open(root))
            .await
            .map_err(|err| Error::Task(err.to_string()))??;
        Self::from_store(store, options).await
    }

    /// Wrap an already-open [`Store`].
    pub async fn from_store(store: Store, options: RepoOptions) -> Result<Self> {
        let inner = tokio::task::spawn_blocking(move || Inner::new(store, options))
            .await
            .map_err(|err| Error::Task(err.to_string()))??;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The underlying object store.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// Lander settings this handle was opened with.
    #[must_use]
    pub fn config(&self) -> &RepoConfig {
        &self.inner.config
    }

    /// Current `head` and its snapshot.
    pub async fn head(&self) -> Result<Head> {
        blocking(&self.inner, Inner::head).await
    }

    /// Load a stored [`ChangeRecord`].
    pub async fn change(&self, id: ChangeId) -> Result<ChangeRecord> {
        blocking(&self.inner, move |inner| inner.change_record(id)).await
    }

    /// Files that differ between two snapshots, in path order (a tree diff).
    pub async fn changed_paths(&self, from: SnapshotId, to: SnapshotId) -> Result<Vec<RepoPath>> {
        blocking(&self.inner, move |inner| {
            Ok(inner
                .changed_paths(from, to)?
                .into_iter()
                .map(|d| d.path)
                .collect())
        })
        .await
    }

    /// Definitions of `path` in `snapshot` with their ids, names, and spans.
    /// Empty when the file is missing, blob tier, or does not parse.
    pub async fn definitions_in(
        &self,
        snapshot: SnapshotId,
        path: RepoPath,
    ) -> Result<Vec<crate::DefinitionInfo>> {
        blocking(&self.inner, move |inner| {
            let Some(view) = inner.file_view(snapshot, &path)? else {
                return Ok(Vec::new());
            };
            let Some(parsed) = view.parsed else {
                return Ok(Vec::new());
            };
            let Some(adapter) = inner.adapter_for(&path, parsed.lang) else {
                return Ok(Vec::new());
            };
            Ok(crate::semantic::definitions(adapter, &path, &parsed.tree))
        })
        .await
    }

    /// Land `files` as the repository's first change, bypassing the queue.
    ///
    /// For tests and benchmarks that need a base without a git import. Fails
    /// with [`Error::NotEmpty`] once anything has landed.
    pub async fn bootstrap(
        &self,
        files: Vec<(RepoPath, Vec<u8>)>,
        intent: Intent,
        actor: Actor,
    ) -> Result<ChangeId> {
        let _lander = self.inner.lander.lock().await;
        blocking(&self.inner, move |inner| {
            inner.bootstrap(files, intent, actor)
        })
        .await
    }

    /// Start an in-memory workspace (spec §6.1).
    ///
    /// O(1): a snapshot pointer, an empty overlay, and an empty access log.
    /// Nothing is written to the store or the filesystem.
    pub async fn begin(&self, options: BeginOptions) -> Result<Workspace> {
        let base = options.base;
        let (snapshot, parent) = if base == Base::Head
            && let Some(head) = *lock(&self.inner.head)
        {
            (head.snapshot, head.change)
        } else {
            blocking(&self.inner, move |inner| inner.resolve_base(base)).await?
        };
        Ok(Workspace::new(
            self.clone(),
            WorkspaceId::generate(),
            snapshot,
            parent,
            options.actor,
            options.session,
            Materialization::InMemory,
        ))
    }

    /// Start a workspace materialized as a directory at `.hord/ws/<id>/`
    /// (the `hord ws new` layout): a copy-on-write clone of the base's
    /// pristine checkout where the filesystem can clone, else a copy
    /// (ADR 0016). Same as [`Repo::begin_directory_with`] with
    /// [`MaterializeMode::Clone`].
    ///
    /// Reads made with ordinary file tools are not observed; writes are found
    /// at propose time from the stat index and the directory (ADR 0012).
    pub async fn begin_directory(&self, options: BeginOptions) -> Result<Workspace> {
        self.begin_directory_with(options, MaterializeMode::Clone)
            .await
    }

    /// [`Repo::begin_directory`] with an explicit [`MaterializeMode`]. The
    /// mode actually used is [`Workspace::materialized_as`]: a clone request
    /// falls back to a copy on filesystems without reflink support. The
    /// first workspace on a base also writes that base's pristine checkout.
    pub async fn begin_directory_with(
        &self,
        options: BeginOptions,
        mode: MaterializeMode,
    ) -> Result<Workspace> {
        let base = options.base;
        let (meta, parent, used) = blocking(&self.inner, move |inner| {
            let (snapshot, parent) = inner.resolve_base(base)?;
            let meta = inner.store.create_workspace(snapshot)?;
            let used = inner.materialize_dir(snapshot, &meta.path, mode)?;
            Ok((meta, parent, used))
        })
        .await?;
        let mut ws = Workspace::new(
            self.clone(),
            meta.id,
            meta.base,
            parent,
            options.actor,
            options.session,
            Materialization::Directory { path: meta.path },
        );
        ws.set_materialized_as(Some(used));
        Ok(ws)
    }

    /// Delete a workspace's store row, directory, and stat index
    /// (`hord ws rm`). Returns whether it existed.
    pub async fn remove_workspace(&self, id: WorkspaceId) -> Result<bool> {
        blocking(&self.inner, move |inner| {
            Ok(inner.store.remove_workspace(id)?)
        })
        .await
    }

    /// Remove pristine checkouts no workspace uses as its base (`hord ws
    /// gc`, ADR 0016). Returns the snapshots whose checkout was removed.
    pub async fn gc_pristine(&self) -> Result<Vec<SnapshotId>> {
        blocking(&self.inner, Inner::gc_pristine).await
    }

    /// Reopen a directory workspace created by [`Repo::begin_directory`] or
    /// `hord ws new`. The access log starts empty.
    ///
    /// The parent is the latest landed change whose result is the workspace
    /// base, if any.
    pub async fn open_workspace(
        &self,
        id: WorkspaceId,
        actor: Actor,
        session: Option<String>,
    ) -> Result<Workspace> {
        let (meta, parent) = blocking(&self.inner, move |inner| {
            let meta = inner
                .store
                .get_workspace(id)?
                .ok_or(Error::UnknownWorkspace(id))?;
            let parent = inner.change_for_snapshot(meta.base)?;
            Ok((meta, parent))
        })
        .await?;
        let used = crate::materialize::load_stat_index(&meta.path).and_then(|i| i.mode);
        let mut ws = Workspace::new(
            self.clone(),
            meta.id,
            meta.base,
            parent,
            actor,
            session,
            Materialization::Directory { path: meta.path },
        );
        ws.set_materialized_as(used);
        Ok(ws)
    }

    /// Hand a proposed change to the lander (spec §6.2). Durable on return.
    ///
    /// Submitting a change that is already queued returns its entry.
    pub async fn submit(&self, change: ChangeId) -> Result<QueueEntry> {
        blocking(&self.inner, move |inner| inner.submit(change)).await
    }

    /// Every lander queue entry in submission order (`hord queue`).
    pub async fn queue(&self) -> Result<Vec<QueueEntry>> {
        blocking(&self.inner, Inner::queue_entries).await
    }

    /// The latest queue entry for `change`: a submitted id, or the id it
    /// landed under.
    pub async fn status(&self, change: ChangeId) -> Result<QueueEntry> {
        blocking(&self.inner, move |inner| inner.queue_status(change)).await
    }

    /// Run the lander inline until no entry is queued (`hord land --local`).
    ///
    /// Returns the entries processed by this call, in order.
    pub async fn land_local(&self) -> Result<Vec<QueueEntry>> {
        crate::lander::run(self).await
    }

    /// Explain a change's conflicts (`hord conflicts`).
    ///
    /// For a processed change, the report the lander recorded. Otherwise the
    /// set check (spec §6.3) against the current head, without a rebase.
    pub async fn conflicts(&self, change: ChangeId) -> Result<crate::ConflictReport> {
        blocking(&self.inner, move |inner| inner.conflicts(change)).await
    }
}

impl Inner {
    /// Write every file of `snapshot` under `dir`.
    pub(crate) fn checkout(&self, snapshot: SnapshotId, dir: &Path) -> Result<()> {
        for (path, blob) in self.list_files(snapshot)? {
            let target = fs_path(dir, &path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, self.blob_bytes(blob)?.as_slice())?;
        }
        Ok(())
    }
}

/// `dir` joined with a repository path.
pub(crate) fn fs_path(dir: &Path, path: &RepoPath) -> PathBuf {
    let mut out = dir.to_path_buf();
    for component in path.components() {
        out.push(component);
    }
    out
}

#[cfg(test)]
mod lru_tests {
    use super::WeightedLru;

    /// Over budget, the least recently used entries go one at a time; the
    /// rest stay (the count cap it replaces cleared every entry at once).
    #[test]
    fn evicts_least_recently_used_until_within_budget() {
        let mut cache = WeightedLru::new(100);
        for i in 0..10 {
            cache.insert(i, i, 10);
        }
        assert_eq!((cache.len(), cache.weight()), (10, 100));
        // Touch 0 so 1 is now the oldest.
        assert_eq!(cache.get(&0), Some(0));
        cache.insert(10, 10, 25);
        assert!(cache.weight() <= 100);
        assert_eq!(cache.get(&0), Some(0), "recently used entry survives");
        for evicted in 1..=3 {
            assert_eq!(cache.get(&evicted), None, "{evicted} was oldest");
        }
        for kept in 4..=10 {
            assert_eq!(cache.get(&kept), Some(kept));
        }
    }

    #[test]
    fn replacing_a_key_reweighs_it_and_an_oversized_entry_stays_alone() {
        let mut cache = WeightedLru::new(50);
        cache.insert("a", 1, 30);
        cache.insert("a", 2, 10);
        assert_eq!((cache.len(), cache.weight()), (1, 10));
        cache.insert("b", 3, 500);
        assert_eq!(cache.len(), 1, "the newest entry is kept even over budget");
        assert_eq!(cache.get(&"b"), Some(3));
        assert_eq!(cache.weight(), 500);
    }
}

#[cfg(test)]
mod bounded_tests {
    use std::sync::Arc;

    use hord_core::{ChangeRecord, ObjectId};

    use super::{MAX_TRACKED_CHANGES, Repo, lock};
    use crate::conflict::Footprint;

    fn id(n: usize) -> ObjectId {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
        ObjectId::from_bytes(bytes)
    }

    /// The per-landing footprint cache and the proposed-change set are
    /// bounded (perf review #7 leftover): past the cap the least recently
    /// used go, and a recent one is still there. Both grew by one entry per
    /// change forever before.
    #[tokio::test]
    async fn footprints_and_proposed_stay_bounded() {
        let dir = std::env::temp_dir().join(format!("hord-txn-bounded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repo::create(&dir).await.unwrap();
        let inner = &repo.inner;
        let record: ChangeRecord = {
            let mut ws = repo
                .begin(crate::BeginOptions::at_head(hord_core::Actor::Human {
                    id: "t".into(),
                }))
                .await
                .unwrap();
            ws.write_file(&"a.txt".parse().unwrap(), "a\n")
                .await
                .unwrap();
            ws.preview(hord_core::Intent {
                summary: "s".into(),
                body: String::new(),
                refs: Vec::new(),
                acceptance: Vec::new(),
            })
            .await
            .unwrap()
            .record
        };
        let footprint = Arc::new(Footprint::of(id(0), &record, &[]));
        let total = MAX_TRACKED_CHANGES + 100;
        for n in 0..total {
            lock(&inner.footprints).insert(id(n), Arc::clone(&footprint), 1);
            inner.note_proposed(id(n));
        }
        assert_eq!(lock(&inner.footprints).len(), MAX_TRACKED_CHANGES);
        assert_eq!(lock(&inner.proposed).len(), MAX_TRACKED_CHANGES);
        assert!(inner.was_proposed(id(total - 1)), "the newest stays");
        assert!(!inner.was_proposed(id(0)), "the oldest went");
        drop(repo);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
