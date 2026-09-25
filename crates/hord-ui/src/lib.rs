//! The hord web UI (spec §10.4): views 1–3 and flight-recorder playback.
//!
//! This crate holds the `askama` templates and static assets, and the
//! state they render:
//!
//! - [`strip`]: the landing strip, a pure fold of the event stream
//!   (spec §10.5.3), shared by the live view and playback;
//! - [`playback`]: a flight-recorder log replayed at any speed, with a
//!   scrubber;
//! - [`view`]: display-ready view models and the page templates (landing
//!   strip, semantic change, arbitration workbench, playback);
//! - [`assets`]: the stylesheet and the two small scripts, embedded.
//!
//! Per §10.4 the UI has no privileged data path: it depends only on
//! `hord-api`, and reads and acts through `hord.proto` alone.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod assets;
pub mod playback;
pub mod strip;
pub mod view;

use hord_api::ApiError;

/// Errors from the UI.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A recording could not be read.
    #[error("recording: {0}")]
    Recording(#[source] ApiError),
    /// A playback speed outside [`playback::SPEEDS`].
    #[error("playback speed {0} is outside 0.1–1000")]
    Speed(f64),
    /// A template failed to render.
    #[error("render: {0}")]
    Render(#[from] askama::Error),
}

/// Result with [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
