//! Core object model for Hord (spec §3).
//!
//! Persistent values serialize with [`hord_encoding`] as canonical CBOR
//! (RFC 8949 §4.2.1) so [`ObjectId::of`] is well-defined. Field names match the
//! spec; exact shapes are serde-friendly encodings of those fields.
//!
//! Byte payloads ([`Bytes`], [`ObjectId`]) encode as CBOR major type 2, not as
//! arrays of integers.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod actor;
mod blob;
mod bytes;
mod change;
mod error;
mod evidence;
mod id;
mod identity;
mod intern;
mod node;
mod policy;
mod snapshot;
mod tree;

pub use actor::{Actor, Timestamp};
pub use blob::Blob;
pub use bytes::Bytes;
pub use change::{
    Acceptance, ChangeRecord, Intent, IntentRef, Op, Provenance, Signature, TreeOpKind,
};
pub use error::Error;
pub use evidence::{Evidence, EvidenceKind, EvidenceResult};
pub use hord_encoding::ObjectId;
pub use id::{ChangeId, NodeId, SnapshotId};
pub use identity::{
    FileIdentity, IdentityDelta, IdentityEdit, IdentityEntry, IdentityMap, IdentityTree,
    IdentityTrees, NodePath, edit_identity_tree,
};
pub use node::{AdapterId, LangId, Node, NodeKind, QualifiedName};
pub use policy::{LandPolicy, Policy, PolicyRule, PolicyWhen};
pub use snapshot::{IndexPointers, Snapshot, SnapshotMetadata};
pub use tree::{NodeFile, RepoPath, Tree, TreeEntry};
