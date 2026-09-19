//! Directory trees, parsed files, and repository paths (spec §3.2).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{AdapterId, Error, LangId, ObjectId};

/// A directory: ordered map of name → ([`Blob`](crate::Blob) | [`Tree`] |
/// [`NodeFile`]).
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Tree {
    /// Child entries, keyed by basename. Encoded as a CBOR map.
    pub entries: BTreeMap<String, TreeEntry>,
}

/// One child of a [`Tree`]: a blob, nested tree, or parsed file.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum TreeEntry {
    /// [`Blob`](crate::Blob) object.
    Blob(ObjectId),
    /// Nested [`Tree`] object.
    Tree(ObjectId),
    /// Parsed [`NodeFile`] object.
    NodeFile(ObjectId),
}

/// Root of a parsed file: adapter, language, root node, and raw-bytes hash.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct NodeFile {
    /// Adapter that parsed this file.
    pub adapter: AdapterId,
    /// Language id reported by the adapter.
    pub lang: LangId,
    /// [`crate::Node`] object id of the file's root.
    pub root: ObjectId,
    /// Hash of the original file bytes, for round-trip checking.
    pub raw_hash: ObjectId,
}

/// Repository-relative path, stored as path components.
///
/// Display and [`FromStr`] use `/` as the separator. The empty path is the
/// repository root. Encoded as a CBOR array of text strings (not a joined
/// path), so changing that representation would retag every `Op` that carries
/// a path.
#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoPath(Box<[String]>);

impl RepoPath {
    /// Build a path from components.
    ///
    /// Empty components are not rejected here; [`FromStr`] rejects them.
    #[must_use]
    pub fn new(components: impl Into<Vec<String>>) -> Self {
        Self(components.into().into_boxed_slice())
    }

    /// Path components, without separators.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.0
    }

    /// Whether this is the repository root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

impl FromStr for RepoPath {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(Self(Box::from([])));
        }
        if s.split('/').any(|c| c.is_empty()) {
            return Err(Error::RepoPath(s.to_owned()));
        }
        Ok(Self(
            s.split('/')
                .map(str::to_owned)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ))
    }
}

impl From<Vec<String>> for RepoPath {
    fn from(components: Vec<String>) -> Self {
        Self(components.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_parse_display() {
        let path: RepoPath = "crates/hord-core/src/lib.rs".parse().unwrap();
        assert_eq!(path.to_string(), "crates/hord-core/src/lib.rs");
        assert_eq!(path.components(), ["crates", "hord-core", "src", "lib.rs"]);
        assert!("foo//bar".parse::<RepoPath>().is_err());
        assert!("/foo".parse::<RepoPath>().is_err());
        let root: RepoPath = "".parse().unwrap();
        assert!(root.is_root());
    }
}
