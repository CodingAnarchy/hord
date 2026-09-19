//! `hord init [--from-git <path>]`

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::git_bridge::{self, ImportReport};
use crate::output;
use crate::repo;

#[derive(Debug, Serialize)]
struct InitResult {
    hord_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_git: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported: Option<ImportReport>,
}

pub fn run(json: bool, from_git: Option<PathBuf>) -> Result<()> {
    let cwd = std::env::current_dir().context("current directory")?;
    if let Some(ref git_path) = from_git {
        git_bridge::ensure_git_repo(git_path)?;
    }

    let mut store = repo::create(&cwd)?;
    let mut imported = None;
    if let Some(ref git_path) = from_git {
        imported = Some(git_bridge::import_git(&mut store, git_path, None)?);
    }

    let result = InitResult {
        hord_dir: store.hord_dir().display().to_string(),
        from_git: from_git.as_ref().map(|p| p.display().to_string()),
        imported,
    };

    if json {
        output::print_json(&result)?;
    } else {
        println!("initialized hord repository in {}", result.hord_dir);
        if let Some(imported) = &result.imported {
            match imported.git_ref.as_deref() {
                Some(git_ref) => println!(
                    "imported git {git_ref} from {} ({} changes)",
                    imported.git_path, imported.changes
                ),
                None => println!(
                    "imported git history from {} ({} changes)",
                    imported.git_path, imported.changes
                ),
            }
        }
    }
    Ok(())
}
