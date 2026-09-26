//! Webhooks (spec §10.5.3): each matching event, in the canonical JSON
//! mapping of `EventEnvelope`, is POSTed to the configured URL.
//!
//! Delivery is best effort: one attempt with a timeout, failures reported
//! on stderr. Receivers that must not miss an event resume the event stream
//! from a cursor instead.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hord_api::proto::event::Kind;
use hord_api::{RepoBackend, proto};
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio_stream::StreamExt;

use crate::config::WebhookConfig;

/// Names of the event kinds, as `server.toml` filters them: the `Event`
/// oneof fields of `hord.proto`.
pub const EVENT_KINDS: [&str; 11] = [
    "submitted",
    "conflict_check",
    "verifying",
    "evidence_attached",
    "replaying",
    "parked",
    "arbitrated",
    "landed",
    "rejected",
    "head_moved",
    "bridge_checked",
];

/// How long one delivery may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The [`EVENT_KINDS`] name of `kind`.
#[must_use]
pub fn event_kind_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Submitted(_) => "submitted",
        Kind::ConflictCheck(_) => "conflict_check",
        Kind::Verifying(_) => "verifying",
        Kind::EvidenceAttached(_) => "evidence_attached",
        Kind::Replaying(_) => "replaying",
        Kind::Parked(_) => "parked",
        Kind::Arbitrated(_) => "arbitrated",
        Kind::Landed(_) => "landed",
        Kind::Rejected(_) => "rejected",
        Kind::HeadMoved(_) => "head_moved",
        Kind::BridgeChecked(_) => "bridge_checked",
    }
}

fn wants(hook: &WebhookConfig, repo: &str, kind: &str) -> bool {
    (hook.repos.is_empty() || hook.repos.iter().any(|r| r == repo))
        && (hook.kinds.is_empty() || hook.kinds.iter().any(|k| k == kind))
}

/// Follow `backend`'s live events and deliver those `hooks` want, until
/// the stream ends.
pub(crate) async fn deliver(
    repo: String,
    backend: Arc<dyn RepoBackend>,
    hooks: Vec<WebhookConfig>,
) {
    let hooks: Vec<_> = hooks
        .into_iter()
        .filter(|h| h.repos.is_empty() || h.repos.contains(&repo))
        .collect();
    if hooks.is_empty() {
        return;
    }
    let mut events = match backend.events(proto::EventsRequest { from: None }).await {
        Ok(events) => events,
        Err(err) => {
            eprintln!("hord serve: webhooks for {repo}: {err}");
            return;
        }
    };
    let client: Client<HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    while let Some(item) = events.next().await {
        let Ok(envelope) = item else { continue };
        let Some(kind) = envelope.kind() else {
            continue;
        };
        let kind = event_kind_name(kind);
        let Ok(body) = serde_json::to_vec(&envelope) else {
            continue;
        };
        let body = Bytes::from(body);
        for hook in hooks.iter().filter(|h| wants(h, &repo, kind)) {
            let request = http::Request::post(&hook.url)
                .header(http::header::CONTENT_TYPE, "application/json")
                .header("x-hord-repo", &repo)
                .header("x-hord-event", kind)
                .body(Full::new(body.clone()));
            let Ok(request) = request else { continue };
            match tokio::time::timeout(TIMEOUT, client.request(request)).await {
                Ok(Ok(response)) if response.status().is_success() => {}
                Ok(Ok(response)) => {
                    eprintln!(
                        "hord serve: webhook {}: HTTP {}",
                        hook.url,
                        response.status()
                    );
                }
                Ok(Err(err)) => eprintln!("hord serve: webhook {}: {err}", hook.url),
                Err(_) => eprintln!("hord serve: webhook {}: timed out", hook.url),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_by_repo_and_kind() {
        let hook = WebhookConfig {
            url: "http://h".into(),
            kinds: vec!["landed".into()],
            repos: vec![],
        };
        assert!(wants(&hook, "a", "landed"));
        assert!(!wants(&hook, "a", "parked"));
        let scoped = WebhookConfig {
            repos: vec!["b".into()],
            kinds: vec![],
            ..hook
        };
        assert!(!wants(&scoped, "a", "landed"));
        assert!(wants(&scoped, "b", "parked"));
    }
}
