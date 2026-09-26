//! The team host kit in `deploy/` (docs/hosting.md) stays in step with the
//! server: its sample `server.toml` parses, serves TLS with an auth file,
//! and binds every interface, which TLS allows without `--insecure-bind`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hord_server::{Error, ServeOptions, ServerConfig, check_bind};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn deploy() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy")
}

#[test]
fn the_sample_server_toml_parses_with_tls_and_auth() -> TestResult {
    let path = deploy().join("server.toml");
    let config = ServerConfig::load(&path)?;
    let tls = config.tls.ok_or("the sample serves TLS")?;
    // `ends_with`: on Windows, `/etc/…` joins onto the checkout's drive.
    assert!(tls.cert.ends_with("etc/hord/tls/cert.pem"), "{tls:?}");
    assert!(tls.key.ends_with("etc/hord/tls/key.pem"), "{tls:?}");
    let auth = config.auth.ok_or("the sample requires tokens")?;
    assert!(auth.file.ends_with("etc/hord/auth.toml"), "{auth:?}");
    assert!(config.webhooks.is_empty());

    let bind: SocketAddr = config.bind.ok_or("the sample binds")?.parse()?;
    assert!(!bind.ip().is_loopback());
    let with_tls = ServeOptions {
        insecure_bind: false,
        tls: true,
    };
    check_bind(bind, &with_tls)?;
    assert!(matches!(
        check_bind(bind, &ServeOptions::default()),
        Err(Error::InsecureBind(_))
    ));
    Ok(())
}

#[test]
fn the_systemd_unit_serves_the_sample_config() -> TestResult {
    let unit = std::fs::read_to_string(deploy().join("hord.service"))?;
    let exec = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .ok_or("the unit has ExecStart")?;
    let args: Vec<&str> = exec.split_whitespace().collect();
    assert_eq!(args[..2], ["/usr/local/bin/hord", "serve"]);
    assert!(
        args.contains(&"--repo") && args.contains(&"--config"),
        "{exec}"
    );
    assert!(!args.contains(&"--insecure-bind"), "{exec}");
    assert!(
        unit.contains("KillSignal=SIGINT"),
        "hord serve stops cleanly on SIGINT"
    );
    Ok(())
}
