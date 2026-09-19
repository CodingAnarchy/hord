//! Syntax-tree nodes and related names (spec §3.2–3.3).

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use lasso::Spur;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::intern;
use crate::{Bytes, ObjectId};

macro_rules! interned_str {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Copy, Clone)]
        pub struct $name(Spur);

        impl $name {
            /// Intern `value`. Repeated calls with the same string share one copy.
            #[must_use]
            pub fn new(value: impl AsRef<str>) -> Self {
                Self(intern::intern(value.as_ref()))
            }

            /// Borrow the interned string.
            #[must_use]
            pub fn as_str(&self) -> &'static str {
                intern::resolve(self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.as_str()).finish()
            }
        }

        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                self.0 == other.0
            }
        }

        impl Eq for $name {}

        impl Hash for $name {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.hash(state);
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> Ordering {
                self.as_str().cmp(other.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct V;
                impl Visitor<'_> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a string")
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        Ok($name::new(v))
                    }
                    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                        Ok($name::new(v))
                    }
                }
                deserializer.deserialize_str(V)
            }
        }
    };
}

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Box<str>);

        impl $name {
            /// Wrap a string value.
            #[must_use]
            pub fn new(value: impl Into<Box<str>>) -> Self {
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
                Self(value.into_boxed_str())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(Box::from(value))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

interned_str! {
    /// Adapter-defined syntax kind, e.g. `fn_item` or `struct_item`.
    NodeKind
}

interned_str! {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_intern_equal_strings() {
        let a = NodeKind::new("function_item");
        let b = NodeKind::new(String::from("function_item"));
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "function_item");
        assert!(std::ptr::eq(a.as_str(), b.as_str()));
    }
}
