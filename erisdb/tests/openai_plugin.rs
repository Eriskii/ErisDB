//! The OpenAI executable against a local upstream: no real key or network.

use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use futures::stream;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

async fn chat_completions(headers: HeaderMap, Json(body): Json<Value>) -> Response<Body> {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer test-key") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "wrong key"})),
        )
            .into_response();
    }
    if body["stream"] == true {
        let chunks = stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            )),
            Ok::<_, Infallible>(Bytes::from_static(b"data: [DONE]\n\n")),
        ]);
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("x-request-id", "req_test")
            .body(Body::from_stream(chunks))
            .unwrap();
    }
    Json(json!({
        "object": "chat.completion",
        "received": body,
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id":"call_1", "type":"function", "function":{"name":"lookup", "arguments":"{}"}}]
            },
            "finish_reason": "tool_calls"
        }]
    }))
    .into_response()
}

async fn upstream() -> String {
    let app = Router::new().route("/v1/chat/completions", post(chat_completions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}/v1")
}

async fn invoke(base_url: &str, input: Value) -> (Value, Vec<u8>) {
    let executable = env!("CARGO_BIN_EXE_erisdb-plugin-openai");
    let mut child = tokio::process::Command::new(executable)
        .env_clear()
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base_url)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let invocation = json!({
        "protocol": 1,
        "plugin": "openai",
        "operation": "chat.completions",
        "input": input,
        "context": {"user": "alice"}
    });
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&serde_json::to_vec(&invocation).unwrap())
        .await
        .unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let split = output
        .stdout
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap();
    let head = serde_json::from_slice(&output.stdout[..split]).unwrap();
    (head, output.stdout[split + 1..].to_vec())
}

#[tokio::test]
async fn chat_completion_bodies_and_tool_calls_pass_through() {
    let base = upstream().await;
    let input = json!({
        "model": "test-model",
        "messages": [{"role":"user", "content":"use a tool"}],
        "tools": [{"type":"function", "function":{"name":"lookup", "parameters":{"type":"object"}}}],
        "tool_choice": "auto"
    });
    let (head, body) = invoke(&base, input.clone()).await;
    assert_eq!(head["status"], 200);
    assert_eq!(head["content_type"], "application/json");
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["received"], input);
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
}

#[tokio::test]
async fn chat_completion_sse_is_streamed_without_rewriting() {
    let base = upstream().await;
    let input = json!({
        "model": "test-model",
        "messages": [{"role":"user", "content":"hello"}],
        "stream": true
    });
    let (head, body) = invoke(&base, input).await;
    assert_eq!(head["status"], 200);
    assert_eq!(head["content_type"], "text/event-stream");
    assert_eq!(head["headers"]["x-request-id"], "req_test");
    assert_eq!(
        body,
        b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n"
    );
}
