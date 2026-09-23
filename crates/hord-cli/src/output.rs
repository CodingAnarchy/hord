//! JSON vs human output. `--json` is canonical (spec §10.2).

use anyhow::{Error, Result};
use serde::Serialize;

/// Print `value` as pretty JSON on stdout.
pub fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print a command failure. JSON goes to stderr so agents can parse it.
///
/// A store held by another process past `HORD_LOCK_TIMEOUT` (ADR 0021) adds
/// `"kind": "store_locked"`, the `lock` path, the `holder` pid (or null), and
/// `waited_secs`, so an agent can tell it apart and retry.
pub fn fail(json: bool, err: &Error) {
    if json {
        let mut payload = serde_json::json!({ "error": format!("{err:#}") });
        if let Some(hord_store::Error::Locked {
            lock,
            holder,
            waited,
        }) = err
            .chain()
            .find_map(|e| e.downcast_ref::<hord_store::Error>())
        {
            payload["kind"] = "store_locked".into();
            payload["lock"] = lock.display().to_string().into();
            payload["holder"] = (*holder).into();
            payload["waited_secs"] = waited.as_secs_f64().into();
        }
        match serde_json::to_string_pretty(&payload) {
            Ok(s) => eprintln!("{s}"),
            Err(_) => eprintln!("error: {err:#}"),
        }
    } else {
        eprintln!("error: {err:#}");
    }
}
