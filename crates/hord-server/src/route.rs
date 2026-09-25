//! Multi-repository routing (spec §10.5.1): `/r/<name>/…` selects a hosted
//! repository. A tower layer strips the prefix before gRPC routing and
//! records the name as a [`RepoName`] request extension, which the services
//! read. A name may contain `/`: the layer matches the longest hosted name
//! after `/r/`, so web UI paths of any depth work (ADR 0030). For a name it
//! does not host, gRPC paths are `/package.Service/Method`, so the name is
//! whatever lies between `/r/` and the last two segments.

use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};

/// The hosted repository a request names (`/r/<name>/…`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoName(pub String);

/// Split `/r/<name>/<rest>` into `(name, /rest)`: the rest is the last two
/// segments (a gRPC method), or `/schema.json`.
#[must_use]
pub fn split_repo_prefix(path: &str) -> Option<(&str, &str)> {
    let tail = path.strip_prefix("/r/")?;
    let cut = if tail.ends_with("/schema.json") {
        tail.len() - "/schema.json".len()
    } else {
        let last = tail.rfind('/')?;
        tail[..last].rfind('/')?
    };
    let (name, rest) = tail.split_at(cut);
    (!name.is_empty()).then_some((name, rest))
}

/// Split `/r/<name>/<rest>` where `<name>` is the longest of `names` that
/// the path continues with a `/` (or ends at); the rest is `/` when empty.
/// Otherwise [`split_repo_prefix`].
#[must_use]
pub fn split_hosted_prefix<'p>(path: &'p str, names: &[String]) -> Option<(&'p str, &'p str)> {
    let tail = path.strip_prefix("/r/")?;
    let hosted = names
        .iter()
        .filter(|n| {
            tail.strip_prefix(n.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .max_by_key(|n| n.len());
    match hosted {
        Some(name) => {
            let (name, rest) = tail.split_at(name.len());
            Some((name, if rest.is_empty() { "/" } else { rest }))
        }
        None => split_repo_prefix(path),
    }
}

/// Tower layer that applies [`split_hosted_prefix`] for the hosted names.
#[derive(Clone, Debug, Default)]
pub struct RepoPrefixLayer {
    names: Arc<[String]>,
}

impl RepoPrefixLayer {
    /// A layer for a server hosting `names`.
    #[must_use]
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }
}

impl<S> Layer<S> for RepoPrefixLayer {
    type Service = RepoPrefix<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RepoPrefix {
            inner,
            names: Arc::clone(&self.names),
        }
    }
}

/// Service made by [`RepoPrefixLayer`].
#[derive(Clone, Debug)]
pub struct RepoPrefix<S> {
    inner: S,
    names: Arc<[String]>,
}

impl<S, B> Service<http::Request<B>> for RepoPrefix<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let split = split_hosted_prefix(request.uri().path(), &self.names)
            .map(|(name, rest)| (name.to_owned(), rest.to_owned()));
        if let Some((name, rest)) = split {
            let mut parts = request.uri().clone().into_parts();
            let path_and_query = match request.uri().query() {
                Some(query) => format!("{rest}?{query}"),
                None => rest,
            };
            if let Ok(pq) = path_and_query.parse() {
                parts.path_and_query = Some(pq);
                if let Ok(uri) = http::Uri::from_parts(parts) {
                    *request.uri_mut() = uri;
                    request.extensions_mut().insert(RepoName(name));
                }
            }
        }
        self.inner.call(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_names_with_slashes_and_the_schema() {
        assert_eq!(
            split_repo_prefix("/r/app/hord.v1.RepoBackend/Head"),
            Some(("app", "/hord.v1.RepoBackend/Head"))
        );
        assert_eq!(
            split_repo_prefix("/r/team/app/hord.v1.RepoBackend/Head"),
            Some(("team/app", "/hord.v1.RepoBackend/Head"))
        );
        assert_eq!(
            split_repo_prefix("/r/team/app/schema.json"),
            Some(("team/app", "/schema.json"))
        );
        assert_eq!(split_repo_prefix("/hord.v1.RepoBackend/Head"), None);
        assert_eq!(split_repo_prefix("/r/hord.v1.RepoBackend/Head"), None);
    }

    #[test]
    fn hosted_names_split_web_paths_of_any_depth() {
        let names = ["team/app".to_owned(), "team".to_owned()];
        assert_eq!(
            split_hosted_prefix("/r/team/app/recordings/abc/frames", &names),
            Some(("team/app", "/recordings/abc/frames"))
        );
        assert_eq!(
            split_hosted_prefix("/r/team/app", &names),
            Some(("team/app", "/"))
        );
        assert_eq!(split_hosted_prefix("/r/team/", &names), Some(("team", "/")));
        assert_eq!(
            split_hosted_prefix("/r/teamx/hord.v1.RepoBackend/Head", &names),
            Some(("teamx", "/hord.v1.RepoBackend/Head"))
        );
    }
}
