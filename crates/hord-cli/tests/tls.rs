//! The CLI against `hord serve --tls-cert --tls-key --auth` (ADR 0032):
//! `hord remote add origin https://… --ca-file`, `hord login`, and the
//! commands behind them work over TLS; without the CA the login is
//! refused, and `HORD_CA_FILE` supplies it for a remote without its own.

mod common;

use common::TempDir;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn hord(dir: &Path, home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hord"));
    cmd.args(args)
        .current_dir(dir)
        .env("HORD_HOME", home)
        .env("HORD_ACTOR", "tester")
        .env("HORD_NO_DAEMON", "1")
        .env_remove("HORD_CA_FILE")
        .env_remove("HORD_AGENT_MODEL");
    cmd
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn run(mut cmd: Command, stdin: &str) -> TestResult<Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin is piped")?
        .write_all(stdin.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn json_in(dir: &Path, home: &Path, args: &[&str], stdin: &str) -> TestResult<serde_json::Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let out = run(hord(dir, home, &args), stdin)?;
    assert!(out.status.success(), "hord {args:?}: {}", describe(&out));
    let text = String::from_utf8(out.stdout)?;
    serde_json::from_str(&text).map_err(|err| format!("{args:?}: {err}\n{text}").into())
}

fn json(dir: &Path, home: &Path, args: &[&str]) -> TestResult<serde_json::Value> {
    json_in(dir, home, args, "")
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> TestResult<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("{key} is a string: {v:#}").into())
}

fn utf8(path: &Path) -> TestResult<&str> {
    Ok(path.to_str().ok_or("temp path is UTF-8")?)
}

fn git(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()?;
    assert!(out.status.success(), "git {args:?}: {}", describe(&out));
    Ok(())
}

/// A CA, and a certificate it signed for `localhost` and `127.0.0.1`, as
/// PEM files in `dir`: (cert, key, ca).
fn make_certs(dir: &Path) -> TestResult<(PathBuf, PathBuf, PathBuf)> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca_params, ca_key);
    let mut leaf_params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?;
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf_key = KeyPair::generate()?;
    let leaf = leaf_params.signed_by(&leaf_key, &issuer)?;
    let (cert, key, ca) = (
        dir.join("cert.pem"),
        dir.join("key.pem"),
        dir.join("ca.pem"),
    );
    fs::write(&cert, leaf.pem())?;
    fs::write(&key, leaf_key.serialize_pem())?;
    fs::write(&ca, ca_cert.pem())?;
    Ok((cert, key, ca))
}

/// `hord serve`; killed on drop.
struct Serve {
    child: Child,
    url: String,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &Path, home: &Path, args: &[&str]) -> TestResult<Serve> {
    let mut child = hord(dir, home, args)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let stderr = child.stderr.take().ok_or("serve stderr is piped")?;
    let mut serve = Serve {
        child,
        url: String::new(),
    };
    let mut line = String::new();
    BufReader::new(stderr).read_line(&mut line)?;
    serve.url = line
        .trim()
        .strip_prefix("hord serve: ")
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or_else(|| format!("unexpected serve output {line:?}"))?
        .to_owned();
    Ok(serve)
}

#[test]
fn remote_login_and_submit_work_over_tls() -> TestResult {
    let root = TempDir::new("hord-tls-cli")?;
    let (op, ada) = (root.0.join("op"), root.0.join("ada"));
    fs::create_dir_all(&op)?;
    fs::create_dir_all(&ada)?;
    let (cert, key, ca) = make_certs(&root.0)?;

    let origin = root.0.join("origin");
    fs::create_dir_all(origin.join("src"))?;
    fs::write(origin.join("src/lib.rs"), "pub fn a() -> u32 {\n    1\n}\n")?;
    git(&origin, &["init", "-q", "-b", "main"])?;
    git(&origin, &["add", "."])?;
    git(&origin, &["commit", "-q", "-m", "fixture"])?;
    json(&origin, &op, &["init", "--from-git", utf8(&origin)?])?;
    let auth = root.0.join("auth.toml");
    json_in(
        &origin,
        &op,
        &[
            "user",
            "add",
            "ada",
            "--auth-file",
            utf8(&auth)?,
            "--password-stdin",
            "--scope",
            "read",
            "--scope",
            "propose",
        ],
        "ada-pw\n",
    )?;
    let server = serve(
        &origin,
        &op,
        &[
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--auth",
            utf8(&auth)?,
            "--tls-cert",
            utf8(&cert)?,
            "--tls-key",
            utf8(&key)?,
        ],
    )?;
    assert!(server.url.starts_with("https://"), "{}", server.url);

    let clone = root.0.join("clone");
    fs::create_dir_all(&clone)?;
    json(&clone, &ada, &["init"])?;

    // Without the CA, the login is refused.
    json(&clone, &ada, &["remote", "add", "untrusted", &server.url])?;
    let out = run(
        hord(
            &clone,
            &ada,
            &["login", "untrusted", "--user", "ada", "--password-stdin"],
        ),
        "ada-pw\n",
    )?;
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("connect to remote untrusted"), "{stderr}");
    // `HORD_CA_FILE` trusts the CA for a remote without its own.
    let mut with_env = hord(
        &clone,
        &ada,
        &[
            "login",
            "untrusted",
            "--user",
            "ada",
            "--password-stdin",
            "--json",
        ],
    );
    with_env.env("HORD_CA_FILE", &ca);
    let out = run(with_env, "ada-pw\n")?;
    assert!(out.status.success(), "{}", describe(&out));
    json(&clone, &ada, &["remote", "rm", "untrusted"])?;

    // With `--ca-file`, everything goes over TLS.
    json(
        &clone,
        &ada,
        &[
            "remote",
            "add",
            "origin",
            &server.url,
            "--ca-file",
            utf8(&ca)?,
        ],
    )?;
    json(&clone, &ada, &["remote", "set-default", "origin"])?;
    json_in(
        &clone,
        &ada,
        &["login", "origin", "--user", "ada", "--password-stdin"],
        "ada-pw\n",
    )?;
    let ws = json(&clone, &ada, &["ws", "new"])?;
    let id = str_field(&ws, "id")?.to_owned();
    let lib = PathBuf::from(str_field(&ws, "materialization")?).join("src/lib.rs");
    fs::write(&lib, "pub fn a() -> u32 {\n    2\n}\n")?;
    let intent = root.0.join("intent.md");
    fs::write(&intent, "---\nsummary: a is 2\n---\nOver TLS.\n")?;
    let proposed = json(
        &clone,
        &ada,
        &["propose", "-w", &id, "--intent", utf8(&intent)?],
    )?;
    let change = str_field(&proposed, "change")?.to_owned();
    json(&clone, &ada, &["submit", &change])?;
    let queue = json(&clone, &ada, &["queue"])?;
    let entries = queue["entries"].as_array().ok_or("entries is an array")?;
    assert!(
        entries.iter().any(|e| e["change"] == change.as_str()),
        "{queue:#}"
    );
    json(&clone, &ada, &["log"])?;
    Ok(())
}
