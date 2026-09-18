//! Syntax-tree nodes and related names (spec §3.2–3.3).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Bytes, ObjectId};

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap a string value.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the inner string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype! {
    /// Adapter-defined syntax kind, e.g. `fn_item` or `struct_item`.
    NodeKind
}

string_newtype! {
    /// Language identifier, e.g. `rust` or `toml`.
    LangId
}

string_newtype! {
    /// Qualified name of a definition, e.g. `hord_core::Node`.
    QualifiedName
}

string_newtype! {
    /// Identifier of the language adapter that produced a [`NodeFile`](crate::NodeFile).
    AdapterId
}

/// One syntax-tree node, holding its exact source bytes and its children.
///
/// Invariants (spec §3.3) are enforced by parse/project, not by this type:
/// concatenation of children's `raw` equals `raw`, and `normalized` is the
/// trivia-stripped semantic identity.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Node {
    /// Adapter-defined kind, e.g. `fn_item`.
    pub kind: NodeKind,
    /// Language this node was parsed as.
    pub lang: LangId,
    /// Exact source bytes of this subtree, including trivia.
    pub raw: Bytes,
    /// Hash of the trivia-stripped canonical form (semantic identity).
    pub normalized: ObjectId,
    /// Child [`Node`] object ids. Concatenation of children `raw` equals `raw`.
    pub children: Vec<ObjectId>,
    /// Qualified name, only for named definitions.
    pub name: Option<QualifiedName>,
}
