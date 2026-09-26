//! The GitHub REST client against a local stand-in for the API (no
//! network): what it asks for, and how it reads the answers.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Request, Response, StatusCode, header};
use hord_git::sync::{GitHub, GitHubOptions, PullRequests, StatusState};
use serde_json::{Value, json};
use tokio::net::TcpListener;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Method, path and query, authorization header, and JSON body.
type Seen = Arc<Mutex<Vec<(String, String, String, Value)>>>;

async fn api(State(seen): State<Seen>, request: Request<Body>) -> Response<Body> {
    let method = request.method().to_string();
    let uri = request.uri().to_string();
    let auth = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    seen.lock().unwrap_or_else(PoisonError::into_inner).push((
        method.clone(),
        uri.clone(),
        auth,
        body,
    ));
    let (status, reply) = if method == "GET" && uri.starts_with("/repos/o/r/pulls?") {
        let pulls = json!([{
            "number": 5,
            "title": "Fix it",
            "body": null,
            "html_url": "https://github.com/o/r/pull/5",
            "head": { "sha": "0123456789abcdef0123456789abcdef01234567" }
        }]);
        (StatusCode::OK, pulls)
    } else if method == "PATCH" {
        // Already closed.
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"message": "closed"}),
        )
    } else if uri.contains("/statuses/bad") {
        (StatusCode::NOT_FOUND, json!({"message": "Not Found"}))
    } else {
        (StatusCode::CREATED, json!({}))
    };
    let mut response = Response::new(Body::from(reply.to_string()));
    *response.status_mut() = status;
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_github_client_speaks_the_rest_api() -> TestResult {
    let seen: Seen = Arc::default();
    let app = Router::new().fallback(api).with_state(Arc::clone(&seen));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });

    let github = GitHub::new(GitHubOptions {
        api: format!("http://{addr}/"),
        repository: "o/r".into(),
        token: "secret".into(),
        base: "main".into(),
    })?;
    let pulls = github.open_pulls().await?;
    assert_eq!(pulls.len(), 1);
    let pull = &pulls[0];
    assert_eq!(pull.number, 5);
    assert_eq!(pull.body, "", "a null body is empty");
    assert_eq!(pull.git_ref, "refs/pull/5/head");
    assert_eq!(pull.url, "https://github.com/o/r/pull/5");

    github
        .set_status(5, &pull.head_sha, StatusState::Failure, "Parked")
        .await?;
    github.comment(5, "**Parked.**").await?;
    github.close(5).await?;
    let err = github
        .set_status(5, "bad", StatusState::Pending, "x")
        .await
        .err()
        .ok_or("a 404 is an error")?;
    assert!(err.to_string().contains("404"), "{err}");

    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(seen.iter().all(|(_, _, auth, _)| auth == "Bearer secret"));
    let calls: Vec<(&str, &str)> = seen
        .iter()
        .map(|(m, u, _, _)| (m.as_str(), u.as_str()))
        .collect();
    assert_eq!(
        calls[..4],
        [
            (
                "GET",
                "/repos/o/r/pulls?state=open&base=main&per_page=100&page=1"
            ),
            (
                "POST",
                "/repos/o/r/statuses/0123456789abcdef0123456789abcdef01234567"
            ),
            ("POST", "/repos/o/r/issues/5/comments"),
            ("PATCH", "/repos/o/r/pulls/5"),
        ]
    );
    assert_eq!(
        seen[1].3,
        json!({"state": "failure", "description": "Parked", "context": "hord/lander"})
    );
    assert_eq!(seen[2].3, json!({"body": "**Parked.**"}));
    assert_eq!(seen[3].3, json!({"state": "closed"}));
    Ok(())
}
