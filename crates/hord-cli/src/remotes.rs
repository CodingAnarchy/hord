//! Configured remotes (`hord remote`, ADR 0024 amendment): names, their
//! addresses, and the clone's default upstream, in `.hord/remotes.toml`.
//!
//! ```toml
//! default = "origin"
//!
//! [remotes]
//! origin = "http://127.0.0.1:7878"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// File name under `.hord/`.
const FILE: &str = "remotes.toml";

/// The remotes of one repository.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Remotes {
    /// The default upstream, used when `--remote` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Name → address.
    #[serde(default)]
    pub remotes: BTreeMap<String, String>,
}

fn path(hord_dir: &Path) -> PathBuf {
    hord_dir.join(FILE)
}

impl Remotes {
    /// Read `.hord/remotes.toml`; empty when it does not exist.
    pub fn load(hord_dir: &Path) -> Result<Self> {
        let file = path(hord_dir);
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", file.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", file.display())),
        }
    }

    /// Write `.hord/remotes.toml`.
    pub fn save(&self, hord_dir: &Path) -> Result<()> {
        let file = path(hord_dir);
        std::fs::write(&file, toml::to_string(self)?)
            .with_context(|| format!("write {}", file.display()))
    }

    /// Add `name`, checking the address's shape.
    pub fn add(&mut self, name: &str, url: &str) -> Result<()> {
        if name.is_empty() || name.contains('/') || name.contains(char::is_whitespace) {
            bail!("invalid remote name {name:?}: no slashes or spaces");
        }
        if self.remotes.contains_key(name) {
            bail!("remote {name} already exists");
        }
        if !url.starts_with("http://") {
            bail!("remote address must be http://host:port[/r/<name>] (no TLS until M5)");
        }
        self.remotes.insert(name.to_owned(), url.to_owned());
        Ok(())
    }

    /// Remove `name` (and the default if it was that).
    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.remotes.remove(name).is_none() {
            bail!("no remote {name}");
        }
        if self.default.as_deref() == Some(name) {
            self.default = None;
        }
        Ok(())
    }

    /// Make `name` the default upstream, or clear it.
    pub fn set_default(&mut self, name: Option<&str>) -> Result<()> {
        if let Some(name) = name
            && !self.remotes.contains_key(name)
        {
            bail!("no remote {name}");
        }
        self.default = name.map(str::to_owned);
        Ok(())
    }

    /// The address of `name`.
    pub fn url(&self, name: &str) -> Result<&str> {
        self.remotes
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("no remote {name}; see `hord remote list`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_default_remove_round_trip() {
        let dir = std::env::temp_dir().join(format!("hord-remotes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut remotes = Remotes::load(&dir).unwrap();
        assert_eq!(remotes, Remotes::default());
        remotes.add("origin", "http://127.0.0.1:1").unwrap();
        assert!(remotes.add("origin", "http://x").is_err());
        assert!(remotes.add("a/b", "http://x").is_err());
        assert!(remotes.add("tls", "https://x").is_err());
        remotes.set_default(Some("origin")).unwrap();
        assert!(remotes.set_default(Some("nope")).is_err());
        remotes.save(&dir).unwrap();
        let mut loaded = Remotes::load(&dir).unwrap();
        assert_eq!(loaded, remotes);
        loaded.remove("origin").unwrap();
        assert_eq!(loaded.default, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
