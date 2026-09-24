//! Which repository a command talks to, and how (ADR 0021, ADR 0024 and
//! its amendment).
//!
//! - `--remote <name>`, else the clone's default upstream: a true remote.
//!   [`hord_api::RepoBackend`] commands go to the server; workspace
//!   commands run here over the clone's store as a cache
//!   ([`LocalWorkspaces::remote`]).
//! - Otherwise the repository's daemon, started on demand
//!   ([`crate::daemon`]): both services over its local endpoint.
//! - `--no-daemon` (or `HORD_NO_DAEMON=1`): the store opened in this
//!   process, with ADR 0021's lock wait.

use std::path::Path;

use anyhow::{Context, Result};
use hord_api::{RepoBackend, WorkspacesBackend};
use hord_remote::RemoteRepo;
use hord_txn::{LocalRepo, Repo};

use crate::remotes::Remotes;
use crate::txn::block_on;
use crate::workspaces::LocalWorkspaces;
use crate::{daemon, repo};

/// Global flags that pick the target.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// `--remote`.
    pub remote: Option<String>,
    /// `--no-daemon` or `HORD_NO_DAEMON=1`.
    pub no_daemon: bool,
}

impl Target {
    /// From the command line and the environment.
    pub fn new(remote: Option<String>, no_daemon: bool) -> Self {
        let env = std::env::var("HORD_NO_DAEMON").is_ok_and(|v| !v.is_empty() && v != "0");
        Self {
            remote,
            no_daemon: no_daemon || env,
        }
    }
}

/// An open connection to the repository a command acts on.
pub enum Session {
    /// The repository's daemon.
    Daemon {
        /// Its backend.
        remote: RemoteRepo,
    },
    /// The store, opened here.
    Direct {
        /// The repository.
        repo: Repo,
    },
    /// A true remote, with the clone's store as the object cache.
    Remote {
        /// Its backend.
        remote: RemoteRepo,
        /// The clone's store, reading from `remote`.
        cache: Repo,
    },
}

impl Session {
    /// Open the session `target` asks for, from the current directory.
    pub fn open(target: &Target) -> Result<Self> {
        let root = repo::discover_root()?;
        let hord_dir = root.join(hord_store::HORD_DIR);
        let remotes = Remotes::load(&hord_dir)?;
        let name = target.remote.clone().or(remotes.default.clone());
        if let Some(name) = name {
            return Self::remote(&root, &remotes, &name);
        }
        if target.no_daemon {
            return Self::direct(&root);
        }
        match daemon::connect_or_start(&root)? {
            Some(remote) => Ok(Self::Daemon { remote }),
            // The daemon could not start (for example another process holds
            // the store): work here, waiting for the lock.
            None => Self::direct(&root),
        }
    }

    /// Open the store here, after stopping a running daemon (it holds it).
    pub fn direct(root: &Path) -> Result<Self> {
        daemon::stop(root)?;
        let store = hord_store::Store::open(root)?;
        let repo = block_on(Repo::from_store(store, hord_txn::RepoOptions::default()))?;
        Ok(Self::Direct { repo })
    }

    fn remote(root: &Path, remotes: &Remotes, name: &str) -> Result<Self> {
        let url = remotes.url(name)?.to_owned();
        daemon::stop(root)?;
        let remote = block_on(RemoteRepo::connect(&url))
            .with_context(|| format!("connect to remote {name}"))?;
        let cache = block_on(hord_remote::open_cache(
            root,
            remote.clone(),
            hord_txn::RepoOptions::default(),
        ))?;
        Ok(Self::Remote { remote, cache })
    }

    /// The repository backend.
    pub fn backend(&self) -> Box<dyn RepoBackend> {
        match self {
            Self::Daemon { remote, .. } | Self::Remote { remote, .. } => Box::new(remote.clone()),
            // No lander task: a one-shot command must not land as a side
            // effect (`hord land --local` does that).
            Self::Direct { repo } => Box::new(LocalRepo::without_lander(repo.clone())),
        }
    }

    /// The workspace commands.
    pub fn workspaces(&self) -> Box<dyn WorkspacesBackend> {
        match self {
            Self::Daemon { remote, .. } => Box::new(remote.workspaces()),
            Self::Direct { repo } => Box::new(LocalWorkspaces::local(repo.clone(), None)),
            Self::Remote { remote, cache, .. } => {
                Box::new(LocalWorkspaces::remote(cache.clone(), remote.clone()))
            }
        }
    }
}
