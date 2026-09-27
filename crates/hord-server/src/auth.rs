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
//!
//! [[lander]]                     # a lander key `hord audit` trusts (ADR 0038)
//! id = "ed25519:<hex>"
//! ```
//!
//! Other processes (`hord user add`, an operator's editor) may write the
//! file while a server runs: every change re-reads it first, and a token or
//! key the server has not seen makes it re-read before refusing. Removing a
//! token or key takes effect at the next [`AuthStore::reload_if_changed`]
//! (a running server checks every second) or [`AuthStore::reload`] (`hord
//! serve` on SIGHUP), without a restart.

use std::fs::OpenOptions;
use std::io::Write as _;
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
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

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
    #[serde(default, rename = "lander")]
    landers: Vec<Lander>,
}

/// A lander key whose signatures on `Landed` events the audit trusts
/// (ADR 0038). Kept after a rotation, so older landings still verify.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Lander {
    id: String,
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
    /// When it was revoked (RFC 3339, ADR 0038): refused from then on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Key {
    id: String,
    actor: StoredActor,
    /// When it was revoked (RFC 3339, ADR 0038): a signature it made
    /// before then still verifies; none made after does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<String>,
}

/// A key's binding (ADR 0038): the actor it is bound to, and when it was
/// revoked, if it was.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    /// The actor.
    pub actor: Actor,
    /// When it was revoked, in ms since the Unix epoch.
    pub revoked_at_ms: Option<u64>,
}

impl KeyBinding {
    /// Whether a signature made at `at_ms` counts: before any revocation.
    #[must_use]
    pub fn valid_at(&self, at_ms: u64) -> bool {
        self.revoked_at_ms.is_none_or(|revoked| at_ms < revoked)
    }
}

/// `revoked_at` in ms since the Unix epoch; `read` checked that it parses.
fn revoked_ms(revoked_at: Option<&str>) -> Option<u64> {
    revoked_at.map(|at| parse_time(at).unwrap_or(0))
}

/// An RFC 3339 time in ms since the Unix epoch.
fn parse_time(at: &str) -> Option<u64> {
    let parsed = OffsetDateTime::parse(at, &Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Now, in ms since the Unix epoch.
fn now_ms() -> u64 {
    u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Now, as `revoked_at` stores it.
fn now_rfc3339() -> Result<String, AuthError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| AuthError::Invalid {
            path: PathBuf::new(),
            reason: err.to_string(),
        })
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

    /// [`Self::open`], or when the file cannot be read now, an empty table
    /// that the next [`Self::reload_if_changed`] reads again, and why it
    /// could not be read. For a store only audits read (a daemon's key
    /// bindings), which must not give up on the file for good.
    pub fn open_retrying(path: &Path) -> (Self, Option<AuthError>) {
        match Self::open(path) {
            Ok(store) => (store, None),
            Err(err) => {
                let store = Self {
                    path: path.to_path_buf(),
                    state: Mutex::new(Loaded {
                        file: AuthFile::default(),
                        // No file has this stamp: the next check reads it.
                        stamp: Some((None, u64::MAX)),
                    }),
                };
                (store, Some(err))
            }
        }
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

    /// `key_id`'s binding, revoked or not, as this process last read the
    /// file: no I/O. For judging a signature by when it was made (ADR
    /// 0038).
    #[must_use]
    pub fn key_binding(&self, key_id: &str) -> Option<KeyBinding> {
        let state = self.lock();
        let key = state.keys.iter().find(|k| k.id == key_id)?;
        Some(KeyBinding {
            actor: Actor::from(&key.actor),
            revoked_at_ms: revoked_ms(key.revoked_at.as_deref()),
        })
    }

    /// `key_id`'s binding, revoked or not ([`Self::key_binding`]),
    /// re-reading the file (blocking) for a key not seen yet.
    pub fn key_binding_fresh(&self, key_id: &str) -> Result<Option<KeyBinding>, AuthError> {
        if let Some(binding) = self.key_binding(key_id) {
            return Ok(Some(binding));
        }
        *self.lock() = load(&self.path)?;
        Ok(self.key_binding(key_id))
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

    /// Whether `key_id` was a bridge's key at `at_ms` (ADR 0038): bound
    /// then, and its actor then held a `bridge` token. As this process
    /// last read the file: no I/O.
    #[must_use]
    pub fn bridge_key_at(&self, key_id: &str, at_ms: u64) -> bool {
        let state = self.lock();
        let Some(key) = state.keys.iter().find(|k| k.id == key_id) else {
            return false;
        };
        let live = |revoked_at: Option<&str>| revoked_ms(revoked_at).is_none_or(|r| at_ms < r);
        live(key.revoked_at.as_deref())
            && state.tokens.iter().any(|t| {
                t.actor == key.actor
                    && live(t.revoked_at.as_deref())
                    && t.scopes.iter().any(|s| s == "bridge")
            })
    }

    /// Whether `key_id` is a lander key the file lists (ADR 0038), as this
    /// process last read it: no I/O.
    #[must_use]
    pub fn lander_listed(&self, key_id: &str) -> bool {
        self.lock().landers.iter().any(|l| l.id == key_id)
    }

    /// List `key_id` as a lander key (ADR 0038); nothing if it is listed.
    /// Whether it was added.
    pub fn add_lander(&self, key_id: &str) -> Result<bool, AuthError> {
        PublicKey::from_key_id(key_id).map_err(|err| AuthError::Rejected(err.to_string()))?;
        if self.lander_listed(key_id) {
            return Ok(false);
        }
        self.update(|state| {
            if state.landers.iter().any(|l| l.id == key_id) {
                return Ok(false);
            }
            state.landers.push(Lander {
                id: key_id.to_owned(),
            });
            Ok(true)
        })
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

    /// Revoke `key_id` in the auth file at `path` now (`hord key revoke`,
    /// ADR 0038): it signs nothing from now on, and what it signed before
    /// still verifies. Its entry stays. Revoking a revoked key keeps the
    /// first time.
    pub fn revoke_key(path: &Path, key_id: &str) -> Result<(), AuthError> {
        let mut state = read(path)?;
        let key = state
            .keys
            .iter_mut()
            .find(|k| k.id == key_id)
            .ok_or_else(|| AuthError::Rejected(format!("no key {key_id} in {}", path.display())))?;
        if key.revoked_at.is_none() {
            key.revoked_at = Some(now_rfc3339()?);
        }
        write(path, &state)
    }

    /// Revoke every token of the actor `actor_id` in the auth file at
    /// `path` now (`hord token revoke`, ADR 0038). Their entries stay. The
    /// number revoked.
    pub fn revoke_tokens(path: &Path, actor_id: &str) -> Result<usize, AuthError> {
        let mut state = read(path)?;
        let now = now_rfc3339()?;
        let mut revoked = 0;
        for token in &mut state.tokens {
            let id = match &token.actor {
                StoredActor::Human { id } | StoredActor::Agent { id, .. } => id,
            };
            if id == actor_id && token.revoked_at.is_none() {
                token.revoked_at = Some(now.clone());
                revoked += 1;
            }
        }
        if revoked == 0 {
            return Err(AuthError::Rejected(format!(
                "no unrevoked token of {actor_id} in {}",
                path.display()
            )));
        }
        write(path, &state)?;
        Ok(revoked)
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
    let now = now_ms();
    let token = state
        .tokens
        .iter()
        .find(|t| t.hash == hash && revoked_ms(t.revoked_at.as_deref()).is_none_or(|r| now < r))?;
    Some(Principal {
        actor: Actor::from(&token.actor),
        // Scopes were checked when issued; one edited into something
        // unknown grants nothing.
        scopes: token.scopes.iter().filter_map(|s| s.parse().ok()).collect(),
    })
}

/// The actor `key_id` is bound to, unless it is revoked: what may sign
/// now.
fn find_key(state: &AuthFile, key_id: &str) -> Option<Actor> {
    let now = now_ms();
    state
        .keys
        .iter()
        .find(|k| k.id == key_id && revoked_ms(k.revoked_at.as_deref()).is_none_or(|r| now < r))
        .map(|k| Actor::from(&k.actor))
}

/// Bind `key_id` to `actor`; a key already bound to another actor is
/// refused.
fn bind_key(state: &mut AuthFile, key_id: &str, actor: &StoredActor) -> Result<(), AuthError> {
    match state.keys.iter().find(|k| k.id == key_id) {
        Some(key) if key.revoked_at.is_some() => Err(AuthError::Rejected(format!(
            "key {key_id} was revoked; use a new key"
        ))),
        Some(key) if &key.actor == actor => Ok(()),
        Some(_) => Err(AuthError::Rejected(format!(
            "key {key_id} is bound to another actor"
        ))),
        None => {
            state.keys.push(Key {
                id: key_id.to_owned(),
                actor: actor.clone(),
                revoked_at: None,
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
        revoked_at: None,
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
    let times = file
        .tokens
        .iter()
        .map(|t| t.revoked_at.as_deref())
        .chain(file.keys.iter().map(|k| k.revoked_at.as_deref()));
    for at in times.flatten() {
        if parse_time(at).is_none() {
            return Err(AuthError::Invalid {
                path: path.to_path_buf(),
                reason: format!("revoked_at {at:?} is not an RFC 3339 time"),
            });
        }
    }
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
    // A new file, owner-only from the start: never readable by others, and
    // never a leftover (or a symlink planted in its place) written through.
    match std::fs::remove_file(&tmp) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(io(err)),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).map_err(io)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io)?;
    drop(file);
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
    fn revoking_keeps_the_entry_and_dates_it() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_file("revoke-at");
        let _ = std::fs::remove_file(&path);
        AuthStore::add_user(&path, "ada", "pw", &[Scope::Read])?;
        let store = AuthStore::open(&path)?;
        let key = SigningKey::generate()?.public().key_id();
        let issued = store.login("ada", "pw", &key)?;
        let before = now_ms();

        AuthStore::revoke_key(&path, &key)?;
        assert_eq!(AuthStore::revoke_tokens(&path, "ada")?, 1);
        assert!(AuthStore::revoke_tokens(&path, "ada").is_err(), "none left");
        store.reload()?;
        let binding = store.key_binding(&key).ok_or("the key's entry stays")?;
        assert_eq!(binding.actor, Actor::Human { id: "ada".into() });
        let revoked = binding.revoked_at_ms.ok_or("dated")?;
        assert!(revoked >= before);
        assert!(binding.valid_at(revoked - 1), "signed before: valid");
        assert!(!binding.valid_at(revoked), "signed after: not");
        // Nothing signs or authenticates with them now.
        assert_eq!(store.key_actor(&key)?, None);
        assert_eq!(store.authenticate(&issued.token)?, None);
        assert!(matches!(
            store.login("ada", "pw", &key),
            Err(AuthError::Rejected(_))
        ));

        // A revocation time that does not parse is an invalid file.
        let text = std::fs::read_to_string(&path)?;
        let (head, tail) = text.split_once("revoked_at = \"").ok_or("a revoked_at")?;
        let tail = tail.split_once('"').ok_or("quoted")?.1;
        std::fs::write(&path, format!("{head}revoked_at = \"yesterday\"{tail}"))?;
        assert!(matches!(
            AuthStore::open(&path),
            Err(AuthError::Invalid { .. })
        ));
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_written_owner_only_never_through_a_planted_link()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let path = temp_file("planted");
        let _ = std::fs::remove_file(&path);
        let elsewhere = temp_file("elsewhere");
        std::fs::write(&elsewhere, "")?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let _ = std::fs::remove_file(&tmp);
        symlink(&elsewhere, &tmp)?;

        AuthStore::add_user(&path, "ada", "pw", &[Scope::Read])?;
        assert_eq!(
            std::fs::read_to_string(&elsewhere)?,
            "",
            "not written through"
        );
        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        for file in [&path, &elsewhere, &tmp] {
            let _ = std::fs::remove_file(file);
        }
        Ok(())
    }

    #[test]
    fn a_store_that_could_not_open_reads_the_file_once_it_parses()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_file("retrying");
        std::fs::write(&path, "[[user]\n")?;
        let (store, err) = AuthStore::open_retrying(&path);
        assert!(matches!(err, Some(AuthError::Invalid { .. })), "{err:?}");
        assert!(store.reload_if_changed().is_err(), "still broken");
        std::fs::remove_file(&path)?;
        AuthStore::add_user(&path, "ada", "pw", &[Scope::Read])?;
        assert!(store.reload_if_changed()?, "read once it parses");
        let key = SigningKey::generate()?.public().key_id();
        store.login("ada", "pw", &key)?;
        assert!(store.key_binding(&key).is_some());
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
