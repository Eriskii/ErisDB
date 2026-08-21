//! The MCP contract, proven end to end: real Postgres (Docker via
//! testcontainers), a real bezel serving real HTTP on a real socket, and
//! the bezel-mcp binary as a real subprocess speaking JSON-RPC over its
//! stdio. No mocks.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const SECRET: &[u8] = b"mcp-e2e-secret";

/// A migrated store and a core serving it over TCP; returns the base URL.
async fn spawn_core() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default().start().await.expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("pg port");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"))
        .await
        .expect("db connect");
    bezel::MIGRATOR.run(&pool).await.expect("migrate");
    let app = bezel::app(pool, SECRET.to_vec());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .unwrap();
    });
    (container, format!("http://{addr}"))
}

/// The bezel-mcp binary under test, driven over its stdio.
struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: i64,
}

impl Mcp {
    fn spawn(url: &str, token: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bezel-mcp"))
            .env("BEZEL_URL", url)
            .env("BEZEL_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn bezel-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self { _child: child, stdin, stdout, next_id: 0 }
    }

    async fn send(&mut self, msg: Value) {
        let line = format!("{msg}\n");
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.stdout.next_line())
                .await
                .expect("response timeout")
                .expect("read stdout")
                .expect("server closed its stdout");
            let v: Value = serde_json::from_str(&line).expect("stdout carries only JSON-RPC");
            if v["id"] == json!(id) {
                assert!(v.get("error").is_none(), "rpc error: {v}");
                return v["result"].clone();
            }
        }
    }

    async fn notify(&mut self, method: &str) {
        self.send(json!({"jsonrpc": "2.0", "method": method})).await;
    }

    /// Call a tool expecting success; the result's text content is JSON.
    async fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.call("tools/call", json!({"name": name, "arguments": args})).await;
        assert_ne!(r["isError"], json!(true), "tool {name} errored: {r}");
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Call a tool expecting failure; returns the error text.
    async fn tool_err(&mut self, name: &str, args: Value) -> String {
        let r = self.call("tools/call", json!({"name": name, "arguments": args})).await;
        assert_eq!(r["isError"], json!(true), "tool {name} unexpectedly succeeded: {r}");
        r["content"][0]["text"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn the_mcp_server_drives_a_bezel() {
    let (_pg, url) = spawn_core().await;
    let root = bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), Some("clod"))
        .unwrap();
    let mut mcp = Mcp::spawn(&url, &root);

    // MCP handshake.
    let init = mcp
        .call(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "0"}
            }),
        )
        .await;
    assert_eq!(init["serverInfo"]["name"], "bezel-mcp");
    assert!(init["capabilities"]["tools"].is_object());
    mcp.notify("notifications/initialized").await;

    // The full toolbox is advertised, each tool with a schema.
    let tools = mcp.call("tools/list", json!({})).await;
    let names: Vec<&str> =
        tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "list_facets",
        "read_items",
        "get_item",
        "search_items",
        "create_item",
        "update_item",
        "delete_item",
        "item_history",
        "revert_item",
        "read_changes",
        "mint_capability",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}, have {names:?}");
    }
    for t in tools["tools"].as_array().unwrap() {
        assert!(t["inputSchema"]["type"] == "object", "tool {} lacks a schema", t["name"]);
    }

    // Registering a facet is just a write to the meta-facet.
    mcp.tool(
        "create_item",
        json!({"facet": "facet", "body": {"name": "notes/v1", "strict": false, "schema": {}}}),
    )
    .await;
    let facets = mcp.tool("list_facets", json!({})).await;
    assert!(
        facets["items"].as_array().unwrap().iter().any(|i| i["body"]["name"] == "notes/v1"),
        "{facets}"
    );

    // Create and read back.
    let created = mcp
        .tool("create_item", json!({"facet": "notes/v1", "body": {"title": "groceries", "text": "buy oat milk"}}))
        .await;
    let id = created["id"].as_str().unwrap().to_string();
    let items = mcp.tool("read_items", json!({"facet": "notes/v1"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 1);

    // Search without naming a facet sweeps every registered one,
    // case-insensitively.
    let hits = mcp.tool("search_items", json!({"query": "OAT MILK"})).await;
    assert!(hits["items"].as_array().unwrap().iter().any(|i| i["id"] == id.as_str()), "{hits}");
    let misses = mcp.tool("search_items", json!({"query": "no such thing anywhere"})).await;
    assert_eq!(misses["items"].as_array().unwrap().len(), 0);

    // Update without a revision: the tool fetches the current one.
    mcp.tool(
        "update_item",
        json!({"id": id, "body": {"title": "groceries", "text": "buy oat milk and bread"}}),
    )
    .await;
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert!(item["body"]["text"].as_str().unwrap().contains("bread"));

    // Two states so far; revert to the first, which lands as a third.
    let hist = mcp.tool("item_history", json!({"id": id})).await;
    let states = hist["history"].as_array().unwrap();
    assert_eq!(states.len(), 2);
    let first_seq = states[0]["seq"].as_i64().unwrap();
    mcp.tool("revert_item", json!({"id": id, "seq": first_seq})).await;
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert_eq!(item["body"]["text"], "buy oat milk");
    assert_eq!(item["revision"], 3);

    // The change feed saw everything, and pages by cursor.
    let changes = mcp.tool("read_changes", json!({"since": 0})).await;
    assert!(changes["changes"].as_array().unwrap().len() >= 4, "{changes}");
    assert!(changes["next"].as_i64().unwrap() > 0);

    // Delete; the item is gone but its history is not.
    mcp.tool("delete_item", json!({"id": id})).await;
    let items = mcp.tool("read_items", json!({"facet": "notes/v1"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);
    let hist = mcp.tool("item_history", json!({"id": id})).await;
    assert_eq!(hist["history"].as_array().unwrap().len(), 4);

    // Admin surface: mint a narrower token.
    let minted = mcp
        .tool("mint_capability", json!({"facets": ["notes/v1"], "verbs": ["read"], "ttl_secs": 60}))
        .await;
    assert!(minted["token"].as_str().unwrap().starts_with("bz1."));

    // Failures are tool errors, not crashes: the loop keeps serving.
    let err = mcp
        .tool_err("get_item", json!({"id": "00000000-0000-0000-0000-000000000000"}))
        .await;
    assert!(err.contains("404"), "{err}");
    let items = mcp.tool("read_items", json!({"facet": "notes/v1"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);
}
