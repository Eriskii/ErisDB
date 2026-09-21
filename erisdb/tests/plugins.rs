//! Real HTTP sockets and executable plugins. The database is deliberately
//! unavailable: plugin invocations must not depend on the item store.

use std::{path::PathBuf, time::Duration};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;

const SECRET: &[u8] = b"plugin-test-secret";
const ECHO: &str = r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"application/json"}'
printf '%s' "$invocation"
"#;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("erisdb-plugin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn manifest(&self, script: &str, timeout: u64) -> Value {
        json!({
            "protocol": 1, "name": "echo", "executable": "/bin/sh",
            "args": ["-c", format!("printf '%s\\n' \"$$\" > \"$1\"\n{script}"), "plugin", self.0.join("pid")],
            "environment": {"PATH": false}, "timeout_secs": timeout,
            "operations": {"call": {
                "permission": "echo:call", "request_schema": {
                    "type": "object", "required": ["message"],
                    "properties": {"message": {"type": "string"}},
                    "additionalProperties": false
                }
            }}
        })
    }

    fn write(&self, manifest: &Value) {
        std::fs::write(self.0.join("echo.json"), serde_json::to_vec(manifest).unwrap()).unwrap();
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    directory: Directory,
    base: String,
    http: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(script: &str, timeout: u64) -> Self {
        let directory = Directory::new();
        directory.write(&directory.manifest(script, timeout));
        let registry = erisdb::PluginRegistry::load_dir(&directory.0).unwrap();
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://postgres:postgres@127.0.0.1:1/erisdb")
            .unwrap();
        let app = erisdb::app_with_plugins(pool, SECRET.to_vec(), registry);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { directory, base, http: reqwest::Client::new(), task }
    }

    fn token(grants: &[&str]) -> String {
        erisdb::auth::mint(SECRET, grants, Some(60), Some("alice")).unwrap()
    }

    fn request(&self, grants: &[&str], input: Value) -> reqwest::RequestBuilder {
        self.http.post(format!("{}/v1/call", self.base))
            .bearer_auth(Self::token(grants))
            .json(&json!({"plugin": "echo", "operation": "call", "input": input}))
    }

    async fn call(&self) -> reqwest::Response {
        self.request(&["echo:call"], json!({"message": "hello"})).send().await.unwrap()
    }

    async fn reaped(&self) {
        let pid = std::fs::read_to_string(self.directory.0.join("pid")).unwrap();
        let pid: u32 = pid.trim().parse().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let alive = tokio::process::Command::new("/bin/sh")
                    .args(["-c", "kill -0 \"$1\" 2>/dev/null", "probe", &pid.to_string()])
                    .status().await.unwrap().success();
                if !alive { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the invocation must be killed and reaped");
    }
}

impl Drop for Server {
    fn drop(&mut self) { self.task.abort(); }
}

#[tokio::test]
async fn a_call_streams_before_the_process_exits_and_filters_response_headers() {
    let server = Server::start(r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"text/event-stream","headers":{"content-length":"1","content-type":"text/html","transfer-encoding":"gzip","cache-control":"public","authorization":"secret","set-cookie":"session=secret","www-authenticate":"secret","x-request-id":"req_real"}}'
printf 'data: first\n\n'
sleep 2
printf 'data: second\n\n'
"#, 10).await;
    let mut response = tokio::time::timeout(Duration::from_secs(1), server.call()).await
        .expect("the response head must arrive before the process exits");
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["transfer-encoding"], "chunked");
    assert_eq!(response.headers()["x-request-id"], "req_real");
    for header in ["content-length", "authorization", "set-cookie", "www-authenticate"] {
        assert!(!response.headers().contains_key(header), "leaked {header}");
    }
    let first = tokio::time::timeout(Duration::from_secs(1), response.chunk()).await.unwrap().unwrap().unwrap();
    assert_eq!(first, "data: first\n\n");
    assert_eq!(response.text().await.unwrap(), "data: second\n\n");
    server.reaped().await;
}

#[tokio::test]
async fn permissions_schema_discovery_and_invocation_work_over_http() {
    let server = Server::start(ECHO, 5).await;
    for (grants, input, status) in [
        (vec!["echo:other"], json!({"message": "hello"}), 403),
        (vec!["echo:call"], json!({"message": 3}), 422),
    ] {
        let response = server.request(&grants, input).send().await.unwrap();
        assert_eq!(response.status(), status);
        assert!(!server.directory.0.join("pid").exists(), "rejected requests must not spawn a plugin");
    }
    for (grant, count) in [("echo:call", 1), ("other:call", 0)] {
        let response = server.http.get(format!("{}/v1/plugins", server.base))
            .bearer_auth(Server::token(&[grant])).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["plugins"].as_array().unwrap().len(), count);
    }
    let response = server.call().await;
    assert_eq!(response.status(), 200);
    let invocation: Value = response.json().await.unwrap();
    assert_eq!(invocation, json!({
        "protocol": 1, "plugin": "echo", "operation": "call",
        "input": {"message": "hello"}, "context": {"user": "alice"}
    }));
    server.reaped().await;
}

#[tokio::test]
async fn oversized_unterminated_response_heads_are_rejected_without_waiting_for_timeout() {
    let server = Server::start(r#"
IFS= read -r invocation
printf '%20000s' x
exec sleep 30
"#, 10).await;
    let response = tokio::time::timeout(Duration::from_secs(2), server.call()).await
        .expect("oversized headers must fail as soon as the limit is exceeded");
    assert_eq!(response.status(), 502);
    server.reaped().await;
}

#[tokio::test]
async fn timeout_covers_process_exit_after_stdout_closes() {
    let server = Server::start(r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"text/plain"}'
printf 'started'
exec 1>&-
exec sleep 30
"#, 1).await;
    let response = server.call().await;
    assert_eq!(response.status(), 200);
    let body = tokio::time::timeout(Duration::from_secs(3), response.text()).await
        .expect("closing stdout must not bypass the process deadline");
    assert!(body.is_err(), "a timed out invocation must not look complete");
    server.reaped().await;
}

#[tokio::test]
async fn disconnect_kills_silent_plugins_and_releases_process_slots() {
    let server = Server::start(r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"text/plain"}'
printf 'started'
exec sleep 30
"#, 10).await;
    // More calls than the registry limit proves disconnected requests release
    // their slots; each real process must disappear before the next call.
    for _ in 0..=erisdb::plugin::MAX_PLUGIN_PROCESSES {
        let response = server.call().await;
        assert_eq!(response.status(), 200);
        drop(response);
        server.reaped().await;
    }
}

#[tokio::test]
async fn a_nonzero_exit_after_the_body_is_an_incomplete_http_response() {
    let server = Server::start(r#"
IFS= read -r invocation
printf '%s\n' '{"protocol":1,"status":200,"content_type":"text/plain"}'
printf 'partial'
sleep 1
exit 9
"#, 5).await;
    let response = server.call().await;
    assert_eq!(response.status(), 200);
    assert!(response.text().await.is_err());
    server.reaped().await;
}

#[tokio::test]
async fn invalid_deployment_manifests_fail_the_real_server_startup() {
    let directory = Directory::new();
    let original = directory.manifest(ECHO, 5);
    for (pointer, value) in [
        ("/operations/call/permission", json!("other:call")),
        ("/operations/call/request_schema", json!({"$ref": "https://example.com/schema.json"})),
    ] {
        let mut manifest = original.clone();
        *manifest.pointer_mut(pointer).unwrap() = value;
        directory.write(&manifest);
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_erisdb"))
            .args(["serve", "--database-url", "postgres://postgres:postgres@127.0.0.1:1/erisdb",
                "--secret", "plugin-test-secret", "--no-iroh", "--plugin-dir"])
            .arg(&directory.0).output().await.unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("namespace") || error.contains("outside the document"), "{error}");
    }
}
