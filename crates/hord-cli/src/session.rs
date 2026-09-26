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

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use hord_api::{RepoBackend, WorkspacesBackend};
use hord_core::Actor;
use hord_core::sign::SigningKey;
use hord_remote::{ConnectOptions, RemoteRepo};
use hord_txn::{LocalRepo, Repo};

use crate::identity::{self, Credential, Credentials};
use crate::remotes::Remotes;
use crate::txn::{self, block_on};
use crate::workspaces::LocalWorkspaces;
use crate::{daemon, repo};

/// An actor and the key that signs for them (spec §10.5.4).
#[derive(Clone, Debug)]
pub struct Signer {
    /// Who.
    pub actor: Actor,
    /// Their key.
    pub key: Arc<SigningKey>,
}

/// This process's actor ([`txn::actor`]) and their key in
/// `~/.hord/keys/<id>.pem` (`HORD_HOME` honored), created on first use.
pub fn local_signer() -> Result<Signer> {
    let actor = txn::actor();
    let key = identity::load_or_create_key(&identity::key_path(actor.id())?)?;
    Ok(Signer {
        actor,
        key: Arc::new(key),
    })
}

/// Connect to the remote `name` at `url`, with the token `hord login`
/// stored for it, if any.
pub fn connect(name: &str, url: &str) -> Result<(RemoteRepo, Option<Credential>)> {
    let credential = Credentials::for_url(url)?;
    let token = credential.as_ref().map(|c| c.token.clone());
    let remote = connect_as(name, url, token)?;
    Ok((remote, credential))
}

/// Connect to the remote `name` at `url`, sending `token` if given. An
/// `https` address trusts the system's roots plus the remote's CA file
/// (`hord remote add --ca-file`) or `HORD_CA_FILE` (ADR 0032).
pub fn connect_as(name: &str, url: &str, token: Option<String>) -> Result<RemoteRepo> {
    let remotes = match repo::discover_root() {
        Ok(root) => Remotes::load(&root.join(hord_store::HORD_DIR))?,
        Err(_) => Remotes::default(),
    };
    let ca_pem = remotes
        .ca_file_for(url)
        .map(|file| {
            std::fs::read(&file).with_context(|| format!("read CA file {}", file.display()))
        })
        .transpose()?;
    block_on(RemoteRepo::connect_with(
        url,
        &ConnectOptions { token, ca_pem },
    ))
    .with_context(|| format!("connect to remote {name}"))
}

/// The remote a command that only makes sense against a server targets:
/// `--remote`, else the clone's default upstream, else `name` given on the
/// command line as a name or an `http[s]://` address. Returns its name and
/// address.
pub fn remote_url(target: &Target, name: Option<&str>) -> Result<(String, String)> {
    if let Some(url) = name.filter(|n| n.starts_with("http://") || n.starts_with("https://")) {
        return Ok((url.to_owned(), url.to_owned()));
    }
    let root = repo::discover_root()?;
    let remotes = Remotes::load(&root.join(hord_store::HORD_DIR))?;
    let name = name
        .map(str::to_owned)
        .or_else(|| target.remote.clone())
        .or_else(|| remotes.default.clone())
        .ok_or_else(|| anyhow!("no remote: name one, or pass --remote"))?;
    let url = remotes.url(&name)?.to_owned();
    Ok((name, url))
}

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
        /// What `hord login` stored for it, if anything: the token sent
        /// and the key that signs.
        credential: Option<Credential>,
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
        let (remote, credential) = connect(name, &url)?;
        let cache = block_on(hord_remote::open_cache(
            root,
            remote.clone(),
            hord_txn::RepoOptions::default(),
        ))?;
        Ok(Self::Remote {
            remote,
            cache,
            credential,
        })
    }

    /// Who acts in this session, and the key that signs for them: the
    /// logged-in actor for a remote with a stored token, else this
    /// process's actor ([`txn::actor`]) and their key in `~/.hord/keys/`,
    /// created if missing.
    pub fn signer(&self) -> Result<Signer> {
        if let Self::Remote {
            credential: Some(credential),
            ..
        } = self
        {
            return Ok(Signer {
                actor: credential.actor(),
                key: Arc::new(credential.key()?),
            });
        }
        local_signer()
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
            Self::Remote {
                remote,
                cache,
                credential,
            } => {
                let signer = credential.as_ref().and_then(|c| {
                    Some(Signer {
                        actor: c.actor(),
                        key: Arc::new(c.key().ok()?),
                    })
                });
                Box::new(LocalWorkspaces::remote(
                    cache.clone(),
                    remote.clone(),
                    signer,
                ))
            }
        }
    }
}
