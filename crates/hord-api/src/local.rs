//! The local endpoint of a per-repo daemon (ADR 0021): a Unix domain socket,
//! or a named pipe on Windows, named after the repository.
//!
//! The name is derived from the canonical repository root, so every process
//! that opens the same repository finds the same endpoint. It lives in the
//! temporary directory rather than under `.hord/`, because a Unix socket
//! path is limited to about 100 bytes.

use std::path::Path;

/// Where the daemon for the repository at `repo_root` listens: a socket
/// path on Unix, a pipe name (`\\.\pipe\hord-…`) on Windows.
pub fn endpoint(repo_root: &Path) -> std::io::Result<String> {
    let root = std::fs::canonicalize(repo_root)?;
    let hash = blake3::hash(root.to_string_lossy().as_bytes());
    let short = &hash.to_hex()[..16];
    Ok(platform_endpoint(short))
}

#[cfg(unix)]
fn platform_endpoint(short: &str) -> String {
    std::env::temp_dir()
        .join(format!("hord-{short}.sock"))
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn platform_endpoint(short: &str) -> String {
    format!(r"\\.\pipe\hord-{short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_repository_one_endpoint() -> Result<(), Box<dyn std::error::Error>> {
        let here = endpoint(Path::new("."))?;
        let again = endpoint(&std::env::current_dir()?)?;
        assert_eq!(here, again);
        let other = endpoint(&std::env::temp_dir())?;
        assert_ne!(here, other);
        assert!(here.len() < 104, "{here} fits a Unix socket path");
        Ok(())
    }
}
