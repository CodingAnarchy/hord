//! Shared helpers for `hord-remote` integration tests.

#![allow(dead_code)]

use std::env;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use hord_server::{ServeOptions, Server};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// A test's temp dir, removed on drop.
pub struct Dir(pub PathBuf);

impl Drop for Dir {
    /// A drop cannot return an error, and panicking there could abort a
    /// failing test and lose its failure, so a failed removal is reported
    /// on stderr; [`temp`] fails loudly if the leftover is still there next
    /// time.
    fn drop(&mut self) {
        if let Err(err) = remove_tree(&self.0) {
            eprintln!("remove temp dir {}: {err}", self.0.display());
        }
    }
}

/// A fresh temp dir named for `tag`, the pid and a per-process counter.
pub fn temp(tag: &str) -> io::Result<Dir> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = env::temp_dir().join(format!(
        "hord-remote-{tag}-{}-{}",
        process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    // Paths repeat when the OS reuses a pid: clear a stale leftover, or
    // fail here rather than collide later.
    remove_tree(&path)?;
    fs::create_dir_all(&path)?;
    Ok(Dir(path))
}

/// Remove `path` and everything under it, if it exists. Directory
/// workspaces keep a read-only pristine checkout (ADR 0016), which
/// `fs::remove_dir_all` alone cannot empty, so write access is restored to
/// every directory first.
fn remove_tree(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_err_and(|err| err.kind() == ErrorKind::NotFound) {
        return Ok(());
    }
    make_writable(path)?;
    fs::remove_dir_all(path)
}

fn make_writable(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir)?.permissions().mode();
        fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

/// A test server listening on a loopback port until [`Running::stop`].
pub struct Running {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Running {
    /// Bind `server` to a free loopback port and serve it in the background.
    pub async fn start(server: Server) -> TestResult<Self> {
        let listener = Server::bind("127.0.0.1:0".parse()?, &ServeOptions::default()).await?;
        let addr = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            server
                .serve(listener, async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve the test server");
        });
        Ok(Self {
            addr,
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
