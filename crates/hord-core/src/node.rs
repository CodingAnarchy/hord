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

/// One syntax-tree node.
///
/// Invariants (spec §3.3, ADR 0008) are enforced by parse/project, not by this
/// type. A leaf stores `raw`. An internal node's stored object has no `raw`;
/// an in-memory value may still cache `concat(children.raw)` for projection,
/// and that cache is omitted from the content hash.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Node {
    /// Adapter-defined kind, e.g. `fn_item`.
    pub kind: NodeKind,
    /// Language this node was parsed as.
    pub lang: LangId,
    /// Exact source bytes of this subtree, including trivia.
    ///
    /// Present on leaves. On an internal node this is an optional cache of
    /// `concat(children.raw)` and is not part of the stored object.
    pub raw: Bytes,
    /// Semantic identity. A leaf hashes its stripped token text. An internal
    /// node hashes its children's `normalized` ids, in order.
    pub normalized: ObjectId,
    /// Child [`Node`] object ids.
    pub children: Vec<ObjectId>,
    /// Qualified name, only for named definitions.
    pub name: Option<QualifiedName>,
}

impl Serialize for Node {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // Leaves store `raw`. Internal nodes do not (ADR 0008).
        let leaf = self.children.is_empty();
        // Fields are emitted in canonical key order (RFC 8949 §4.2.1: shorter
        // encoded key first, then bytewise), so [`Node::content_id`] can
        // stream the encoding without sorting it.
        let mut out = serializer.serialize_struct("Node", if leaf { 6 } else { 5 })?;
        if leaf {
            out.serialize_field("raw", &self.raw)?;
        }
        out.serialize_field("kind", &self.kind)?;
        out.serialize_field("lang", &self.lang)?;
        out.serialize_field("name", &self.name)?;
        out.serialize_field("children", &self.children)?;
        out.serialize_field("normalized", &self.normalized)?;
        out.end()
    }
}

impl Node {
    /// Content [`ObjectId`]: equal to `ObjectId::of(self)`, computed without
    /// the canonicalizing pass. `Node`'s `Serialize` emits its fields in
    /// canonical order and holds no map or float, so the streamed bytes are
    /// already canonical (checked against [`hord_encoding::encode`] by the
    /// `canonical_order` proptest).
    pub fn content_id(&self) -> Result<ObjectId, hord_encoding::Error> {
        ObjectId::of_ordered(self)
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = NodeStored::deserialize(deserializer)?;
        let raw = if stored.children.is_empty() {
            stored.raw
        } else {
            Bytes::default()
        };
        Ok(Node {
            kind: stored.kind,
            lang: stored.lang,
            raw,
            normalized: stored.normalized,
            children: stored.children,
            name: stored.name,
        })
    }
}

#[derive(Deserialize)]
struct NodeStored {
    kind: NodeKind,
    lang: LangId,
    #[serde(default)]
    raw: Bytes,
    normalized: ObjectId,
    #[serde(default)]
    children: Vec<ObjectId>,
    #[serde(default)]
    name: Option<QualifiedName>,
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
