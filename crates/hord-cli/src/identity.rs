//! Who this user is to hord servers (spec §10.5.4): signing keys in
//! `~/.hord/keys/`, and the bearer tokens `hord login` stored in
//! `~/.hord/credentials.toml`, by remote address.
//!
//! `HORD_HOME` replaces `~/.hord` (tests, several identities on one
//! machine).
//!
//! ```toml
//! [remotes."http://127.0.0.1:7878"]
//! token = "hord_…"
//! key_file = "/home/ada/.hord/keys/ada.pem"
//! scopes = ["read", "propose", "review:human"]
//! actor = { kind = "human", id = "ada" }
//! ```

use std::collections::BTreeMap;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use hord_core::sign::SigningKey;
use hord_core::{Actor, Bytes};
use serde::{Deserialize, Serialize};

/// `~/.hord`, or `HORD_HOME`.
pub fn home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("HORD_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    std::env::home_dir()
        .map(|home| home.join(".hord"))
        .ok_or_else(|| anyhow!("no home directory; set HORD_HOME"))
}

/// The key file for `name` under `~/.hord/keys/`.
pub fn key_path(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        bail!("invalid key name {name:?}");
    }
    Ok(home()?.join("keys").join(format!("{name}.pem")))
}

/// Read a PKCS#8 PEM key file.
pub fn read_key(path: &Path) -> Result<SigningKey> {
    let pem =
        std::fs::read_to_string(path).with_context(|| format!("read key {}", path.display()))?;
    SigningKey::from_pem(&pem).with_context(|| format!("key {}", path.display()))
}

/// Write `key` to `path`, readable by its owner only. An existing file is
/// never replaced.
pub fn write_key(path: &Path, key: &SigningKey) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    write_private(path, key.to_pem()?.as_bytes(), true)
}

/// The key at `path`, generated there first if it does not exist.
pub fn load_or_create_key(path: &Path) -> Result<SigningKey> {
    if path.exists() {
        return read_key(path);
    }
    let key = SigningKey::generate()?;
    write_key(path, &key)?;
    Ok(key)
}

/// Write a secret file with owner-only permissions; with `new`, fail if it
/// exists.
fn write_private(path: &Path, bytes: &[u8], new: bool) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))
}

/// An actor as the credentials file stores it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum StoredActor {
    /// A human.
    Human {
        /// Their id.
        id: String,
    },
    /// An agent.
    Agent {
        /// Its id.
        id: String,
        /// Its model.
        model: String,
        /// Its harness.
        harness: String,
    },
}

impl From<&Actor> for StoredActor {
    fn from(actor: &Actor) -> Self {
        match actor {
            Actor::Human { id } => Self::Human { id: id.clone() },
            Actor::Agent {
                id, model, harness, ..
            } => Self::Agent {
                id: id.clone(),
                model: model.clone(),
                harness: harness.clone(),
            },
        }
    }
}

impl From<&StoredActor> for Actor {
    fn from(actor: &StoredActor) -> Self {
        match actor {
            StoredActor::Human { id } => Self::Human { id: id.clone() },
            StoredActor::Agent { id, model, harness } => Self::Agent {
                id: id.clone(),
                model: model.clone(),
                model_hash: Bytes::default(),
                harness: harness.clone(),
            },
        }
    }
}

/// What `hord login` stored for one remote.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// The bearer token.
    pub token: String,
    /// The signing key bound to the actor on that server.
    pub key_file: PathBuf,
    /// The token's scopes, as the server reported them.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The actor the token is bound to.
    pub actor: StoredActor,
}

impl Credential {
    /// The actor the token is bound to.
    pub fn actor(&self) -> Actor {
        Actor::from(&self.actor)
    }

    /// Its signing key.
    pub fn key(&self) -> Result<SigningKey> {
        read_key(&self.key_file)
    }
}

/// `~/.hord/credentials.toml`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// By remote address.
    #[serde(default)]
    pub remotes: BTreeMap<String, Credential>,
}

impl Credentials {
    fn path() -> Result<PathBuf> {
        Ok(home()?.join("credentials.toml"))
    }

    /// Read the file; empty when it does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Write the file, readable by its owner only.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        write_private(&path, toml::to_string(self)?.as_bytes(), false)
    }

    /// The credential for the remote at `url`, if logged in.
    pub fn for_url(url: &str) -> Result<Option<Credential>> {
        Ok(Self::load()?.remotes.remove(url))
    }
}

/// A password: the first line of stdin with `from_stdin`, else prompted
/// for on the terminal without echo.
pub fn password(from_stdin: bool, prompt: &str) -> Result<String> {
    let text = if from_stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read the password from stdin")?;
        line
    } else {
        rpassword::prompt_password(prompt).context("read the password from the terminal")?
    };
    let text = text.trim_end_matches(['\r', '\n']).to_owned();
    if text.is_empty() {
        bail!("empty password");
    }
    Ok(text)
}
