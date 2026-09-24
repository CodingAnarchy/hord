//! Local content-addressed object store (spec §8.1).
//!
//! Objects are addressed by [`hord_core::ObjectId`] (BLAKE3-256 of canonical CBOR).
//! Recently written data is stored as loose files under `.hord/objects/`.
//! [`Store::pack`] writes zstd-compressed pack files with a sidecar offset
//! index (spec §8.1: packing is a background job, not part of [`Store::put`]).
//!
//! The redb index holds the landing [`Store::log`], named refs, workspace
//! metadata, and packed-object locations. [`Store::set_ref`] and
//! [`Store::append_log`] are buffered and flushed on [`Store::set_head`],
//! [`Store::pack`], [`Store::flush`], or drop. Ingest batches skip fsync;
//! those flush points use a durable commit.
//!
//! `node_history`, `edges`, and `rebased` index facts that also live in
//! stored objects (spec §8.1). [`Store::rebuild_index`] reconstructs them.
//! `evidence_by_snapshot` is created empty. NodeIds are not indexed here: a
//! snapshot's identity is part of its [`hord_core::Snapshot`] object
//! (ADR 0017).
//!
//! The index records a format version ([`Store::create`]); opening a store
//! of another format fails with [`Error::StoreFormat`].
//!
//! `hord-txn` keeps more tables here: the persistent lander queue
//! ([`Store::queue_push`]) with an index from change ids to its entries
//! ([`Store::queue_named`]) and the changes whose ops were checked
//! ([`Store::mark_checked`]). The store does not interpret a queue entry.
//! [`Store::land`] writes one landing (queue entry, log, `head`,
//! `node_history`, `rebased`) in one durable commit.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod error;
mod pack;
mod store;
mod workspace;

pub use error::Error;
pub use store::{EdgeKind, HORD_DIR, Landing, Store, touched_nodes};
pub use workspace::{WorkspaceId, WorkspaceMeta};

/// Result of a store operation.
pub type Result<T> = std::result::Result<T, Error>;
