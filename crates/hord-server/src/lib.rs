//! `hord serve` (spec §10.5.1, ADR 0024): the hosting server.
//!
//! One process hosts one repository (`--repo`) or every repository under a
//! directory (`--root`, each at `/r/<name>/`). For each it runs a
//! [`hord_txn::LocalRepo`], whose lander is a tokio task (spec §6.7), and
//! serves it over gRPC and gRPC-Web on one port:
//!
//! - `hord.v1.RepoBackend`: spec §10.5.2 method for method, including the
//!   batched object service and the resumable event stream;
//! - `hord.v1.Schema/GetSchema` and `GET /schema.json`: the descriptor set
//!   and the JSON Schema generated from it;
//! - optional webhooks from `server.toml`: the JSON-mapped event POSTed to
//!   each URL, filtered by kind.
//!
//! A TCP listener binds loopback only unless [`ServeOptions::insecure_bind`]
//! is set (auth is M5). [`Server::serve_local`] listens on the repository's
//! local endpoint instead: a Unix socket, or a named pipe on Windows
//! (ADR 0021's per-repo daemon).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod activity;
mod changes;
mod config;
mod error;
mod hosts;
mod local;
mod route;
mod server;
mod service;
mod webhook;

pub use activity::Activity;
pub use changes::{LocalChanges, RECORDINGS_DIR, save_recording};
pub use config::{ServerConfig, WebhookConfig};
pub use error::{Error, Result};
pub use hosts::Hosts;
pub use server::{ServeOptions, Server, check_bind};
