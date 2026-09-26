//! The hord web UI (spec §10.4): views 1–6 and flight-recorder playback.
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
//! - [`present`]: API messages mapped to view models;
//! - [`browse`]: views 4–6 (node lineage, provenance trace, and the
//!   repository browser with its graph view), models and mapping;
//! - [`assets`]: the stylesheet and the two small scripts, embedded;
//! - [`router`]: the routes `hord serve` mounts (ADR 0030);
//! - [`audit`]: the M5 check that every call is an RPC of `hord.proto`.
//!
//! Per §10.4 and ADR 0030 the UI has no privileged data path: it renders
//! on the server as a client of `hord.proto`, through the [`UiRepo`]
//! backends it is handed, and depends on no other hord crate than
//! `hord-api`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod app;
pub mod assets;
pub mod audit;
pub mod browse;
pub mod playback;
pub mod present;
pub mod strip;
pub mod view;

pub use app::{
    ArbitrationSigner, MAX_ROWS, ReviewBackend, SingleRepo, UI_TOKEN_COOKIE, UiHosts, UiRepo,
    router,
};

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
