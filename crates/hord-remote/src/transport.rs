//! The client transport: a tonic channel over TCP or a local endpoint,
//! with the `/r/<name>` prefix of a multi-repository server prepended to
//! every request path.

use std::sync::Arc;
use std::task::{Context, Poll};

use tonic::body::Body;
use tonic::transport::Channel;
use tower::Service;

/// A [`Channel`] that prefixes request paths with `/r/<name>`, if any.
#[derive(Clone, Debug)]
pub(crate) struct Transport {
    channel: Channel,
    prefix: Option<Arc<str>>,
}

impl Transport {
    pub(crate) fn new(channel: Channel, prefix: Option<String>) -> Self {
        Self {
            channel,
            prefix: prefix.map(Arc::from),
        }
    }
}

impl Service<http::Request<Body>> for Transport {
    type Response = http::Response<Body>;
    type Error = tonic::transport::Error;
    type Future = <Channel as Service<http::Request<Body>>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::poll_ready(&mut self.channel, cx)
    }

    fn call(&mut self, mut request: http::Request<Body>) -> Self::Future {
        if let Some(prefix) = &self.prefix {
            let mut parts = request.uri().clone().into_parts();
            let path = parts
                .path_and_query
                .as_ref()
                .map_or("/", http::uri::PathAndQuery::as_str);
            if let Ok(pq) = format!("{prefix}{path}").parse() {
                parts.path_and_query = Some(pq);
                if let Ok(uri) = http::Uri::from_parts(parts) {
                    *request.uri_mut() = uri;
                }
            }
        }
        self.channel.call(request)
    }
}

/// Split `http://host:port/r/<name>` into the origin and `/r/<name>`.
pub(crate) fn split_url(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    let origin = format!("http://{authority}");
    match path {
        "" => Some((origin, None)),
        p if p.starts_with("/r/") && p.len() > 3 => Some((origin, Some(p.to_owned()))),
        _ => None,
    }
}

/// Open a connection to the repository's local endpoint (a Unix socket, or
/// a named pipe on Windows).
#[cfg(unix)]
pub(crate) async fn connect_local_io(
    endpoint: String,
) -> std::io::Result<hyper_util::rt::TokioIo<tokio::net::UnixStream>> {
    Ok(hyper_util::rt::TokioIo::new(
        tokio::net::UnixStream::connect(endpoint).await?,
    ))
}

/// Open a connection to the repository's local endpoint (a Unix socket, or
/// a named pipe on Windows). A busy pipe is retried briefly.
#[cfg(windows)]
pub(crate) async fn connect_local_io(
    endpoint: String,
) -> std::io::Result<hyper_util::rt::TokioIo<tokio::net::windows::named_pipe::NamedPipeClient>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    /// `ERROR_PIPE_BUSY`: every instance is taken; the server makes more.
    const PIPE_BUSY: i32 = 231;
    for _ in 0..200 {
        match ClientOptions::new().open(&endpoint) {
            Ok(client) => return Ok(hyper_util::rt::TokioIo::new(client)),
            Err(err) if err.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{endpoint}: every pipe instance stayed busy"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_origin_and_repo_prefix() {
        assert_eq!(
            split_url("http://127.0.0.1:7878"),
            Some(("http://127.0.0.1:7878".into(), None))
        );
        assert_eq!(
            split_url("http://h:1/r/team/app/"),
            Some(("http://h:1".into(), Some("/r/team/app".into())))
        );
        assert_eq!(split_url("https://h"), None);
        assert_eq!(split_url("http://h/other"), None);
        assert_eq!(split_url("http:///r/x"), None);
    }
}
