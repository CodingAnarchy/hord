//! `hord log`

use anyhow::Result;
use serde::Serialize;

use crate::output;
use crate::repo;

#[derive(Debug, Serialize)]
struct LogResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    log: Vec<String>,
}

pub fn run(json: bool) -> Result<()> {
    let store = repo::discover()?;
    let log = store.log()?;
    let result = LogResult {
        head: store.head()?.map(|id| id.to_hex()),
        log: log.iter().map(ToString::to_string).collect(),
    };
    if json {
        output::print_json(&result)?;
    } else if result.log.is_empty() {
        println!("(empty log)");
    } else {
        for id in &result.log {
            println!("{id}");
        }
    }
    Ok(())
}
