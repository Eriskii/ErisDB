//! The real executable and its failure paths, with no simulated provider.
//! The live contract test is explicit because it consumes provider credits.

use std::{process::Stdio, time::Duration};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

fn invocation(input: Value) -> Value {
    json!({"protocol": 1, "plugin": "openai", "operation": "chat.completions", "input": input})
}

async fn invoke(request: &[u8], environment: &[(&str, &str)]) -> std::process::Output {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_erisdb-plugin-openai"))
        .env_clear().envs(environment.iter().copied())
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn().unwrap();
    child.stdin.take().unwrap().write_all(request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await
        .expect("the executable must finish").unwrap()
}

fn response(output: std::process::Output) -> (Value, Vec<u8>) {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let split = output.stdout.iter().position(|byte| *byte == b'\n').expect("protocol response head");
    (serde_json::from_slice(&output.stdout[..split]).unwrap(), output.stdout[split + 1..].to_vec())
}

#[tokio::test]
async fn executable_rejects_invalid_invocations_and_missing_credentials() {
    let valid = invocation(json!({"model": "unused", "messages": []}));
    let mut protocol = valid.clone();
    protocol["protocol"] = json!(2);
    let mut operation = valid.clone();
    operation["operation"] = json!("unsupported");
    for (request, message) in [
        (b"not json".to_vec(), "parsing invocation"),
        (serde_json::to_vec(&protocol).unwrap(), "protocol 2 is not supported"),
        (serde_json::to_vec(&operation).unwrap(), "unsupported operation"),
        (serde_json::to_vec(&valid).unwrap(), "OPENAI_API_KEY is not set"),
    ] {
        let output = invoke(&request, &[]).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty(), "invalid invocations must not produce success headers");
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
}

#[tokio::test]
async fn unavailable_upstream_returns_a_complete_protocol_error() {
    // Reserve an actual local port, then close it. No service implements or
    // fabricates provider behavior; the executable must handle connection loss.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    drop(listener);
    let request = serde_json::to_vec(&invocation(json!({"model": "unused", "messages": []}))).unwrap();
    let (head, bytes) = response(invoke(&request, &[
        ("OPENAI_API_KEY", "failure-path-only"), ("OPENAI_BASE_URL", &base),
    ]).await);
    assert_eq!(head["protocol"], 1);
    assert_eq!(head["status"], 502);
    assert_eq!(head["content_type"], "application/json");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"], "upstream_unavailable");
    assert!(body["detail"].as_str().unwrap().contains("error sending request"));
}

#[tokio::test]
#[ignore = "calls the real provider; set OPENAI_API_KEY and ERISDB_TEST_OPENAI_MODEL and run --ignored"]
async fn live_provider_tool_calls_and_sse_pass_through_the_executable() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let model = std::env::var("ERISDB_TEST_OPENAI_MODEL").expect("ERISDB_TEST_OPENAI_MODEL");
    let mut environment = vec![("OPENAI_API_KEY".to_owned(), key)];
    for name in ["OPENAI_BASE_URL", "OPENAI_ORGANIZATION", "OPENAI_PROJECT"] {
        if let Ok(value) = std::env::var(name) { environment.push((name.to_owned(), value)); }
    }
    let environment: Vec<_> = environment.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
    let input = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Call lookup with no arguments."}],
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {
            "type": "object", "properties": {}, "additionalProperties": false
        }}}],
        "tool_choice": {"type": "function", "function": {"name": "lookup"}}
    });
    let (head, bytes) = response(invoke(&serde_json::to_vec(&invocation(input.clone())).unwrap(), &environment).await);
    assert_eq!(head["status"], 200, "{}", String::from_utf8_lossy(&bytes));
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "lookup");
    let mut streaming = input;
    streaming["stream"] = json!(true);
    let (head, bytes) = response(invoke(&serde_json::to_vec(&invocation(streaming)).unwrap(), &environment).await);
    assert_eq!(head["status"], 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(head["content_type"], "text/event-stream");
    let events = String::from_utf8(bytes).unwrap();
    assert!(events.contains("\"tool_calls\""));
    assert!(events.contains("data: [DONE]"));
}
