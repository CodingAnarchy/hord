//! Path-derived identity of a file root (ADR 0015).

use hord_core::{NodeId, ObjectId, RepoPath};

/// [`NodeId`] of the CST root of the file at `path` (ADR 0015).
///
/// The root is not a definition, but top-level `Insert`/`Move` ops need a
/// parent and a `Replace` of file-level glue needs a node. The id is derived
/// from canonical CBOR of `("hord/file", path components)`, so every file
/// has its own and the same path always gets the same one. It never equals
/// [`NodeId::nil`]. A renamed file gets a new root id.
#[must_use]
pub fn file_root_id(path: &RepoPath) -> NodeId {
    // Canonical CBOR of (domain, components) is injective, so distinct paths
    // hash distinct inputs. Encoding a tuple of strings cannot fail.
    let id = ObjectId::of(&("hord/file", path.components()))
        .unwrap_or_else(|_| ObjectId::from_canonical(b"hord/file"));
    let mut high = [0u8; 16];
    high.copy_from_slice(&id.as_bytes()[..16]);
    let value = u128::from_be_bytes(high);
    NodeId::from_u128(if value == 0 { 1 } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_ids_are_stable_distinct_and_not_nil() {
        let a: RepoPath = "src/lib.rs".parse().unwrap();
        let b: RepoPath = "src/main.rs".parse().unwrap();
        assert_eq!(file_root_id(&a), file_root_id(&a.clone()));
        assert_ne!(file_root_id(&a), file_root_id(&b));
        assert_ne!(file_root_id(&a), NodeId::nil());
        assert_ne!(file_root_id(&RepoPath::default()), NodeId::nil());
    }
}
