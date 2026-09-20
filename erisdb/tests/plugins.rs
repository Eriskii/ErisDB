//! One-shot plugin HTTP contract, with a deliberately unreachable database.
//! A successful call here proves plugin execution does not touch Postgres.

use std::time::Duration;

use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const SECRET: &[u8] = b"plugin-test-secret";

fn registry(script: &str) -> erisdb::PluginRegistry {
    let dir = std::env::temp_dir().join(format!(
        "erisdb-plugin-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = json!({
        "protocol": 1,
        "name": "echo",
        "description": "test echo",
        "executable": "/bin/sh",
        "args": ["-c", script],
        "environment": {"PATH": false},
        "timeout_secs": 5,
        "operations": {
            "call": {
                "description": "echo one request",
                "permission": "echo:call",
                "request_schema": {
                    "type": "object",
                    "required": ["message"],
                    "properties": {"message": {"type": "string"}},
                    "additionalProperties": false
                }
            }
        }
    });
    std::fs::write(
        dir.join("echo.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let registry = erisdb::PluginRegistry::load_dir(&dir).unwrap();
    std::fs::remove_dir_all(dir).ok();
    registry
}

fn app(registry: erisdb::PluginRegistry) -> axum::Router {
    // Nothing is listening here. The plugin routes must still work.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://postgres:postgres@127.0.0.1:1/erisdb")
        .unwrap();
    erisdb::app_with_plugins(pool, SECRET.to_vec(), registry)
}

fn request(token: &str, body: Value) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/v1/call")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn a_call_streams_from_one_process_without_touching_postgres() {
    let script = r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"text/event-stream"}'
printf 'data: first\n\n'
sleep 1
printf 'data: second\n\n'
"#;
    let token = erisdb::auth::mint(SECRET, &["echo:call"], Some(60), None).unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(300),
        app(registry(script)).oneshot(request(
            &token,
            json!({
                "plugin": "echo",
                "operation": "call",
                "input": {"message": "hello"}
            }),
        )),
    )
    .await
    .expect("the response head must arrive before the process exits")
    .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = tokio::time::timeout(Duration::from_secs(2), response.into_body().collect())
        .await
        .expect("stream completes")
        .unwrap()
        .to_bytes();
    assert_eq!(&body[..], b"data: first\n\ndata: second\n\n");
}

#[tokio::test]
async fn manifest_permissions_and_request_schemas_are_enforced() {
    let script = r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"application/json"}'
printf '%s' "$invocation"
"#;
    let registry = registry(script);
    let allowed = erisdb::auth::mint(SECRET, &["echo:call"], Some(60), Some("alice")).unwrap();
    let denied = erisdb::auth::mint(SECRET, &["echo:other"], Some(60), None).unwrap();

    let response = app(registry.clone())
        .oneshot(request(
            &denied,
            json!({"plugin":"echo", "operation":"call", "input":{"message":"hello"}}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);

    let response = app(registry.clone())
        .oneshot(request(
            &allowed,
            json!({"plugin":"echo", "operation":"call", "input":{"message":3}}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 422);

    let response = app(registry)
        .oneshot(request(
            &allowed,
            json!({"plugin":"echo", "operation":"call", "input":{"message":"hello"}}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["plugin"], "echo");
    assert_eq!(body["operation"], "call");
    assert_eq!(body["input"]["message"], "hello");
    assert_eq!(body["context"]["user"], "alice");
}

#[tokio::test]
async fn discovery_only_lists_operations_the_token_can_call() {
    let registry = registry("exit 1");
    let allowed = erisdb::auth::mint(SECRET, &["echo:call"], Some(60), None).unwrap();
    let denied = erisdb::auth::mint(SECRET, &["other:call"], Some(60), None).unwrap();

    for (token, expected) in [(&allowed, 1usize), (&denied, 0usize)] {
        let request = axum::http::Request::builder()
            .uri("/v1/plugins")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app(registry.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["plugins"].as_array().unwrap().len(), expected);
    }
}
