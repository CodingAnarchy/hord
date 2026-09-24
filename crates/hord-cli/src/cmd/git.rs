//! `hord git import <ref>` / `hord git export <ref>`

use anyhow::Result;

use crate::git_bridge;
use crate::output;
use crate::repo;

pub fn run_import(json: bool, git_ref: String) -> Result<()> {
    let mut store = repo::discover()?;
    let git_path = git_bridge::sibling_git(&store)?;
    let report = git_bridge::import_git(&mut store, &git_path, Some(&git_ref))?;
    if json {
        output::print_json(&report.message())?;
    } else {
        println!(
            "imported git {} from {} ({} changes)",
            git_ref, report.git_path, report.changes
        );
        if let Some(head) = report.head {
            println!("head {head}");
        }
    }
    Ok(())
}

pub fn run_export(json: bool, hord_ref: String) -> Result<()> {
    let store = repo::discover()?;
    let git_path = git_bridge::sibling_git(&store)?;
    let report = git_bridge::export_tree(&store, &hord_ref, &git_path)?;
    if json {
        output::print_json(&report.message())?;
    } else {
        println!("exported {} to {}", report.hord_ref, report.git_path);
        if let Some(tree) = report.git_tree {
            println!("git_tree {tree}");
        }
        if let Some(commit) = report.git_commit {
            println!("git_commit {commit}");
        }
    }
    Ok(())
}
