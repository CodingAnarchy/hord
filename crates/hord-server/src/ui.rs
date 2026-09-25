//! The web UI (ADR 0030) mounted over [`Hosts`]: each request goes to the
//! repository its `/r/<name>/` prefix names, else the only one, with the
//! same backends the gRPC services serve.

use std::sync::Arc;

use axum::http::Extensions;
use hord_api::ApiResult;
use hord_ui::{UiHosts, UiRepo};

use crate::hosts::Hosts;
use crate::route::RepoName;

/// [`UiHosts`] over the server's [`Hosts`].
#[derive(Debug)]
pub(crate) struct HostedUi {
    hosts: Arc<Hosts>,
}

impl HostedUi {
    pub(crate) fn new(hosts: Arc<Hosts>) -> Self {
        Self { hosts }
    }
}

impl UiHosts for HostedUi {
    fn resolve(&self, extensions: &Extensions) -> ApiResult<UiRepo> {
        let prefix = extensions.get::<RepoName>();
        Ok(UiRepo {
            backend: self.hosts.resolve(prefix)?,
            changes: self.hosts.resolve_changes(prefix)?,
            // Seam for agent `auth`: the review RPC and signing.
            review: None,
            name: self.hosts.addressed(prefix)?.to_owned(),
            base: prefix.map_or_else(String::new, |RepoName(name)| format!("/r/{name}")),
        })
    }
}
