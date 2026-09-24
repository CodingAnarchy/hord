//! JSON vs human output. `--json` is canonical (spec §10.2): the protobuf
//! JSON mapping of a `hord.proto` message (ADR 0024).

use anyhow::{Error, Result};
use serde::Serialize;

/// Print `value` as pretty JSON on stdout.
pub fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print `value` as one line of JSON on stdout (a stream, such as `hord
/// watch --json`).
pub fn print_json_line(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// Print a command failure. JSON (an `ErrorResult`) goes to stderr so
/// agents can parse it.
///
/// A store held by another process past `HORD_LOCK_TIMEOUT` (ADR 0021) sets
/// `kind` to `store_locked`, with the `lock` path, the `holder` pid (if
/// known), and `waitedSecs`, so an agent can tell it apart and retry.
pub fn fail(json: bool, err: &Error) {
    if json {
        let mut payload = hord_api::proto::ErrorResult {
            error: format!("{err:#}"),
            ..Default::default()
        };
        if let Some(hord_store::Error::Locked {
            lock,
            holder,
            waited,
        }) = err
            .chain()
            .find_map(|e| e.downcast_ref::<hord_store::Error>())
        {
            payload.kind = Some("store_locked".into());
            payload.lock = Some(lock.display().to_string());
            payload.holder = *holder;
            payload.waited_secs = Some(waited.as_secs_f64());
        }
        match serde_json::to_string_pretty(&payload) {
            Ok(s) => eprintln!("{s}"),
            Err(_) => eprintln!("error: {err:#}"),
        }
    } else {
        eprintln!("error: {err:#}");
    }
}
