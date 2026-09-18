//! Clap surface for M0 commands (spec §10.2).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Semantic VCS.
#[derive(Debug, Parser)]
#[command(
    name = "hord",
    version,
    about = "Hord: a transactional database over a semantic code graph",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Emit JSON (canonical agent output).
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// M0 subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a repository (optionally importing git history).
    Init {
        /// Import git history from this repository.
        #[arg(long, value_name = "PATH")]
        from_git: Option<PathBuf>,
    },
    /// Workspace commands.
    Ws {
        #[command(subcommand)]
        command: WsCommand,
    },
    /// Show workspace status: ops, read/write sets, evidence staleness.
    Status {
        /// Workspace id.
        #[arg(short = 'w', long = "workspace", value_name = "WS")]
        workspace: Option<String>,
    },
    /// Show the landed change log.
    Log,
    /// Git import/export.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
}

/// `hord ws` subcommands.
#[derive(Debug, Subcommand)]
pub enum WsCommand {
    /// Create a workspace and print its id and materialization path.
    New {
        /// Base snapshot id (hex) or ref name.
        #[arg(long, value_name = "SNAP|REF")]
        base: Option<String>,
    },
}

/// `hord git` subcommands.
#[derive(Debug, Subcommand)]
pub enum GitCommand {
    /// Import a git ref into the hord log.
    Import {
        /// Git ref to import (for example `HEAD` or `main`).
        #[arg(value_name = "REF")]
        git_ref: String,
    },
    /// Export a hord ref as a git tree.
    Export {
        /// Hord ref or snapshot to export (for example `head` or `main`).
        #[arg(value_name = "REF")]
        hord_ref: String,
    },
}
