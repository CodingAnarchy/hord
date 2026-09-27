//! `hord token mint --agent <id> --model <m> --harness <h> --scope <s>…`
//! (spec §10.5.4): an operator (`admin` token) mints a token bound to an
//! agent actor, with a new signing key. Hand both to the agent, which runs
//! `hord login <remote> --token <t> --key-file <pem>`.
//!
//! `hord token revoke --actor <id> --auth-file <file>` revokes an actor's
//! tokens in the server's auth file (ADR 0038).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hord_api::auth::Scope;
use hord_api::proto;
use hord_core::sign::SigningKey;
use hord_server::AuthStore;

use crate::session::{self, Target};
use crate::txn::block_on;
use crate::{identity, output};

pub struct Mint {
    pub agent: String,
    pub model: String,
    pub harness: String,
    pub scopes: Vec<Scope>,
    pub key_out: Option<PathBuf>,
}

pub fn run_mint(json: bool, target: &Target, mint: Mint) -> Result<()> {
    let (name, url) = session::remote_url(target, None)?;
    let (remote, credential) = session::connect(&name, &url)?;
    if credential.is_none() {
        bail!("not logged in to {name}: `hord login {name}` with an admin user first");
    }
    let mut reply = block_on(remote.auth().mint_token(proto::MintTokenRequest {
        agent_id: mint.agent,
        model: mint.model,
        harness: mint.harness,
        scopes: mint.scopes.iter().map(ToString::to_string).collect(),
    }))
    .context("mint a token")?;
    if let Some(path) = &mint.key_out {
        let key = SigningKey::from_pem(&reply.private_key_pem)?;
        identity::write_key(path, &key)?;
        // Written to the file; not printed.
        reply.private_key_pem.clear();
    }
    if json {
        return output::print_json(&reply);
    }
    println!("token {}", reply.token);
    println!("scopes {}", reply.scopes.join(", "));
    println!("key {}", reply.key_id);
    match &mint.key_out {
        Some(path) => println!("private key written to {}", path.display()),
        None => print!("{}", reply.private_key_pem),
    }
    Ok(())
}

pub fn run_revoke(json: bool, actor: &str, auth_file: &Path) -> Result<()> {
    let revoked = AuthStore::revoke_tokens(auth_file, actor)
        .with_context(|| format!("revoke the tokens of {actor}"))?;
    let result = proto::TokenRevokeResult {
        actor: actor.to_owned(),
        revoked: u32::try_from(revoked).unwrap_or(u32::MAX),
        auth_file: auth_file.display().to_string(),
    };
    if json {
        return output::print_json(&result);
    }
    println!(
        "revoked {} token(s) of {} in {}",
        result.revoked, result.actor, result.auth_file
    );
    Ok(())
}
