//! Toolchain objects (spec §3.5, §3.6).

use std::collections::BTreeMap;

use hord_core::ObjectId;
use serde::{Deserialize, Serialize};

use crate::Result;

/// What a verifier runs with. [`hord_core::Evidence::toolchain`] is the
/// [`ObjectId`] of this object, so evidence produced by another compiler,
/// cargo, or coverage tool never matches a reuse key.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Toolchain {
    /// Language the verifier checks, e.g. `rust`.
    pub lang: String,
    /// Tool name to its version line, e.g. `rustc` → `rustc 1.98.1 (…)`.
    /// A missing optional tool is absent, so adding it changes the id.
    pub components: BTreeMap<String, String>,
}

impl Toolchain {
    /// A toolchain for `lang` with no components yet.
    #[must_use]
    pub fn new(lang: impl Into<String>) -> Self {
        Self {
            lang: lang.into(),
            components: BTreeMap::new(),
        }
    }

    /// Record `tool` at `version`.
    #[must_use]
    pub fn with(mut self, tool: impl Into<String>, version: impl Into<String>) -> Self {
        self.components.insert(tool.into(), version.into());
        self
    }

    /// Whether `tool` is part of this toolchain.
    #[must_use]
    pub fn has(&self, tool: &str) -> bool {
        self.components.contains_key(tool)
    }

    /// The object id [`hord_core::Evidence::toolchain`] names.
    pub fn id(&self) -> Result<ObjectId> {
        Ok(ObjectId::of(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_depends_on_every_component() {
        let a = Toolchain::new("rust").with("rustc", "1.0");
        let b = Toolchain::new("rust").with("rustc", "1.1");
        let c = a.clone().with("cargo-llvm-cov", "0.9");
        assert_eq!(
            a.id().expect("compute toolchain id"),
            a.clone().id().expect("compute toolchain id")
        );
        assert_ne!(
            a.id().expect("compute toolchain id"),
            b.id().expect("compute toolchain id")
        );
        assert_ne!(
            a.id().expect("compute toolchain id"),
            c.id().expect("compute toolchain id")
        );
        assert!(c.has("cargo-llvm-cov") && !a.has("cargo-llvm-cov"));
    }
}
