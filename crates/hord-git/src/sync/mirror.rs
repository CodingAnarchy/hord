//! The git remote the bridge mirrors to, reached with the `git` command.
//!
//! gix does not push, so the network side (`ls-remote`, `fetch`, `push`)
//! runs `git` against the bridge's own bare export repository. A token is
//! passed as an HTTP header through `GIT_CONFIG_*` environment variables,
//! never on the command line or in the URL.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Output;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::Uri;
use tokio::process::Command;

use super::SyncError;
use crate::Error;
use crate::import::open_repo;

/// The branch the bridge owns (ADR 0036).
pub(crate) const MAIN: &str = "refs/heads/main";

/// What a push did.
#[derive(Debug)]
pub(crate) enum Pushed {
    /// `main` now names the commit.
    Updated,
    /// The remote refused a fast-forward: `main` has a commit the export
    /// does not.
    Rejected(String),
}

/// A git remote and the local bare repository the bridge exports into.
#[derive(Clone)]
pub(crate) struct Mirror {
    url: String,
    token: Option<String>,
    git_dir: PathBuf,
}

impl fmt::Debug for Mirror {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mirror")
            .field("url", &redact(&self.url))
            .field("token", &self.token.as_ref().map(|_| "…"))
            .field("git_dir", &self.git_dir)
            .finish()
    }
}

impl Mirror {
    pub(crate) fn new(url: String, token: Option<String>, git_dir: PathBuf) -> Self {
        Self {
            url,
            token,
            git_dir,
        }
    }

    /// The remote's URL without any credentials in it.
    pub(crate) fn display_url(&self) -> String {
        redact(&self.url)
    }

    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The commit the remote's `main` names; `None` when it has none.
    pub(crate) async fn main(&self) -> Result<Option<gix::ObjectId>, SyncError> {
        let out = self
            .run("ls-remote", &["ls-remote", "--exit-code", &self.url, MAIN])
            .await?;
        // `--exit-code`: 2 when no ref matched.
        if out.status.code() == Some(2) {
            return Ok(None);
        }
        let stdout = checked("ls-remote", out)?;
        // One line, `<sha>\t<ref>`.
        let sha = stdout.split_whitespace().next().unwrap_or_default();
        gix::ObjectId::from_hex(sha.as_bytes())
            .map(Some)
            .map_err(|err| SyncError::Command {
                command: "ls-remote".into(),
                detail: format!("unexpected output {stdout:?}: {err}"),
            })
    }

    /// Fetch the remote's `src` into the local ref `dst` (forced) and
    /// return the commit it names.
    pub(crate) async fn fetch(&self, src: &str, dst: &str) -> Result<gix::ObjectId, SyncError> {
        let refspec = format!("+{src}:{dst}");
        let out = self
            .run(
                "fetch",
                &["fetch", "--no-tags", "--quiet", &self.url, &refspec],
            )
            .await?;
        checked("fetch", out)?;
        let repo = open_repo(&self.git_dir)?;
        let id = repo.find_reference(dst).map_err(Error::git)?.id().detach();
        Ok(id)
    }

    /// Push `commit` to `main`: fast-forward only, or `force`d. A push the
    /// remote refuses for another reason (a hook, branch protection) is an
    /// error.
    pub(crate) async fn push(
        &self,
        commit: gix::ObjectId,
        force: bool,
    ) -> Result<Pushed, SyncError> {
        let refspec = format!("{}{commit}:{MAIN}", if force { "+" } else { "" });
        let out = self
            .run("push", &["push", "--porcelain", &self.url, &refspec])
            .await?;
        if out.status.success() {
            return Ok(Pushed::Updated);
        }
        // `--porcelain` flags a refused ref with `!`: `[rejected]` when
        // `main` has commits the push lacks, `[remote rejected]` or
        // `[remote failure]` when the remote refused it for its own reasons.
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = stdout
            .lines()
            .find(|l| l.starts_with('!') && l.contains("[rejected]"))
        {
            return Ok(Pushed::Rejected(line.to_owned()));
        }
        let refused = stdout.lines().find(|l| l.starts_with('!'));
        Err(SyncError::Command {
            command: "push".into(),
            detail: [
                refused.unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim(),
            ]
            .join("\n")
            .trim()
            .to_owned(),
        })
    }

    async fn run(&self, what: &str, args: &[&str]) -> Result<Output, SyncError> {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .kill_on_drop(true);
        if let Some(token) = &self.token {
            // GitHub's git endpoints take the token as basic auth.
            let basic = STANDARD.encode(format!("x-access-token:{token}"));
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
        }
        command.output().await.map_err(|err| SyncError::Command {
            command: what.to_owned(),
            detail: format!("cannot run git: {err}"),
        })
    }
}

/// Standard output of a successful command, else its standard error.
fn checked(what: &str, out: Output) -> Result<String, SyncError> {
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(SyncError::Command {
        command: what.to_owned(),
        detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    })
}

/// Whether a token may be sent to `url`: always over anything but plain
/// `http://`; over `http://` only with `insecure`, and only to a loopback
/// host (a test server, a local proxy).
pub(crate) fn check_transport(url: &str, insecure: bool) -> Result<(), SyncError> {
    if !url.starts_with("http://") {
        return Ok(());
    }
    let loopback = url.parse::<Uri>().ok().is_some_and(|uri| {
        uri.host().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    });
    if insecure && loopback {
        Ok(())
    } else {
        Err(SyncError::PlainHttp(redact(url)))
    }
}

/// `url` without a `user:password@` part.
pub(crate) fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            match authority.rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://{host}{path}"),
                None => url.to_owned(),
            }
        }
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{check_transport, redact};

    #[test]
    fn a_token_goes_over_plain_http_only_to_loopback_when_insecure() {
        for url in [
            "https://github.com/o/r.git",
            "/srv/mirror.git",
            "file:///m.git",
        ] {
            assert!(check_transport(url, false).is_ok(), "{url}");
        }
        for url in [
            "http://127.0.0.1:9000/o/r.git",
            "http://localhost/api",
            "http://[::1]:80/",
        ] {
            assert!(check_transport(url, false).is_err(), "{url}");
            assert!(check_transport(url, true).is_ok(), "{url}");
        }
        for url in [
            "http://github.com/o/r.git",
            "http://10.0.0.1/",
            "http://u:p@evil/",
        ] {
            assert!(check_transport(url, true).is_err(), "{url}");
        }
    }

    #[test]
    fn credentials_are_redacted() {
        assert_eq!(
            redact("https://x:secret@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(redact("/srv/mirror.git"), "/srv/mirror.git");
    }
}
