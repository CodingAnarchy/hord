//! Configured remotes (`hord remote`, ADR 0024 amendment): names, their
//! addresses, and the clone's default upstream, in `.hord/remotes.toml`.
//!
//! ```toml
//! default = "origin"
//!
//! [remotes]
//! origin = "https://hord.example:7878"
//!
//! [ca_files]                    # optional: a CA to trust besides the system's
//! origin = "/etc/hord/ca.pem"
//! ```
//!
//! Outside `[ca_files]`, `HORD_CA_FILE` names a CA for every `https`
//! remote (ADR 0032).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// File name under `.hord/`.
const FILE: &str = "remotes.toml";

/// A PEM CA certificate to trust for `https` remotes without their own.
pub const CA_FILE_ENV: &str = "HORD_CA_FILE";

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
    /// Name → a PEM CA certificate to trust for it besides the system's
    /// roots (absolute path).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ca_files: BTreeMap<String, PathBuf>,
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
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("remote address must be http[s]://host:port[/r/<name>]");
        }
        self.remotes.insert(name.to_owned(), url.to_owned());
        Ok(())
    }

    /// Remove `name` (and the default if it was that).
    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.remotes.remove(name).is_none() {
            bail!("no remote {name}");
        }
        self.ca_files.remove(name);
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

    /// Trust `file`'s CA for `name`, an `https` remote.
    pub fn set_ca_file(&mut self, name: &str, file: &Path) -> Result<()> {
        let url = self.url(name)?;
        if !url.starts_with("https://") {
            bail!("remote {name} is not https: a CA file only applies to TLS");
        }
        let file =
            std::path::absolute(file).with_context(|| format!("CA file {}", file.display()))?;
        if !file.is_file() {
            bail!("CA file {} does not exist", file.display());
        }
        self.ca_files.insert(name.to_owned(), file);
        Ok(())
    }

    /// The CA file to trust for `url`: that of a remote with this address,
    /// else `HORD_CA_FILE`, else none.
    pub fn ca_file_for(&self, url: &str) -> Option<PathBuf> {
        if !url.starts_with("https://") {
            return None;
        }
        self.remotes
            .iter()
            .filter(|(_, u)| u.as_str() == url)
            .find_map(|(name, _)| self.ca_files.get(name).cloned())
            .or_else(|| {
                std::env::var_os(CA_FILE_ENV)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
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
    fn add_default_remove_round_trip() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("hord-remotes-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let mut remotes = Remotes::load(&dir)?;
        assert_eq!(remotes, Remotes::default());
        remotes.add("origin", "http://127.0.0.1:1")?;
        assert!(remotes.add("origin", "http://x").is_err());
        assert!(remotes.add("a/b", "http://x").is_err());
        assert!(remotes.add("ftp", "ftp://x").is_err());
        remotes.add("tls", "https://h:1")?;
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, "pem")?;
        assert!(remotes.set_ca_file("origin", &ca).is_err());
        assert!(remotes.set_ca_file("tls", &dir.join("nope.pem")).is_err());
        remotes.set_ca_file("tls", &ca)?;
        assert_eq!(remotes.ca_file_for("https://h:1"), Some(ca.clone()));
        assert_eq!(remotes.ca_file_for("http://127.0.0.1:1"), None);
        remotes.set_default(Some("origin"))?;
        assert!(remotes.set_default(Some("nope")).is_err());
        remotes.save(&dir)?;
        let mut loaded = Remotes::load(&dir)?;
        assert_eq!(loaded, remotes);
        loaded.remove("origin")?;
        assert_eq!(loaded.default, None);
        loaded.remove("tls")?;
        assert!(loaded.ca_files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
