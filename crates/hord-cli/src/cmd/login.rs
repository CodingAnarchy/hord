//! `hord login <remote>` (spec §10.5.4, §10.5.5): get a bearer token for a
//! remote and store it in `~/.hord/credentials.toml`.
//!
//! - A human logs in with the server's user table: name and password. Their
//!   signing key (`~/.hord/keys/<user>.pem`, created if missing) is bound to
//!   them on the server.
//! - An agent stores the token and key an operator minted for it (`hord
//!   token mint`): `--token <t> --key-file <pem>`. The server says whose
//!   token it is, and the key must be bound to that actor.
//!
//! OIDC against a configured provider (spec §10.5.4) is future work; the
//! user table serves single-team deployments.

use std::path::{self, PathBuf};

use anyhow::{Context, Result, bail};
use hord_api::{proto, wire};

use crate::identity::{self, Credential, Credentials, StoredActor};
use crate::output;
use crate::session::{self, Target};
use crate::txn::{self, block_on};

/// How to log in.
pub enum Method {
    /// The user table.
    Password {
        /// User name (default: this process's actor id).
        user: Option<String>,
        /// Read the password from stdin.
        stdin: bool,
    },
    /// A minted token and its key.
    Token {
        /// The token.
        token: String,
        /// The key file.
        key_file: PathBuf,
    },
}

pub fn run(json: bool, target: &Target, remote: String, method: Method) -> Result<()> {
    let (name, url) = session::remote_url(target, Some(&remote))?;
    let credential = match method {
        Method::Password { user, stdin } => {
            let user = user.unwrap_or_else(|| txn::actor().id().to_owned());
            let key_file = identity::key_path(&user)?;
            let key = identity::load_or_create_key(&key_file)?;
            let password = identity::password(stdin, &format!("password for {user} at {url}: "))?;
            let remote = session::connect_as(&name, &url, None)?;
            let reply = block_on(remote.auth().login(proto::LoginRequest {
                user,
                password,
                key_id: key.public().key_id(),
            }))
            .context("log in")?;
            let actor = reply.actor.context("the server named no actor")?;
            Credential {
                token: reply.token,
                key_file,
                scopes: reply.scopes,
                actor: StoredActor::from(&wire::actor_from("actor", &actor)?),
            }
        }
        Method::Token { token, key_file } => {
            let key_file = path::absolute(&key_file)
                .with_context(|| format!("key file {}", key_file.display()))?;
            let key = identity::read_key(&key_file)?;
            let remote = session::connect_as(&name, &url, Some(token.clone()))?;
            let me = block_on(remote.auth().who_am_i()).context("check the token")?;
            let Some(actor) = me.actor else {
                bail!("remote {name} does not require auth: there is nothing to log in to");
            };
            let actor = wire::actor_from("actor", &actor)?;
            let key_id = key.public().key_id();
            let bound = block_on(remote.auth().get_key(&key_id))
                .with_context(|| format!("look up key {key_id}"))?;
            let bound = wire::actor_from("actor", &bound.actor.unwrap_or_default())?;
            if StoredActor::from(&bound) != StoredActor::from(&actor) {
                bail!(
                    "key {key_id} is bound to {}, not this token's {}",
                    bound.id(),
                    actor.id()
                );
            }
            Credential {
                token,
                key_file,
                scopes: me.scopes,
                actor: StoredActor::from(&actor),
            }
        }
    };
    let key_id = credential.key()?.public().key_id();
    let mut all = Credentials::load()?;
    all.remotes.insert(url.clone(), credential.clone());
    all.save()?;
    let result = proto::LoginResult {
        remote: name,
        url,
        actor: Some(wire::actor(&credential.actor())),
        scopes: credential.scopes.clone(),
        key_id,
        key_file: credential.key_file.display().to_string(),
    };
    if json {
        output::print_json(&result)
    } else {
        println!(
            "logged in to {} as {} ({})",
            result.remote,
            credential.actor().id(),
            result.scopes.join(", ")
        );
        println!("signing key {} ({})", result.key_id, result.key_file);
        Ok(())
    }
}
