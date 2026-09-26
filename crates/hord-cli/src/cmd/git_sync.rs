//! `hord git sync [--once | --check | --repair]`: the git bridge (spec §9,
//! ADR 0036, ADR 0037). Setup is in `docs/bridge.md`.
//!
//! The bridge's config is a TOML file (default `.hord/bridge.toml`):
//!
//! ```toml
//! remote = "https://github.com/owner/hord.git"  # the mirror; no credentials
//! token_file = "/etc/hord/github-token"         # outside the repository
//! poll_secs = 60                                # pull request polling
//! check_secs = 3600                             # divergence checks
//! work_dir = "/var/lib/hord/bridge"             # default .hord/bridge
//! follow = "origin"                             # a hord remote; default this repository
//!
//! [github]                                      # none: export and checks only
//! repository = "owner/hord"
//! api = "https://api.github.com"
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hord_api::proto::{self, BridgeCheckTrigger};
use hord_api::{RepoBackend, wire};
use hord_git::sync::{
    Bridge, BridgeOptions, Check, GitHub, GitHubOptions, PullRequests, SyncReport,
};
use hord_txn::LocalRepo;
use serde::Deserialize;

use crate::session::{self, Session, Target};
use crate::txn::block_on;
use crate::{output, repo};

/// What `hord git sync` does.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// Run until interrupted.
    Daemon,
    /// One pass.
    Once,
    /// Check `main`, change nothing.
    Check,
    /// Force-push the export.
    Repair,
}

/// The bridge's config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The mirror's git URL.
    remote: String,
    /// A file holding the token for the mirror and the GitHub API.
    token_file: Option<PathBuf>,
    /// Pull request polling interval.
    poll_secs: Option<u64>,
    /// Divergence check interval.
    check_secs: Option<u64>,
    /// The bridge's work directory.
    work_dir: Option<PathBuf>,
    /// The hord repository to follow: a remote name or address.
    follow: Option<String>,
    /// The pull request host.
    github: Option<GitHubConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitHubConfig {
    /// `owner/name`.
    repository: String,
    /// API root.
    api: Option<String>,
}

pub fn run(json: bool, target: &Target, mode: Mode, config: Option<PathBuf>) -> Result<()> {
    let root = repo::discover_root().ok();
    let config_path = match (config, &root) {
        (Some(path), _) => path,
        (None, Some(root)) => root.join(hord_store::HORD_DIR).join("bridge.toml"),
        (None, None) => bail!("no .hord directory here: pass --config"),
    };
    let text = fs::read_to_string(&config_path)
        .with_context(|| format!("read the bridge config {}", config_path.display()))?;
    let config: Config = toml::from_str(&text)
        .with_context(|| format!("parse the bridge config {}", config_path.display()))?;
    let token = match &config.token_file {
        Some(file) => Some(read_token(file, root.as_deref())?),
        None => None,
    };
    let work_dir = match (&config.work_dir, &root) {
        (Some(dir), _) => dir.clone(),
        (None, Some(root)) => root.join(hord_store::HORD_DIR).join("bridge"),
        (None, None) => bail!("set work_dir in {}", config_path.display()),
    };

    let (backend, voucher) = backend(target, config.follow.as_deref())?;
    let pulls: Option<Arc<dyn PullRequests>> = match &config.github {
        Some(github) => {
            let token = token
                .clone()
                .ok_or_else(|| anyhow!("[github] needs token_file"))?;
            let options = GitHubOptions {
                api: github
                    .api
                    .clone()
                    .unwrap_or_else(|| "https://api.github.com".into()),
                repository: github.repository.clone(),
                token,
                base: "main".into(),
            };
            Some(Arc::new(GitHub::new(options)?))
        }
        None => None,
    };
    let mut options = BridgeOptions::new(config.remote.clone(), work_dir);
    options.token = token;
    options.voucher = voucher;
    if let Some(secs) = config.poll_secs {
        options.poll = Duration::from_secs(secs.max(1));
    }
    if let Some(secs) = config.check_secs {
        options.check_every = Duration::from_secs(secs.max(1));
    }

    let mut bridge = block_on(Bridge::open(backend, pulls, options))?;
    match mode {
        Mode::Once => {
            let report = block_on(bridge.sync_once())?;
            print_report(json, &report)
        }
        Mode::Check => {
            let check = block_on(bridge.check(BridgeCheckTrigger::Check))?;
            print_check(json, &check)?;
            if check.diverged {
                // Divergence is the answer, not a failure to run: exit 1
                // after the report, like `hord policy check` on deny.
                process::exit(1);
            }
            Ok(())
        }
        Mode::Repair => {
            let check = block_on(bridge.repair())?;
            print_check(json, &check)
        }
        Mode::Daemon => {
            if !json {
                eprintln!("hord git sync: mirroring to {}", config.remote);
            }
            block_on(bridge.run(async {
                let _ = tokio::signal::ctrl_c().await;
            }))?;
            Ok(())
        }
    }
}

/// The repository to follow, and the key id that vouches for pull
/// requests (ADR 0037): the logged-in credential's, against a remote.
fn backend(
    target: &Target,
    follow: Option<&str>,
) -> Result<(Arc<dyn RepoBackend>, Option<String>)> {
    if follow.is_some() || target.remote.is_some() {
        let (name, url) = session::remote_url(target, follow)?;
        let (remote, credential) = session::connect(&name, &url)?;
        let voucher = match credential {
            Some(credential) => Some(credential.key()?.public().key_id()),
            None => None,
        };
        return Ok((Arc::new(remote), voucher));
    }
    let backend: Arc<dyn RepoBackend> = match Session::open(target)? {
        Session::Daemon { remote } => Arc::new(remote),
        Session::Remote {
            remote, credential, ..
        } => {
            let voucher = match credential {
                Some(credential) => Some(credential.key()?.public().key_id()),
                None => None,
            };
            return Ok((Arc::new(remote), voucher));
        }
        // No daemon: land here, so pull requests do not wait for one.
        Session::Direct { repo } => Arc::new(LocalRepo::new(repo)?),
    };
    Ok((backend, None))
}

/// The token in `file`, which must not be in the repository's tracked
/// tree (ADR 0036: the token is the host's, never the repository's).
fn read_token(file: &Path, root: Option<&Path>) -> Result<String> {
    let path = file
        .canonicalize()
        .with_context(|| format!("token file {}", file.display()))?;
    if let Some(root) = root.and_then(|r| r.canonicalize().ok())
        && path.starts_with(&root)
        && !path.starts_with(root.join(hord_store::HORD_DIR))
    {
        bail!(
            "token file {} is inside the repository; keep it outside (or under .hord/)",
            path.display()
        );
    }
    let token =
        fs::read_to_string(&path).with_context(|| format!("read token file {}", path.display()))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        bail!("token file {} is empty", path.display());
    }
    Ok(token)
}

fn check_message(check: &Check) -> proto::BridgeChecked {
    proto::BridgeChecked {
        remote: check.remote.clone(),
        diverged: check.diverged,
        expected: check.expected.clone(),
        actual: check.actual.clone(),
        trigger: check.trigger.into(),
        detail: check.detail.clone(),
        head: check.head.map(wire::id),
    }
}

fn print_check(json: bool, check: &Check) -> Result<()> {
    if json {
        return output::print_json(&check_message(check));
    }
    let verdict = if check.diverged { "DIVERGED" } else { "ok" };
    println!("{verdict}: {}", check.detail);
    Ok(())
}

fn print_report(json: bool, report: &SyncReport) -> Result<()> {
    if json {
        let result = proto::GitSyncResult {
            exported: report.exported.iter().copied().map(wire::id).collect(),
            pushed: report.pushed.clone(),
            proposed: report
                .proposed
                .iter()
                .map(|(pull, change)| proto::GitSyncProposal {
                    pull: *pull,
                    change: wire::id(*change),
                })
                .collect(),
            reported: report
                .reported
                .iter()
                .map(|(pull, outcome)| proto::GitSyncReport {
                    pull: *pull,
                    outcome: outcome.clone(),
                })
                .collect(),
            checks: report.checks.iter().map(check_message).collect(),
        };
        return output::print_json(&result);
    }
    println!("exported {} landed change(s)", report.exported.len());
    if let Some(pushed) = &report.pushed {
        println!("pushed {pushed} to main");
    }
    for (pull, change) in &report.proposed {
        println!("pull request #{pull}: submitted {change}");
    }
    for (pull, outcome) in &report.reported {
        println!("pull request #{pull}: reported {outcome}");
    }
    for check in &report.checks {
        let verdict = if check.diverged { "DIVERGED" } else { "ok" };
        println!("{verdict}: {}", check.detail);
    }
    Ok(())
}
