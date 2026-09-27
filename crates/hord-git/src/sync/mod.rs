//! The git bridge daemon (spec §9 Sync, ADR 0036, ADR 0037).
//!
//! A [`Bridge`] keeps a git remote's `main` equal to the export of a hord
//! log, and takes the remote's pull requests as proposals:
//!
//! - **Export.** Each landed change is exported ([`crate::export_change`])
//!   in log order and pushed to `main`, fast-forward only. What was pushed
//!   is kept in the bridge's state, so a restart neither skips nor repeats a
//!   change.
//! - **Pull requests.** An open pull request's head becomes one Tier 0
//!   proposal ([`crate::propose_git_commit`]) against the landed change it
//!   was branched from, submitted to the lander. The outcome is reported
//!   back as a commit status and a comment, and the pull request is closed
//!   once its change is on `main`.
//! - **Divergence.** `main` has diverged when it names a commit the export
//!   does not reach. The bridge checks after every push it makes, every
//!   hour, and on demand, and records every check as a `BridgeChecked`
//!   event. It never repairs on its own: [`Bridge::repair`] force-pushes the
//!   export when an operator asks.
//!
//! The pull request host is behind [`PullRequests`]: [`GitHub`] is the live
//! REST implementation, [`ScriptedPulls`] an in-memory one for tests. The
//! mirror itself is any git URL, reached with the `git` command.

mod bridge;
mod cache;
mod error;
mod github;
mod mirror;
mod pulls;
mod state;

pub use bridge::{Bridge, BridgeOptions, Check, SyncReport};
pub use error::SyncError;
pub use github::{GitHub, GitHubOptions};
pub use pulls::{PullRequest, PullRequests, Reported, ScriptedPulls, StatusState};
