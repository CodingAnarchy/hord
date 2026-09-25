//! `hord user add <name> --auth-file <file> --scope <scope>…` (spec
//! §10.5.4): add a user to a server's local user table. Run by the operator
//! where the server's auth file lives; a running server sees the user at
//! their first login.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hord_api::auth::Scope;
use hord_api::proto;
use hord_server::AuthStore;

use crate::{identity, output};

pub fn run_add(
    json: bool,
    name: String,
    auth_file: PathBuf,
    scopes: Vec<Scope>,
    password_stdin: bool,
) -> Result<()> {
    let password = identity::password(password_stdin, &format!("password for {name}: "))?;
    AuthStore::add_user(&auth_file, &name, &password, &scopes)
        .with_context(|| format!("add user {name}"))?;
    let result = proto::UserAddResult {
        user: name,
        scopes: scopes.iter().map(ToString::to_string).collect(),
        auth_file: auth_file.display().to_string(),
    };
    if json {
        output::print_json(&result)
    } else {
        println!(
            "added user {} ({}) to {}",
            result.user,
            result.scopes.join(", "),
            result.auth_file
        );
        Ok(())
    }
}
