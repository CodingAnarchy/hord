//! What the bridge has done, kept in `<work>/state.json` so a restart
//! resumes without skipping or repeating anything.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

use super::SyncError;

/// The bridge's state.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct State {
    /// The last event handled (spec §10.5.3 `EventCursor`).
    #[serde(default)]
    pub(crate) cursor: Option<u64>,
    /// The last landed change pushed to `main`, as a hex `ChangeId`.
    #[serde(default)]
    pub(crate) pushed: Option<String>,
    /// Pull requests with a proposal, by number.
    #[serde(default)]
    pub(crate) pulls: BTreeMap<u64, PullState>,
}

/// One pull request's proposal.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct PullState {
    /// The head it was built from.
    pub(crate) head: String,
    /// The submitted `ChangeId`; unset when it could not be submitted.
    #[serde(default)]
    pub(crate) change: Option<String>,
    /// The last outcome reported on the pull request.
    #[serde(default)]
    pub(crate) reported: Option<String>,
}

impl State {
    /// Load `path`; a missing file is a fresh state.
    pub(crate) async fn load(path: &Path) -> Result<Self, SyncError> {
        match fs::read(path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| SyncError::State {
                path: path.to_owned(),
                reason: err.to_string(),
            }),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SyncError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Write to `path` atomically (a temporary file, then a rename).
    pub(crate) async fn save(&self, path: &Path) -> Result<(), SyncError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|err| SyncError::State {
            path: path.to_owned(),
            reason: err.to_string(),
        })?;
        let tmp: PathBuf = path.with_extension("json.tmp");
        let io = |source| SyncError::Io {
            path: path.to_owned(),
            source,
        };
        fs::write(&tmp, bytes).await.map_err(io)?;
        fs::rename(&tmp, path).await.map_err(io)
    }
}
