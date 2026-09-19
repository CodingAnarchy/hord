//! JSON vs human output. `--json` is canonical (spec §10.2).

use anyhow::{Error, Result};
use serde::Serialize;

/// Print `value` as pretty JSON on stdout.
pub fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print a command failure. JSON goes to stderr so agents can parse it.
pub fn fail(json: bool, err: &Error) {
    if json {
        let payload = serde_json::json!({ "error": format!("{err:#}") });
        match serde_json::to_string_pretty(&payload) {
            Ok(s) => eprintln!("{s}"),
            Err(_) => eprintln!("error: {err:#}"),
        }
    } else {
        eprintln!("error: {err:#}");
    }
}
