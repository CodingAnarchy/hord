//! The pull request host, behind a trait (ADR 0036).

use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use super::SyncError;

/// An open pull request against `main`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    /// Its number on the host.
    pub number: u64,
    /// Title: the proposal's intent summary.
    pub title: String,
    /// Description: the proposal's intent body.
    pub body: String,
    /// The commit it proposes, hex.
    pub head_sha: String,
    /// The ref on the mirror that names the head, fetched by the bridge
    /// (`refs/pull/<n>/head` on GitHub).
    pub git_ref: String,
    /// Where people read it; recorded as an intent ref. Empty for none.
    pub url: String,
}

/// State of a commit status (GitHub's four).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusState {
    /// Submitted; the lander has not decided.
    Pending,
    /// Landed.
    Success,
    /// Parked or rejected by the lander.
    Failure,
    /// The bridge could not submit it.
    Error,
}

impl StatusState {
    /// The host's name for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

/// Where pull requests come from and where outcomes go (ADR 0036).
#[async_trait]
pub trait PullRequests: Send + Sync {
    /// Open pull requests whose base is `main`.
    async fn open_pulls(&self) -> Result<Vec<PullRequest>, SyncError>;

    /// Set the lander's commit status on `sha`, the head of `pull`.
    async fn set_status(
        &self,
        pull: u64,
        sha: &str,
        state: StatusState,
        description: &str,
    ) -> Result<(), SyncError>;

    /// Add a comment to `pull`.
    async fn comment(&self, pull: u64, body: &str) -> Result<(), SyncError>;

    /// Close `pull`: its change landed on `main`.
    async fn close(&self, pull: u64) -> Result<(), SyncError>;
}

/// One thing the bridge told a [`ScriptedPulls`] host, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reported {
    /// [`PullRequests::set_status`].
    Status {
        /// The pull request.
        pull: u64,
        /// The commit.
        sha: String,
        /// The state.
        state: StatusState,
        /// The description.
        description: String,
    },
    /// [`PullRequests::comment`].
    Comment {
        /// The pull request.
        pull: u64,
        /// The text.
        body: String,
    },
    /// [`PullRequests::close`].
    Closed {
        /// The pull request.
        pull: u64,
    },
}

/// An in-memory pull request host for tests: the test opens and updates
/// pull requests, and reads back what the bridge reported. A closed pull
/// request is no longer listed as open.
#[derive(Debug, Default)]
pub struct ScriptedPulls {
    open: Mutex<Vec<PullRequest>>,
    reported: Mutex<Vec<Reported>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ScriptedPulls {
    /// No pull requests.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open `pull`, or replace the open one with its number (a new push).
    pub fn open(&self, pull: PullRequest) {
        let mut open = lock(&self.open);
        open.retain(|p| p.number != pull.number);
        open.push(pull);
    }

    /// Everything reported so far, in order.
    #[must_use]
    pub fn reported(&self) -> Vec<Reported> {
        lock(&self.reported).clone()
    }

    /// What was reported about `pull`, in order.
    #[must_use]
    pub fn reported_on(&self, pull: u64) -> Vec<Reported> {
        self.reported()
            .into_iter()
            .filter(|r| match r {
                Reported::Status { pull: p, .. }
                | Reported::Comment { pull: p, .. }
                | Reported::Closed { pull: p } => *p == pull,
            })
            .collect()
    }

    /// Whether `pull` is open.
    #[must_use]
    pub fn is_open(&self, pull: u64) -> bool {
        lock(&self.open).iter().any(|p| p.number == pull)
    }
}

#[async_trait]
impl PullRequests for ScriptedPulls {
    async fn open_pulls(&self) -> Result<Vec<PullRequest>, SyncError> {
        Ok(lock(&self.open).clone())
    }

    async fn set_status(
        &self,
        pull: u64,
        sha: &str,
        state: StatusState,
        description: &str,
    ) -> Result<(), SyncError> {
        lock(&self.reported).push(Reported::Status {
            pull,
            sha: sha.to_owned(),
            state,
            description: description.to_owned(),
        });
        Ok(())
    }

    async fn comment(&self, pull: u64, body: &str) -> Result<(), SyncError> {
        lock(&self.reported).push(Reported::Comment {
            pull,
            body: body.to_owned(),
        });
        Ok(())
    }

    async fn close(&self, pull: u64) -> Result<(), SyncError> {
        lock(&self.open).retain(|p| p.number != pull);
        lock(&self.reported).push(Reported::Closed { pull });
        Ok(())
    }
}
