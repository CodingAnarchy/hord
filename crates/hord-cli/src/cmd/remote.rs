//! `hord remote add|rm|list|set-default` (spec §10.2, ADR 0024 amendment).

use std::path::PathBuf;

use anyhow::Result;
use hord_api::proto;

use crate::output;
use crate::remotes::Remotes;
use crate::repo;

fn hord_dir() -> Result<PathBuf> {
    Ok(repo::discover_root()?.join(hord_store::HORD_DIR))
}

fn print(json: bool, remotes: &Remotes) -> Result<()> {
    let result = proto::RemotesResult {
        remotes: remotes
            .remotes
            .iter()
            .map(|(name, url)| proto::RemoteInfo {
                name: name.clone(),
                url: url.clone(),
                default: remotes.default.as_deref() == Some(name.as_str()),
            })
            .collect(),
    };
    if json {
        return output::print_json(&result);
    }
    if result.remotes.is_empty() {
        println!("no remotes");
    }
    for remote in &result.remotes {
        let mark = if remote.default { " (default)" } else { "" };
        println!("{} {}{mark}", remote.name, remote.url);
    }
    Ok(())
}

pub fn run_add(json: bool, name: String, url: String, ca_file: Option<PathBuf>) -> Result<()> {
    let dir = hord_dir()?;
    let mut remotes = Remotes::load(&dir)?;
    remotes.add(&name, &url)?;
    if let Some(ca_file) = ca_file {
        remotes.set_ca_file(&name, &ca_file)?;
    }
    remotes.save(&dir)?;
    print(json, &remotes)
}

pub fn run_rm(json: bool, name: String) -> Result<()> {
    let dir = hord_dir()?;
    let mut remotes = Remotes::load(&dir)?;
    remotes.remove(&name)?;
    remotes.save(&dir)?;
    print(json, &remotes)
}

pub fn run_list(json: bool) -> Result<()> {
    print(json, &Remotes::load(&hord_dir()?)?)
}

pub fn run_set_default(json: bool, name: Option<String>, clear: bool) -> Result<()> {
    let dir = hord_dir()?;
    let mut remotes = Remotes::load(&dir)?;
    remotes.set_default(if clear { None } else { name.as_deref() })?;
    remotes.save(&dir)?;
    print(json, &remotes)
}
