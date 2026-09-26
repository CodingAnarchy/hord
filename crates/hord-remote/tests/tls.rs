//! `hord serve` over TLS (ADR 0032), with a CA generated here: login,
//! submit and the event stream work over `https`, the web UI's routes
//! share the port, a client that does not trust the CA is refused, and a
//! plaintext non-loopback bind still needs `--insecure-bind`.

mod common;

use std::path::Path;
use std::time::Duration;

use hord_api::auth::Scope;
use hord_api::{RepoBackend, proto, wire};
use hord_core::sign::{self, SigningKey};
use hord_core::{Actor, Bytes, ChangeRecord, Intent, RepoPath};
use hord_remote::{ConnectOptions, RemoteRepo};
use hord_server::{AuthStore, Error, Hosts, ServeOptions, Server, ServerConfig, TlsConfig};
use hord_txn::{Repo, RepoOptions};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tokio_stream::StreamExt;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
use tower::ServiceExt;

use common::{TestResult, temp};

/// A CA and a server certificate it signed for `localhost` and
/// `127.0.0.1`, written as PEM files under `dir`. Returns the server's
/// TLS config and the CA certificate's PEM.
fn make_certs(dir: &Path) -> TestResult<(TlsConfig, String)> {
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

    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, leaf.pem())?;
    std::fs::write(&key, leaf_key.serialize_pem())?;
    Ok((TlsConfig { cert, key }, ca_cert.pem()))
}

/// A TLS server requiring tokens from `auth`; stopped when the sender
/// drops. Returns its `https://127.0.0.1:<port>` URL.
async fn serve_tls(
    repo: &Path,
    auth: &Path,
    tls: &TlsConfig,
) -> TestResult<(String, tokio::sync::oneshot::Sender<()>)> {
    let hosts = Hosts::open_repo(repo, RepoOptions::default()).await?;
    let server = Server::new(hosts, ServerConfig::default())
        .with_auth(AuthStore::open(auth)?)
        .with_tls(tls)?;
    let options = ServeOptions {
        insecure_bind: false,
        tls: server.tls(),
    };
    let listener = Server::bind("127.0.0.1:0".parse()?, &options).await?;
    let url = format!("https://{}", listener.local_addr()?);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .expect("serve the TLS test server");
    });
    Ok((url, stop))
}

fn trusting(ca: &str, token: Option<String>) -> ConnectOptions {
    ConnectOptions {
        token,
        ca_pem: Some(ca.as_bytes().to_vec()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_submit_and_events_work_over_tls() -> TestResult {
    let dir = temp("works")?;
    let repo_dir = dir.0.join("repo");
    let repo = Repo::create(&repo_dir).await?;
    repo.bootstrap(
        vec![(
            "src/lib.rs".parse::<RepoPath>()?,
            b"pub fn a() {}\n".to_vec(),
        )],
        Intent::from_summary("seed"),
        Actor::Human { id: "seed".into() },
    )
    .await?;
    drop(repo);
    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "root", "pw", &[Scope::Admin])?;
    AuthStore::add_user(&auth, "rita", "pw", &[Scope::Read])?;
    let (tls, ca) = make_certs(&dir.0)?;
    let (url, _stop) = serve_tls(&repo_dir, &auth, &tls).await?;

    // Login over TLS, trusting the CA.
    let anonymous = RemoteRepo::connect_with(&url, &trusting(&ca, None)).await?;
    let login = |user: &str, key: &SigningKey| proto::LoginRequest {
        user: user.into(),
        password: "pw".into(),
        key_id: key.public().key_id(),
    };
    let rita_token = anonymous
        .auth()
        .login(login("rita", &SigningKey::generate()?))
        .await?
        .token;
    let rita = RemoteRepo::connect_with(&url, &trusting(&ca, Some(rita_token))).await?;
    let root_token = anonymous
        .auth()
        .login(login("root", &SigningKey::generate()?))
        .await?
        .token;
    let root = RemoteRepo::connect_with(&url, &trusting(&ca, Some(root_token))).await?;
    let minted = root
        .auth()
        .mint_token(proto::MintTokenRequest {
            agent_id: "bot-1".into(),
            model: "m1".into(),
            harness: "h1".into(),
            scopes: vec!["read".into(), "propose".into()],
        })
        .await?;
    let bot_key = SigningKey::from_pem(&minted.private_key_pem)?;
    let agent = RemoteRepo::connect_with(&url, &trusting(&ca, Some(minted.token))).await?;

    // The event stream, from the start, over TLS.
    let mut events = rita.events(proto::EventsRequest { from: Some(0) }).await?;

    // Submit a change signed by the agent, over TLS.
    let head = agent
        .head(proto::HeadRequest {})
        .await?
        .change
        .ok_or("head")?;
    let reply = agent
        .get_objects(proto::GetObjectsRequest { ids: vec![head] })
        .await?;
    let mut record: ChangeRecord = hord_encoding::decode(&reply.objects[0].cbor)?;
    record.provenance.actor = Actor::Agent {
        id: "bot-1".into(),
        model: "m1".into(),
        model_hash: Bytes::default(),
        harness: "h1".into(),
    };
    record.intent = Intent::from_summary("over tls");
    sign::sign_change(&mut record, &bot_key)?;
    let bytes = hord_encoding::encode(&record)?;
    let change = wire::verified_object(&wire::object(bytes.clone()))?;
    agent
        .put_objects(proto::PutObjectsRequest {
            objects: vec![wire::object(bytes)],
        })
        .await?;
    agent
        .submit(proto::SubmitRequest {
            change: wire::id(change),
        })
        .await?;
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await?
            .ok_or("event stream ended")??;
        if let Some(proto::event::Kind::Submitted(submitted)) = envelope.event.and_then(|e| e.kind)
            && submitted.change == wire::id(change)
        {
            break;
        }
    }

    // The web UI's plain HTTP routes share the TLS port (HTTP/2 by ALPN).
    let channel = Endpoint::from_shared(url.clone())?
        .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(&ca)))?
        .connect()
        .await?;
    let request =
        http::Request::get(format!("{url}/schema.json")).body(tonic::body::Body::empty())?;
    let response = channel.oneshot(request).await?;
    assert_eq!(response.status(), http::StatusCode::OK);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_does_not_trust_the_ca_is_refused() -> TestResult {
    let dir = temp("untrusted")?;
    let repo_dir = dir.0.join("repo");
    drop(Repo::create(&repo_dir).await?);
    let auth = dir.0.join("auth.toml");
    AuthStore::add_user(&auth, "rita", "pw", &[Scope::Read])?;
    let (tls, _ca) = make_certs(&dir.0)?;
    let (url, _stop) = serve_tls(&repo_dir, &auth, &tls).await?;

    // System roots only: the handshake fails, so no call gets through.
    let login = proto::LoginRequest {
        user: "rita".into(),
        password: "pw".into(),
        key_id: SigningKey::generate()?.public().key_id(),
    };
    match RemoteRepo::connect(&url).await {
        Err(_) => {}
        Ok(remote) => {
            let reply = remote.auth().login(login.clone()).await;
            assert!(reply.is_err(), "an untrusted server answered: {reply:?}");
        }
    }
    // Another CA does not help.
    let (_, other_ca) = make_certs(&temp("other-ca")?.0)?;
    match RemoteRepo::connect_with(&url, &trusting(&other_ca, None)).await {
        Err(_) => {}
        Ok(remote) => {
            let reply = remote.auth().login(login.clone()).await;
            assert!(
                reply.is_err(),
                "a server with another CA answered: {reply:?}"
            );
        }
    }
    // Plaintext to the TLS port gets nothing either.
    let plain = url.replacen("https://", "http://", 1);
    if let Ok(remote) = RemoteRepo::connect(&plain).await {
        let reply = remote.auth().login(login).await;
        assert!(reply.is_err(), "plaintext was answered: {reply:?}");
    }
    Ok(())
}

#[tokio::test]
async fn plaintext_needs_insecure_bind_off_loopback_but_tls_does_not() -> TestResult {
    let any = "0.0.0.0:0".parse()?;
    let refused = Server::bind(any, &ServeOptions::default()).await;
    assert!(
        matches!(refused, Err(Error::InsecureBind(_))),
        "{refused:?}"
    );
    let tls = ServeOptions {
        insecure_bind: false,
        tls: true,
    };
    drop(Server::bind(any, &tls).await?);
    Ok(())
}
