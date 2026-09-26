//! The auth file (spec §10.5.4): the local user table, issued tokens, and
//! public signing keys bound to actors.
//!
//! One TOML file per server, named by `server.toml`'s `[auth] file` or
//! `hord serve --auth`. Passwords are stored as Argon2id PHC strings and
//! tokens as their BLAKE3 hash, so the file grants nothing by itself;
//! private keys are never stored.
//!
//! ```toml
//! [[user]]
//! name = "ada"
//! password = "$argon2id$v=19$..."
//! scopes = ["read", "propose", "review:human"]
//!
//! [[token]]
//! hash = "<blake3 of the token, hex>"
//! scopes = ["read", "propose"]
//! actor = { kind = "agent", id = "bot-1", model = "m", harness = "h" }
//!
//! [[key]]
//! id = "ed25519:<hex>"
//! actor = { kind = "human", id = "ada" }
//! ```
//!
//! Other processes (`hord user add`, an operator's editor) may write the
//! file while a server runs: every change re-reads it first, and a token or
//! key the server has not seen makes it re-read before refusing. Removing a
//! token or key takes effect at the next [`AuthStore::reload_if_changed`]
//! (a running server checks every second) or [`AuthStore::reload`] (`hord
//! serve` on SIGHUP), without a restart.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use hord_api::auth::Scope;
use hord_core::sign::{PublicKey, SigningKey};
use hord_core::{Actor, Bytes};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Failure to read, write, or use the auth file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The file could not be read or written.
    #[error("auth file {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The file does not parse.
    #[error("auth file {path}: {reason}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// Unknown user or wrong password (one error, so it does not say which).
    #[error("unknown user or wrong password")]
    BadLogin,
    /// A user with this name exists.
    #[error("user {0:?} already exists")]
    UserExists(String),
    /// A malformed request: a bad name, scope, or key.
    #[error("{0}")]
    Rejected(String),
    /// Hashing, key generation, or the random source failed.
    #[error("{0}")]
    Crypto(String),
}

/// Who a token speaks for and what it may do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    /// The actor bound to the token. Provenance is set from it.
    pub actor: Actor,
    /// The token's scopes.
    pub scopes: Vec<Scope>,
}

impl Principal {
    /// Whether the token holds `scope`.
    #[must_use]
    pub fn has(&self, scope: &Scope) -> bool {
        self.scopes.contains(scope)
    }
}

/// An actor as the auth file stores it. Agents are bound by id, model, and
/// harness (spec §10.5.4); `model_hash` is not part of the binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum StoredActor {
    Human {
        id: String,
    },
    Agent {
        id: String,
        model: String,
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

/// Whether `claimed` is the actor a token or key is bound to: the same
/// human id, or the same agent id, model, and harness.
#[must_use]
pub fn same_actor(claimed: &Actor, bound: &Actor) -> bool {
    StoredActor::from(claimed) == StoredActor::from(bound)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthFile {
    #[serde(default, rename = "user")]
    users: Vec<User>,
    #[serde(default, rename = "token")]
    tokens: Vec<Token>,
    #[serde(default, rename = "key")]
    keys: Vec<Key>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct User {
    name: String,
    password: String,
    scopes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Token {
    hash: String,
    scopes: Vec<String>,
    actor: StoredActor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Key {
    id: String,
    actor: StoredActor,
}

/// What the file looked like when it was last read: its modification time
/// and size. `None` when it did not exist.
type Stamp = Option<(Option<SystemTime>, u64)>;

fn stamp(path: &Path) -> Result<Stamp, AuthError> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some((meta.modified().ok(), meta.len()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AuthError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The file as last read, and its [`Stamp`] then.
#[derive(Debug)]
struct Loaded {
    file: AuthFile,
    stamp: Stamp,
}

impl std::ops::Deref for Loaded {
    type Target = AuthFile;

    fn deref(&self) -> &AuthFile {
        &self.file
    }
}

/// The auth file of a running server.
#[derive(Debug)]
pub struct AuthStore {
    path: PathBuf,
    state: Mutex<Loaded>,
}

/// A new token and its principal.
#[derive(Debug)]
pub struct Issued {
    /// The bearer token. Only its hash is stored.
    pub token: String,
    /// What it grants.
    pub principal: Principal,
}

impl AuthStore {
    /// Open the auth file at `path`; a missing file is an empty table.
    pub fn open(path: &Path) -> Result<Self, AuthError> {
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(load(path)?),
        })
    }

    /// Re-read the file now, dropping every token, key, and user it no
    /// longer holds. A file that does not parse (for example half-written
    /// by an editor) is an error, and what was read before stays in force.
    pub fn reload(&self) -> Result<(), AuthError> {
        let loaded = load(&self.path)?;
        *self.lock() = loaded;
        Ok(())
    }

    /// [`Self::reload`] if the file's modification time or size changed
    /// since it was last read. Whether it reloaded.
    pub fn reload_if_changed(&self) -> Result<bool, AuthError> {
        // Under the lock, like the store's own writes, which update the
        // stamp before releasing it: they are never seen as a change.
        let mut state = self.lock();
        if stamp(&self.path)? == state.stamp {
            return Ok(false);
        }
        *state = load(&self.path)?;
        Ok(true)
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Loaded> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Re-read the file, apply `f`, and write it back.
    fn update<T>(
        &self,
        f: impl FnOnce(&mut AuthFile) -> Result<T, AuthError>,
    ) -> Result<T, AuthError> {
        let mut state = self.lock();
        *state = load(&self.path)?;
        let out = f(&mut state.file)?;
        write(&self.path, &state.file)?;
        state.stamp = stamp(&self.path)?;
        Ok(out)
    }

    /// The principal of `token` if this process has already seen it: no
    /// I/O.
    #[must_use]
    pub fn authenticate_cached(&self, token: &str) -> Option<Principal> {
        find_token(&self.lock(), &token_hash(token))
    }

    /// The principal of `token`, or `None` for a token the file does not
    /// hold. Re-reads the file (blocking) for a token not seen yet.
    pub fn authenticate(&self, token: &str) -> Result<Option<Principal>, AuthError> {
        let hash = token_hash(token);
        let mut state = self.lock();
        if let Some(found) = find_token(&state, &hash) {
            return Ok(Some(found));
        }
        *state = load(&self.path)?;
        Ok(find_token(&state, &hash))
    }

    /// The actor `key_id` is bound to, if any.
    pub fn key_actor(&self, key_id: &str) -> Result<Option<Actor>, AuthError> {
        let mut state = self.lock();
        if let Some(actor) = find_key(&state, key_id) {
            return Ok(Some(actor));
        }
        *state = load(&self.path)?;
        Ok(find_key(&state, key_id))
    }

    /// Check a user's password; bind `key_id` to them and issue a token
    /// with the user's scopes.
    pub fn login(&self, user: &str, password: &str, key_id: &str) -> Result<Issued, AuthError> {
        PublicKey::from_key_id(key_id).map_err(|err| AuthError::Rejected(err.to_string()))?;
        self.update(|state| {
            let found = state
                .users
                .iter()
                .find(|u| u.name == user)
                .ok_or(AuthError::BadLogin)?;
            let (stored, scopes) = (found.password.clone(), found.scopes.clone());
            let hash = PasswordHash::new(&stored).map_err(|err| AuthError::Invalid {
                path: self.path.clone(),
                reason: format!("user {user}: password hash: {err}"),
            })?;
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .map_err(|_| AuthError::BadLogin)?;
            let actor = StoredActor::Human {
                id: user.to_owned(),
            };
            bind_key(state, key_id, &actor)?;
            issue(state, actor, scopes)
        })
    }

    /// Mint a token for an agent (`hord token mint`), with a new signing
    /// key bound to it. The key's private half is returned, never stored.
    pub fn mint(&self, agent: &Actor, scopes: &[Scope]) -> Result<(Issued, SigningKey), AuthError> {
        if !matches!(agent, Actor::Agent { .. }) || agent.id().is_empty() {
            return Err(AuthError::Rejected(
                "a minted token is bound to an agent with an id".into(),
            ));
        }
        let key = SigningKey::generate().map_err(|err| AuthError::Crypto(err.to_string()))?;
        let key_id = key.public().key_id();
        let actor = StoredActor::from(agent);
        let scopes = scopes.iter().map(ToString::to_string).collect();
        let issued = self.update(|state| {
            bind_key(state, &key_id, &actor)?;
            issue(state, actor.clone(), scopes)
        })?;
        Ok((issued, key))
    }

    /// Add a user to the auth file at `path` (`hord user add`), creating
    /// it if needed.
    pub fn add_user(
        path: &Path,
        name: &str,
        password: &str,
        scopes: &[Scope],
    ) -> Result<(), AuthError> {
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(AuthError::Rejected(format!(
                "invalid user name {name:?}: no spaces"
            )));
        }
        if password.is_empty() {
            return Err(AuthError::Rejected("empty password".into()));
        }
        let hash = Argon2::default()
            .hash_password(password.as_bytes())
            .map_err(|err| AuthError::Crypto(err.to_string()))?
            .to_string();
        let mut state = read(path)?;
        if state.users.iter().any(|u| u.name == name) {
            return Err(AuthError::UserExists(name.to_owned()));
        }
        state.users.push(User {
            name: name.to_owned(),
            password: hash,
            scopes: scopes.iter().map(ToString::to_string).collect(),
        });
        write(path, &state)
    }
}

fn find_token(state: &AuthFile, hash: &str) -> Option<Principal> {
    let token = state.tokens.iter().find(|t| t.hash == hash)?;
    Some(Principal {
        actor: Actor::from(&token.actor),
        // Scopes were checked when issued; one edited into something
        // unknown grants nothing.
        scopes: token.scopes.iter().filter_map(|s| s.parse().ok()).collect(),
    })
}

fn find_key(state: &AuthFile, key_id: &str) -> Option<Actor> {
    state
        .keys
        .iter()
        .find(|k| k.id == key_id)
        .map(|k| Actor::from(&k.actor))
}

/// Bind `key_id` to `actor`; a key already bound to another actor is
/// refused.
fn bind_key(state: &mut AuthFile, key_id: &str, actor: &StoredActor) -> Result<(), AuthError> {
    match state.keys.iter().find(|k| k.id == key_id) {
        Some(key) if &key.actor == actor => Ok(()),
        Some(_) => Err(AuthError::Rejected(format!(
            "key {key_id} is bound to another actor"
        ))),
        None => {
            state.keys.push(Key {
                id: key_id.to_owned(),
                actor: actor.clone(),
            });
            Ok(())
        }
    }
}

fn issue(
    state: &mut AuthFile,
    actor: StoredActor,
    scopes: Vec<String>,
) -> Result<Issued, AuthError> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|err| AuthError::Crypto(err.to_string()))?;
    let token = format!("hord_{}", hex::encode(raw));
    let principal = Principal {
        actor: Actor::from(&actor),
        scopes: scopes.iter().filter_map(|s| s.parse().ok()).collect(),
    };
    state.tokens.push(Token {
        hash: token_hash(&token),
        scopes,
        actor,
    });
    Ok(Issued { token, principal })
}

fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Read the file with its [`Stamp`], taken first: a write racing the read
/// leaves a stale stamp, so the next check reads again.
fn load(path: &Path) -> Result<Loaded, AuthError> {
    let stamp = stamp(path)?;
    Ok(Loaded {
        file: read(path)?,
        stamp,
    })
}

fn read(path: &Path) -> Result<AuthFile, AuthError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(AuthFile::default()),
        Err(source) => {
            return Err(AuthError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let file: AuthFile = toml::from_str(&text).map_err(|err| AuthError::Invalid {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    for user in &file.users {
        for scope in &user.scopes {
            scope.parse::<Scope>().map_err(|err| AuthError::Invalid {
                path: path.to_path_buf(),
                reason: format!("user {}: {err}", user.name),
            })?;
        }
    }
    Ok(file)
}

/// Write `state` to `path` atomically (a temporary file renamed over it),
/// readable by its owner only.
fn write(path: &Path, state: &AuthFile) -> Result<(), AuthError> {
    let io = |source| AuthError::Io {
        path: path.to_path_buf(),
        source,
    };
    let text = toml::to_string(state).map_err(|err| AuthError::Invalid {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hord-auth-{tag}-{}.toml", std::process::id()))
    }

    #[test]
    fn a_revoked_token_is_refused_once_the_file_is_reloaded()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_file("revoke");
        let _ = std::fs::remove_file(&path);
        AuthStore::add_user(&path, "ada", "pw", &[Scope::Read])?;
        let store = AuthStore::open(&path)?;
        let key = SigningKey::generate()?.public().key_id();
        let kept = store.login("ada", "pw", &key)?;
        let revoked = store.login("ada", "pw", &key)?;
        assert!(
            !store.reload_if_changed()?,
            "its own writes are not a change"
        );

        // An operator deletes one token from the file.
        let mut file = read(&path)?;
        file.tokens.retain(|t| t.hash != token_hash(&revoked.token));
        write(&path, &file)?;
        assert!(
            store.authenticate_cached(&revoked.token).is_some(),
            "cached"
        );
        assert!(store.reload_if_changed()?);
        assert_eq!(store.authenticate_cached(&revoked.token), None);
        assert_eq!(store.authenticate(&revoked.token)?, None);
        assert_eq!(
            store.authenticate(&kept.token)?,
            Some(kept.principal.clone())
        );
        assert!(!store.reload_if_changed()?);

        // A file that does not parse keeps what was read before.
        std::fs::write(&path, "[[token]\nhash = ")?;
        assert!(store.reload_if_changed().is_err());
        assert_eq!(store.authenticate_cached(&kept.token), Some(kept.principal));
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn login_mint_and_authenticate() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_file("store");
        let _ = std::fs::remove_file(&path);
        let scopes = [Scope::Read, Scope::Review("human".into())];
        AuthStore::add_user(&path, "ada", "pw", &scopes)?;
        assert!(matches!(
            AuthStore::add_user(&path, "ada", "pw", &scopes),
            Err(AuthError::UserExists(_))
        ));
        let store = AuthStore::open(&path)?;
        let key = SigningKey::generate()?;
        let key_id = key.public().key_id();
        assert!(matches!(
            store.login("ada", "wrong", &key_id),
            Err(AuthError::BadLogin)
        ));
        assert!(matches!(
            store.login("bob", "pw", &key_id),
            Err(AuthError::BadLogin)
        ));
        let issued = store.login("ada", "pw", &key_id)?;
        let ada = Actor::Human { id: "ada".into() };
        assert_eq!(issued.principal.actor, ada);
        assert_eq!(issued.principal.scopes, scopes);
        assert_eq!(store.authenticate(&issued.token)?, Some(issued.principal));
        assert_eq!(store.authenticate("hord_nope")?, None);
        assert_eq!(store.key_actor(&key_id)?, Some(ada));

        let agent = Actor::Agent {
            id: "bot".into(),
            model: "m".into(),
            model_hash: Bytes::default(),
            harness: "h".into(),
        };
        let (minted, agent_key) = store.mint(&agent, &[Scope::Read, Scope::Propose])?;
        assert_eq!(
            store.key_actor(&agent_key.public().key_id())?,
            Some(agent.clone())
        );
        // Another process reading the file sees the token.
        let again = AuthStore::open(&path)?;
        let principal = again.authenticate(&minted.token)?.ok_or("minted token")?;
        assert!(same_actor(&principal.actor, &agent));
        assert!(principal.has(&Scope::Propose));
        // The agent's key cannot be claimed by a human login.
        assert!(
            store
                .login("ada", "pw", &agent_key.public().key_id())
                .is_err()
        );
        // Nothing secret is stored.
        let text = std::fs::read_to_string(&path)?;
        assert!(!text.contains(&minted.token) && !text.contains("\"pw\""));
        std::fs::remove_file(&path)?;
        Ok(())
    }
}
