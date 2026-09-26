//! [`PullRequests`] over the GitHub REST API (ADR 0036).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::crypto::ring;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::timeout;

use super::SyncError;
use super::pulls::{PullRequest, PullRequests, StatusState};

/// How long one API call may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Pull requests per page (GitHub's maximum).
const PER_PAGE: usize = 100;
/// The commit status context the bridge sets.
const CONTEXT: &str = "hord/lander";

/// Where the GitHub side is, from the bridge's config file.
#[derive(Clone)]
pub struct GitHubOptions {
    /// API root: `https://api.github.com`, or a GitHub Enterprise one.
    pub api: String,
    /// `owner/name`.
    pub repository: String,
    /// A token with contents and pull request write access, from the
    /// bridge's config (never the repository).
    pub token: String,
    /// The branch pull requests target: `main`.
    pub base: String,
}

impl fmt::Debug for GitHubOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubOptions")
            .field("api", &self.api)
            .field("repository", &self.repository)
            .field("token", &"…")
            .field("base", &self.base)
            .finish()
    }
}

/// The live pull request host: GitHub's REST API over `hyper` and
/// `rustls` (ring, native roots).
#[derive(Clone)]
pub struct GitHub {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    options: Arc<GitHubOptions>,
}

impl fmt::Debug for GitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHub")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct ApiPull {
    number: u64,
    title: String,
    body: Option<String>,
    html_url: String,
    head: ApiHead,
}

#[derive(Deserialize)]
struct ApiHead {
    sha: String,
}

impl GitHub {
    /// A client for `options`. Fails when the system's root certificates
    /// cannot be loaded.
    pub fn new(options: GitHubOptions) -> Result<Self, SyncError> {
        let https = HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(ring::default_provider())
            .map_err(|err| SyncError::Pulls(format!("load root certificates: {err}")))?
            // `http://` for a local API in tests and proxies.
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            options: Arc::new(options),
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/repos/{}/{path}",
            self.options.api.trim_end_matches('/'),
            self.options.repository
        )
    }

    /// Make a call; its status and body, whatever the status.
    async fn send(
        &self,
        method: &Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Bytes), SyncError> {
        let payload = match &body {
            Some(value) => Bytes::from(value.to_string()),
            None => Bytes::new(),
        };
        let request = Request::builder()
            .method(method)
            .uri(url)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.options.token),
            )
            .header(header::USER_AGENT, "hord-git-bridge")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(payload))
            .map_err(|err| SyncError::Pulls(format!("{method} {url}: {err}")))?;
        let response = timeout(TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| SyncError::Pulls(format!("{method} {url}: timed out")))?
            .map_err(|err| SyncError::Pulls(format!("{method} {url}: {err}")))?;
        let status = response.status();
        let bytes = timeout(TIMEOUT, response.into_body().collect())
            .await
            .map_err(|_| SyncError::Pulls(format!("{method} {url}: timed out")))?
            .map_err(|err| SyncError::Pulls(format!("{method} {url}: {err}")))?
            .to_bytes();
        Ok((status, bytes))
    }

    /// Make a call that must succeed; its body.
    async fn call(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<Bytes, SyncError> {
        let (status, bytes) = self.send(&method, url, body).await?;
        if !status.is_success() {
            return Err(failed(&method, url, status, &bytes));
        }
        Ok(bytes)
    }

    async fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T, SyncError> {
        let bytes = self.call(Method::GET, url, None).await?;
        serde_json::from_slice(&bytes).map_err(|err| SyncError::Pulls(format!("GET {url}: {err}")))
    }
}

#[async_trait]
impl PullRequests for GitHub {
    async fn open_pulls(&self) -> Result<Vec<PullRequest>, SyncError> {
        let mut out = Vec::new();
        for page in 1.. {
            let url = self.url(&format!(
                "pulls?state=open&base={}&per_page={PER_PAGE}&page={page}",
                self.options.base
            ));
            let pulls: Vec<ApiPull> = self.get(&url).await?;
            let last = pulls.len() < PER_PAGE;
            out.extend(pulls.into_iter().map(|p| PullRequest {
                git_ref: format!("refs/pull/{}/head", p.number),
                number: p.number,
                title: p.title,
                body: p.body.unwrap_or_default(),
                head_sha: p.head.sha,
                url: p.html_url,
            }));
            if last {
                break;
            }
        }
        Ok(out)
    }

    async fn set_status(
        &self,
        _pull: u64,
        sha: &str,
        state: StatusState,
        description: &str,
    ) -> Result<(), SyncError> {
        let body = json!({
            "state": state.as_str(),
            "description": description,
            "context": CONTEXT,
        });
        self.call(
            Method::POST,
            &self.url(&format!("statuses/{sha}")),
            Some(body),
        )
        .await
        .map(drop)
    }

    async fn comment(&self, pull: u64, body: &str) -> Result<(), SyncError> {
        let body = json!({ "body": body });
        let url = self.url(&format!("issues/{pull}/comments"));
        self.call(Method::POST, &url, Some(body)).await.map(drop)
    }

    async fn close(&self, pull: u64) -> Result<(), SyncError> {
        let body = json!({ "state": "closed" });
        let url = self.url(&format!("pulls/{pull}"));
        let (status, bytes) = self.send(&Method::PATCH, &url, Some(body)).await?;
        // 422: already closed or merged elsewhere.
        if status.is_success() || status == StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(());
        }
        Err(failed(&Method::PATCH, &url, status, &bytes))
    }
}

fn failed(method: &Method, url: &str, status: StatusCode, body: &[u8]) -> SyncError {
    SyncError::Pulls(format!(
        "{method} {url}: {status}: {}",
        String::from_utf8_lossy(body).trim()
    ))
}
