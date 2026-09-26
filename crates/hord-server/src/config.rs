//! `server.toml` (spec §10.5.1): the bind address and webhooks.
//!
//! ```toml
//! bind = "127.0.0.1:7878"
//!
//! [auth]
//! file = "auth.toml"             # relative to this file; see `AuthStore`
//!
//! [[webhook]]
//! url = "http://127.0.0.1:9000/hord"
//! kinds = ["landed", "parked"]   # empty or absent: every kind
//! repos = ["app"]                # empty or absent: every hosted repo
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::webhook::EVENT_KINDS;
use crate::{Error, Result};

/// Settings read from `server.toml`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to listen on when the command line gives none.
    #[serde(default)]
    pub bind: Option<String>,
    /// Webhooks.
    #[serde(default, rename = "webhook")]
    pub webhooks: Vec<WebhookConfig>,
    /// Require bearer tokens (spec §10.5.4).
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

/// `[auth]`: where the auth file is ([`crate::AuthStore`]).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// The auth file. A relative path is relative to `server.toml`'s
    /// directory once [`ServerConfig::load`] has read it.
    pub file: PathBuf,
}

/// One webhook: POST every matching event, JSON-mapped (spec §10.5.3).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// `http://` URL to POST to. (`https` needs TLS, which M4 does not
    /// ship.)
    pub url: String,
    /// Event kinds to send, by their `Event` oneof field names in
    /// `hord.proto` (`landed`, `parked`, …); empty sends every kind.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Hosted repository names to send for; empty sends for all.
    #[serde(default)]
    pub repos: Vec<String>,
}

impl ServerConfig {
    /// Read `path`, or the default config when it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err.into()),
        };
        let mut config = Self::parse(path, &text)?;
        if let (Some(auth), Some(dir)) = (&mut config.auth, path.parent()) {
            auth.file = dir.join(&auth.file);
        }
        Ok(config)
    }

    /// Parse `text` (read from `path`, which errors name).
    pub fn parse(path: &Path, text: &str) -> Result<Self> {
        let bad = |reason: String| Error::Config {
            path: path.to_path_buf(),
            reason,
        };
        let config: Self = toml::from_str(text).map_err(|err| bad(err.to_string()))?;
        for hook in &config.webhooks {
            if !hook.url.starts_with("http://") {
                return Err(bad(format!(
                    "webhook {}: only http:// URLs are supported (no TLS in M4)",
                    hook.url
                )));
            }
            hook.url
                .parse::<http::Uri>()
                .map_err(|err| bad(format!("webhook {}: {err}", hook.url)))?;
            for kind in &hook.kinds {
                if !EVENT_KINDS.contains(&kind.as_str()) {
                    return Err(bad(format!(
                        "webhook {}: unknown event kind {kind:?}; one of {}",
                        hook.url,
                        EVENT_KINDS.join(", ")
                    )));
                }
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_webhooks_and_rejects_unknown_kinds_and_tls() -> Result<(), Box<dyn std::error::Error>>
    {
        let path = Path::new("server.toml");
        let config = ServerConfig::parse(
            path,
            "bind = \"127.0.0.1:1\"\n[auth]\nfile = \"auth.toml\"\n[[webhook]]\nurl = \"http://h/x\"\nkinds = [\"landed\"]\n",
        )?;
        assert_eq!(
            config.auth.as_ref().map(|a| a.file.as_path()),
            Some(Path::new("auth.toml"))
        );
        assert_eq!(config.bind.as_deref(), Some("127.0.0.1:1"));
        assert_eq!(config.webhooks[0].kinds, ["landed"]);
        for bad in [
            "[[webhook]]\nurl = \"http://h\"\nkinds = [\"nope\"]\n",
            "[[webhook]]\nurl = \"https://h\"\n",
            "port = 1\n",
        ] {
            assert!(ServerConfig::parse(path, bad).is_err(), "{bad}");
        }
        assert_eq!(
            ServerConfig::load(Path::new("/no/such/server.toml"))?,
            ServerConfig::default()
        );
        Ok(())
    }
}
