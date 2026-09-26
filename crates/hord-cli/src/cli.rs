//! Clap surface for the CLI (spec §10.2).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use hord_api::auth::Scope;

/// Semantic VCS.
#[derive(Debug, Parser)]
#[command(
    name = "hord",
    version,
    about = "Hord: a transactional database over a semantic code graph",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Emit JSON (canonical agent output): the protobuf JSON mapping of
    /// the command's `hord.proto` message.
    #[arg(long, global = true)]
    pub json: bool,

    /// Run against this configured remote (`hord remote add`) instead of
    /// the local repository or its default upstream.
    #[arg(long, global = true, value_name = "NAME")]
    pub remote: Option<String>,

    /// Open the store in this process instead of talking to the
    /// repository's daemon (ADR 0021). Also `HORD_NO_DAEMON=1`.
    #[arg(long, global = true)]
    pub no_daemon: bool,

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
        /// Re-hash every file instead of trusting the stat index (ADR 0016).
        #[arg(long)]
        paranoid: bool,
    },
    /// Run the checks head's policy requires for a workspace's proposal and
    /// attach the evidence to its snapshot (spec §10.2, §10.3).
    Verify {
        /// Workspace id.
        #[arg(short = 'w', long = "workspace", value_name = "WS")]
        workspace: Option<String>,
        /// Print the plan (with reuse) without running anything.
        #[arg(long)]
        plan_only: bool,
    },
    /// Build a change record from a workspace (spec §6.2) and print its id.
    Propose {
        /// Workspace id.
        #[arg(short = 'w', long = "workspace", value_name = "WS")]
        workspace: Option<String>,
        /// Intent file: Markdown with YAML front matter (`summary`, `refs`,
        /// `acceptance`, optional `reads`).
        #[arg(long, value_name = "FILE")]
        intent: PathBuf,
    },
    /// Send a proposed change to the lander queue.
    Submit {
        /// Change id (64 hex digits).
        #[arg(value_name = "CHANGE")]
        change: String,
    },
    /// Show the lander queue.
    Queue {
        /// Only changes whose actor is `HORD_ACTOR` (else `USER`).
        #[arg(long)]
        mine: bool,
    },
    /// Run the lander until the queue is empty (single-user mode). With a
    /// daemon, wait for its lander instead.
    Land {
        /// Land in this repository (required).
        #[arg(long)]
        local: bool,
        /// Submit this change first.
        #[arg(value_name = "CHANGE")]
        change: Option<String>,
    },
    /// Explain a change's conflict report.
    Conflicts {
        /// Change id (64 hex digits), as submitted or as landed.
        #[arg(value_name = "CHANGE")]
        change: String,
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
    /// Configured remotes (hord servers).
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Tail the event stream (spec §10.5.3).
    Watch {
        /// Only lander queue events.
        #[arg(long)]
        queue: bool,
        /// Only events about this change; exit once it lands, parks, or is
        /// rejected.
        #[arg(long, value_name = "CHANGE")]
        change: Option<String>,
        /// Resume after this event cursor (0 replays everything).
        #[arg(long, value_name = "CURSOR")]
        from: Option<u64>,
    },
    /// Run the hord server (spec §10.5.1).
    Serve {
        /// Host this repository (default: the current one).
        #[arg(long, value_name = "PATH", conflicts_with = "root")]
        repo: Option<PathBuf>,
        /// Host every repository under this directory, at `/r/<name>/`.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// Address to listen on (default: server.toml's, else 127.0.0.1:7878).
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
        /// Allow a non-loopback address without TLS (tokens travel in the
        /// clear). Not needed with TLS.
        #[arg(long)]
        insecure_bind: bool,
        /// Serve TLS with this PEM certificate chain (ADR 0032; default:
        /// server.toml's `[tls] cert`). Needs `--tls-key`.
        #[arg(long, value_name = "FILE", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// The PEM private key for `--tls-cert`.
        #[arg(long, value_name = "FILE", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Require bearer tokens, checked against this auth file (spec
        /// §10.5.4; default: server.toml's `[auth] file`, else none).
        #[arg(long, value_name = "FILE")]
        auth: Option<PathBuf>,
        /// `server.toml` (default: `<repo>/.hord/server.toml` or
        /// `<root>/server.toml`).
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Run as the repository's daemon: listen on its local endpoint and
        /// exit when idle (started by the CLI on demand).
        #[arg(long, hide = true)]
        daemon: bool,
    },
    /// Sign review evidence against a change's result snapshot and attach
    /// it (spec §7.2, ADR 0026).
    Review {
        /// Change id (64 hex digits).
        #[arg(value_name = "CHANGE")]
        change: String,
        /// Reviewer kind: the evidence's qualifier, met by
        /// `review:<kind>` requirements (`human`, `agent-reviewer`, …).
        #[arg(long = "as", value_name = "KIND")]
        kind: String,
        /// Approve (Pass evidence).
        #[arg(long, conflicts_with = "reject", required_unless_present = "reject")]
        approve: bool,
        /// Reject (Fail evidence, with the message as its summary).
        #[arg(long)]
        reject: bool,
        /// Review message.
        #[arg(short = 'm', long = "message", value_name = "MSG")]
        message: String,
    },
    /// Log in to a remote: store a bearer token for it (spec §10.5.4).
    Login {
        /// Remote name, or its http:// address.
        #[arg(value_name = "REMOTE")]
        remote: String,
        /// User name in the server's user table (default: `HORD_ACTOR`,
        /// else `USER`).
        #[arg(long, value_name = "NAME", conflicts_with = "token")]
        user: Option<String>,
        /// Read the password from stdin instead of the terminal.
        #[arg(long, conflicts_with = "token")]
        password_stdin: bool,
        /// Store this minted agent token instead (`hord token mint`).
        #[arg(long, value_name = "TOKEN", requires = "key_file")]
        token: Option<String>,
        /// The minted token's signing key (PKCS#8 PEM).
        #[arg(long, value_name = "FILE", requires = "token")]
        key_file: Option<PathBuf>,
    },
    /// Agent tokens (spec §10.5.4).
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// A server's local user table (spec §10.5.4).
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Signing keys (spec §10.5.4).
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Git import/export.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// Policy commands (spec §7.2).
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Run the replay protocol on a change by hand (spec §6.6): give the
    /// harness a workspace on head, and submit what it proposes as a
    /// replay. Needs the repository's daemon or a remote (the harness
    /// proposes through it).
    Replay {
        /// The conflicted change (64 hex digits).
        #[arg(value_name = "CHANGE")]
        change: String,
        /// The harness command, run by the shell: it reads a ReplayRequest
        /// line on stdin and writes a ReplayResult line on stdout.
        #[arg(long, value_name = "CMD")]
        harness: String,
        /// Wall-clock budget; the harness is killed past it. Default: head's
        /// `[replay] budget` (ADR 0028), like each budget flag.
        #[arg(long, value_name = "SECS")]
        wall_time_secs: Option<u64>,
        /// Token budget; a result reporting more is rejected.
        #[arg(long, value_name = "N")]
        tokens: Option<u64>,
        /// Cost budget in US dollars; a result reporting more is rejected.
        #[arg(long, value_name = "USD")]
        cost_usd: Option<f64>,
        /// A note for the harness, added to the request.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
    /// Resolve a parked change (spec §6.4 rung 3). The resolution lands as a
    /// change whose parents include both colliding changes.
    #[command(group(clap::ArgGroup::new("how").required(true).args(["pick", "edit", "replay"])))]
    Arbitrate {
        /// The parked change (64 hex digits).
        #[arg(value_name = "CHANGE")]
        change: String,
        /// `ours` (keep what landed), `theirs` (take the parked change), or
        /// a change id that resolves it, such as a replay candidate.
        /// `theirs` merges the parked change onto head definition by
        /// definition and takes the parked side of each contested
        /// definition, keeping what head gained elsewhere in the same
        /// files. Files hord cannot merge by definition (blob-tier or
        /// unparseable) are taken whole, and `hord conflicts` lists them.
        #[arg(long, value_name = "ours|theirs|CHANGE")]
        pick: Option<String>,
        /// Open a workspace on head to resolve it by hand, then `hord
        /// propose` there and `hord arbitrate <change> --pick <proposed>`.
        #[arg(long)]
        edit: bool,
        /// Hand it to the repository's replay harness once more.
        #[arg(long)]
        replay: bool,
        /// With `--replay`: a note for the harness.
        #[arg(long, value_name = "TEXT", requires = "replay")]
        note: Option<String>,
    },
}

/// `hord token` subcommands.
#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Mint a token bound to an agent, with a new signing key (needs an
    /// admin login on the remote).
    Mint {
        /// Agent id.
        #[arg(long, value_name = "ID")]
        agent: String,
        /// Model the agent runs.
        #[arg(long, value_name = "MODEL")]
        model: String,
        /// Harness that runs it.
        #[arg(long, value_name = "HARNESS")]
        harness: String,
        /// Scope to grant (repeat): read, propose, review:KIND,
        /// arbitrate, admin.
        #[arg(long = "scope", value_name = "SCOPE", required = true)]
        scopes: Vec<Scope>,
        /// Write the private key here instead of printing it.
        #[arg(long, value_name = "FILE")]
        key_out: Option<PathBuf>,
    },
}

/// `hord user` subcommands.
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Add a user to an auth file (run where the server's auth file is).
    Add {
        /// User name: the human actor id their changes carry.
        #[arg(value_name = "NAME")]
        name: String,
        /// The server's auth file (`[auth] file` in server.toml).
        #[arg(long, value_name = "FILE")]
        auth_file: PathBuf,
        /// Scope a login grants (repeat).
        #[arg(long = "scope", value_name = "SCOPE", required = true)]
        scopes: Vec<Scope>,
        /// Read the password from stdin instead of the terminal.
        #[arg(long)]
        password_stdin: bool,
    },
}

/// `hord key` subcommands.
#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Print a key id from `~/.hord/keys/`, creating the key if missing.
    Show {
        /// Key name (default: `HORD_ACTOR`, else `USER`).
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// Verify a signed change or evidence object. Exits 1 if it does not
    /// verify.
    Verify {
        /// Object id (64 hex digits).
        #[arg(value_name = "OBJECT")]
        object: String,
        /// Key id (`ed25519:<hex>`) to check against (default: the one the
        /// signature names, which proves only that it signed).
        #[arg(long, value_name = "KEY_ID")]
        key: Option<String>,
    },
}

/// `hord policy` subcommands.
#[derive(Debug, Subcommand)]
pub enum PolicyCommand {
    /// Dry-run a policy against a workspace's current proposal. Exits 1 on
    /// deny.
    Check {
        /// Workspace id.
        #[arg(short = 'w', long = "workspace", value_name = "WS")]
        workspace: Option<String>,
        /// Evaluate this policy file instead of head's `.hord-policy.toml`.
        #[arg(long, value_name = "FILE")]
        policy: Option<PathBuf>,
    },
}

/// `hord ws` subcommands.
#[derive(Debug, Subcommand)]
pub enum WsCommand {
    /// Create a workspace and print its id and materialization path.
    New {
        /// Base snapshot or change id (hex), ref name, or `<remote>/<ref>`.
        #[arg(long, value_name = "SNAP|REF")]
        base: Option<String>,
        /// `clone` (copy-on-write, falls back to copy) or `copy` (ADR 0016).
        #[arg(long, value_name = "MODE", default_value = "clone")]
        materialize: Materialize,
    },
    /// List workspaces.
    List,
    /// Delete a workspace and its checkout.
    Rm {
        /// Workspace id.
        #[arg(value_name = "WS")]
        id: String,
    },
    /// Remove pristine checkouts no workspace uses (ADR 0016).
    Gc,
}

/// `hord remote` subcommands.
#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Add a remote: `http[s]://host:port`, or `http[s]://host:port/r/<name>`.
    Add {
        /// Its name.
        #[arg(value_name = "NAME")]
        name: String,
        /// Its address.
        #[arg(value_name = "URL")]
        url: String,
        /// For `https`: a PEM CA certificate to trust besides the system's
        /// roots, such as a team host's own CA (ADR 0032).
        #[arg(long, value_name = "FILE")]
        ca_file: Option<PathBuf>,
    },
    /// Remove a remote.
    Rm {
        /// Its name.
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// List remotes.
    List,
    /// Make a remote the default upstream (commands use it without
    /// `--remote`); `--clear` removes the default.
    SetDefault {
        /// Its name.
        #[arg(value_name = "NAME", required_unless_present = "clear")]
        name: Option<String>,
        /// Remove the default upstream.
        #[arg(long)]
        clear: bool,
    },
}

/// How `hord ws new` materializes the checkout (ADR 0016).
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Materialize {
    /// Copy-on-write clone of the pristine checkout; a copy where the
    /// filesystem cannot clone.
    Clone,
    /// Plain copy.
    Copy,
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
