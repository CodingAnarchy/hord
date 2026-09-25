//! Listening on a repository's local endpoint (ADR 0021): a Unix domain
//! socket, or a named pipe on Windows ([`hord_api::local::endpoint`]).

use std::io;

use tokio_stream::wrappers::ReceiverStream;

/// Accepted local connections.
pub(crate) type Incoming = ReceiverStream<io::Result<Conn>>;

/// Removes the socket file when the listener is done.
#[derive(Debug)]
pub(crate) struct Cleanup(Option<std::path::PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::{Conn, listen};

#[cfg(windows)]
pub(crate) use windows::{Conn, listen};

#[cfg(unix)]
mod unix {
    use std::io;
    use std::path::Path;

    use tokio::net::{UnixListener, UnixStream};

    use super::{Cleanup, Incoming};

    /// A connection on the socket.
    pub(crate) type Conn = UnixStream;

    /// Listen on the socket at `endpoint`. A stale socket file (nothing
    /// accepts on it) is replaced; a live one is an `AddrInUse` error.
    pub(crate) async fn listen(endpoint: &str) -> io::Result<(Incoming, Cleanup)> {
        let path = Path::new(endpoint);
        if path.exists() {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("a server already listens on {endpoint}"),
                ));
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await.map(|(stream, _)| stream);
                if tx.send(accepted).await.is_err() {
                    return;
                }
            }
        });
        Ok((
            tokio_stream::wrappers::ReceiverStream::new(rx),
            Cleanup(Some(path.to_path_buf())),
        ))
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::{Cleanup, Incoming};

    /// A connected named-pipe instance.
    #[derive(Debug)]
    pub(crate) struct Conn(NamedPipeServer);

    impl tonic::transport::server::Connected for Conn {
        type ConnectInfo = ();

        fn connect_info(&self) -> Self::ConnectInfo {}
    }

    impl AsyncRead for Conn {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Conn {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    /// Serve the pipe `endpoint`: one instance per connection, the next
    /// created before the connected one is handed off. The first instance
    /// is created exclusively, so a second server on the same pipe fails.
    pub(crate) async fn listen(endpoint: &str) -> io::Result<(Incoming, Cleanup)> {
        let name = endpoint.to_owned();
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            loop {
                let next = match server.connect().await {
                    Ok(()) => ServerOptions::new().create(&name).map(|next| {
                        let connected = std::mem::replace(&mut server, next);
                        Conn(connected)
                    }),
                    Err(err) => Err(err),
                };
                if tx.send(next).await.is_err() {
                    return;
                }
            }
        });
        Ok((
            tokio_stream::wrappers::ReceiverStream::new(rx),
            Cleanup(None),
        ))
    }
}

/// Whether an error from [`listen`] means another server holds the
/// endpoint.
#[must_use]
pub(crate) fn in_use(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}
