//! [`Cancel`]: stop running verification commands from another thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Asks running commands to stop. A command that sees it is killed with
/// its process group, as on a timeout, and its [`crate::RunOutput`] is
/// marked `cancelled`. Commands started after it is set are killed at
/// once. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A flag that is not set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the flag: every command running under it stops.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the flag is set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
