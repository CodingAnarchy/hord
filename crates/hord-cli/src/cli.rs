//! Clap surface for the CLI (spec §10.2).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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

/// Subcommands.
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
    Log {
        /// NodeId (ULID or 32-digit hex) or qualified name.
        #[arg(long, value_name = "NODE")]
        node: Option<String>,
        /// Repository path. Keeps changes whose write set touches it.
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Actor id (`Actor::Human` or `Actor::Agent` id).
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
        /// Unix time in milliseconds. Keeps changes created at or after this instant.
        #[arg(long, value_name = "MS")]
        since: Option<u64>,
    },
    /// Semantic blame for a definition: change, intent, actor, evidence ids.
    Blame {
        /// Qualified name, `path:line` (1-based), or NodeId (ULID or 32-digit hex).
        #[arg(value_name = "NAME|PATH:LINE")]
        target: String,
    },
    /// Targets of one edge kind leaving a node, in the latest snapshot.
    Query {
        /// `references`, `dependents`, or `tests-of`.
        #[arg(value_name = "EDGE")]
        edge: QueryEdge,
        /// Source NodeId (ULID or 32-digit hex).
        #[arg(value_name = "NODE")]
        node: String,
    },
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

/// Edge kind for `hord query` (spec §3.8, §10.2).
///
/// `dependents` is [`hord_store::EdgeKind::Depends`] and `tests-of` is
/// [`hord_store::EdgeKind::Tests`]. The node argument is the source; the
/// command prints targets.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum QueryEdge {
    /// [`hord_store::EdgeKind::References`].
    References,
    /// [`hord_store::EdgeKind::Depends`].
    Dependents,
    /// [`hord_store::EdgeKind::Tests`].
    TestsOf,
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
