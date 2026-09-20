//! The MCP contract, proven end to end: real Postgres (Docker via
//! testcontainers), a real erisdb binary serving real HTTP on a real
//! socket, and the erisdb-mcp binary as a real subprocess speaking
//! JSON-RPC over its stdio. No mocks.
//!
//! The core is driven as a process and built from the sibling crate when
//! needed. Missing prerequisites fail the suite rather than silently skipping.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const SECRET: &str = "mcp-e2e-secret";

/// The erisdb binary to run a core with: `ERISDB_BIN` if the operator named
/// one, a sibling `../erisdb` checkout built on demand, or `erisdb` on
/// PATH. `None` means this machine cannot run a core right now.
fn erisdb_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ERISDB_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../erisdb/Cargo.toml");
    if manifest.is_file() {
        let built = std::process::Command::new(env!("CARGO"))
            .args(["build", "--bin", "erisdb", "--manifest-path"])
            .arg(&manifest)
            .status();
        let binary = manifest.with_file_name("target/debug/erisdb");
        if matches!(built, Ok(status) if status.success()) && binary.is_file() {
            return Some(binary);
        }
    }

    let found = std::process::Command::new("sh").args(["-c", "command -v erisdb"]).output().ok()?;
    let path = PathBuf::from(String::from_utf8_lossy(&found.stdout).trim());
    path.is_file().then_some(path)
}

/// A migrated store and a core serving it over TCP, both as real
/// processes.
struct Core {
    _pg: ContainerAsync<Postgres>,
    server: Child,
    url: String,
    erisdb: PathBuf,
}

impl Core {
    /// Cut a token with the same CLI an operator would use. `grants` is
    /// what `--grant` takes: permissions, comma-separated.
    fn mint(&self, grants: &str, ttl: i64) -> String {
        let out = std::process::Command::new(&self.erisdb)
            .args(["mint", "--grant", grants])
            .args(["--ttl", &ttl.to_string(), "--user", "clod", "--secret", SECRET])
            .output()
            .expect("run erisdb mint");
        assert!(out.status.success(), "mint failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    async fn api(&self, method: reqwest::Method, path: &str, token: &str, mut body: Value) -> Value {
        if path == "/v1/pair/redeem" { body["challenge"] = json!("tMNl5Xr1UF5oz0tLgctGskAAFN2mmd_MHTaUNW9LCww"); }
        let mut req =
            reqwest::Client::new().request(method.clone(), format!("{}{path}", self.url)).bearer_auth(token).header("X-ErisDB-Client-Proof", "mcp-integration-browser-secret-not-used-outside-tests");
        if method != reqwest::Method::GET {
            req = req.json(&body);
        }
        let resp = req.send().await.expect("reach the core");
        let status = resp.status();
        let text = resp.text().await.expect("read the body");
        assert!(status.is_success(), "{method} {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// Cut a pairing code over the ordinary API, exactly as `erisdb pair`
    /// does. Returns the session id and the code itself.
    async fn cut_a_pairing(&self, operator: &str) -> (String, String) {
        let cut =
            self.api(reqwest::Method::POST, "/v1/pairings", operator, json!({"ttl_secs": 600})).await;
        (
            cut["id"].as_str().expect("an id").to_string(),
            cut["secret"].as_str().expect("a code").to_string(),
        )
    }

    /// A client redeeming a code with its manifest: who it is, and what
    /// it would like.
    async fn redeem(&self, code: &str, client: &str, requested: &[&str]) -> Value {
        self.api(
            reqwest::Method::POST,
            "/v1/pair/redeem",
            code,
            json!({"client": client, "requested": requested}),
        )
        .await
    }

    /// The paired client collecting whatever it was granted — once.
    async fn collect(&self, code: &str) -> Value {
        self.api(reqwest::Method::GET, "/v1/pair/status", code, Value::Null).await
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.server.start_kill();
    }
}

async fn spawn_core() -> Option<Core> {
    let erisdb = erisdb_binary()?;
    let pg = Postgres::default().start().await.expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
    let database = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

    // Claim a port by binding it and letting go: the core takes it next.
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("find a free port")
        .local_addr()
        .expect("local addr")
        .port();
    let url = format!("http://127.0.0.1:{port}");

    let server = Command::new(&erisdb)
        .args(["serve", "--database-url", &database])
        .args(["--listen", &format!("127.0.0.1:{port}"), "--secret", SECRET, "--no-iroh"])
        .kill_on_drop(true)
        .spawn()
        .expect("spawn erisdb serve");

    // The core migrates the store before it listens, so health answering
    // is the signal that everything is up.
    let http = reqwest::Client::new();
    for _ in 0..200 {
        if http.get(format!("{url}/v1/health")).send().await.is_ok_and(|r| r.status() == 200) {
            return Some(Core { _pg: pg, server, url, erisdb });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the core never answered on {url}");
}

/// A core, or a clear word about why there isn't one.
macro_rules! real_core {
    () => {
        match spawn_core().await {
            Some(core) => core,
            None => {
                panic!("a real ErisDB binary is required; build ../erisdb or set ERISDB_BIN");
            }
        }
    };
}

/// The erisdb-mcp binary under test, driven over its stdio.
struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: i64,
}

impl Mcp {
    fn spawn(url: &str, token: &str) -> Self {
        Self::spawn_with(url, &[("ERISDB_TOKEN", token)])
    }

    /// The binary with exactly this environment beyond `ERISDB_URL` —
    /// which credential channel it reads, and which tools are switched on.
    fn spawn_with(url: &str, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_erisdb-mcp"));
        command.env("ERISDB_URL", url);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn erisdb-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self { _child: child, stdin, stdout, next_id: 0 }
    }

    async fn send(&mut self, msg: Value) {
        let line = format!("{msg}\n");
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// The whole JSON-RPC envelope, error and all.
    async fn call_raw(&mut self, method: &str, params: Value) -> Value {
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
                return v;
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let v = self.call_raw(method, params).await;
        assert!(v.get("error").is_none(), "rpc error: {v}");
        v["result"].clone()
    }

    async fn notify(&mut self, method: &str) {
        self.send(json!({"jsonrpc": "2.0", "method": method})).await;
    }

    /// Handshake and hand back the tool list.
    async fn start(&mut self) -> Value {
        let init = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "e2e", "version": "0"}
                }),
            )
            .await;
        assert_eq!(init["serverInfo"]["name"], "erisdb-mcp");
        assert!(init["capabilities"]["tools"].is_object());
        self.notify("notifications/initialized").await;
        self.call("tools/list", json!({})).await
    }

    /// Call a tool expecting success; the result's text content is JSON.
    async fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.call("tools/call", json!({"name": name, "arguments": args})).await;
        assert_ne!(r["isError"], json!(true), "tool {name} errored: {r}");
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Call a tool expecting failure; returns the error text, whether the
    /// tool refused or the arguments never got past the schema.
    async fn tool_err(&mut self, name: &str, args: Value) -> String {
        let v = self.call_raw("tools/call", json!({"name": name, "arguments": args})).await;
        if let Some(error) = v.get("error") {
            return error.to_string();
        }
        let r = &v["result"];
        assert_eq!(r["isError"], json!(true), "tool {name} unexpectedly succeeded: {r}");
        r["content"][0]["text"].as_str().unwrap().to_string()
    }
}

fn schema_of<'a>(tools: &'a Value, name: &str) -> &'a Value {
    tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

fn requires(tools: &Value, name: &str, field: &str) -> bool {
    schema_of(tools, name)["inputSchema"]["required"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r == field)
}

// ---------------------------------------------------------------------
// No core needed: the handshake, the toolbox, and every guard that
// refuses before it reaches the network.
// ---------------------------------------------------------------------

/// The full toolbox is advertised, each tool with a schema — and the
/// schemas themselves carry the guards. A field the model may omit is a
/// field the model will omit, so the ones whose absence costs data are
/// required, not optional.
#[tokio::test]
async fn the_toolbox_is_advertised_with_its_guards() {
    let mut mcp = Mcp::spawn("http://127.0.0.1:1", "bz1.not-a-real-token");
    let tools = mcp.start().await;

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
        "server_state",
        "my_permissions",
        "list_pairings",
        "get_pairing",
        "approve_pairing",
        "deny_pairing",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}, have {names:?}");
    }
    for t in tools["tools"].as_array().unwrap() {
        assert!(t["inputSchema"]["type"] == "object", "tool {} lacks a schema", t["name"]);
    }

    // Omitting a revision used to mean "fetch whatever is current and
    // overwrite it", which turns a partial view into silent data loss.
    assert!(requires(&tools, "update_item", "revision"), "update_item must demand a revision");
    assert!(requires(&tools, "revert_item", "revision"), "revert_item must demand a revision");

    // Deleting is not something to arrive at by omission.
    assert!(requires(&tools, "delete_item", "confirm"), "delete_item must demand confirmation");

    // A token with no expiry is forever; asking for one has to be
    // deliberate.
    assert!(requires(&tools, "mint_capability", "ttl_secs"), "mint_capability must demand a ttl");
    assert!(requires(&tools, "mint_capability", "grants"), "mint_capability mints grants");

    // Approving hands a stranger a credential. Which permissions is not
    // a field to arrive at by omission, so "approve what was asked" is
    // not a default — the grants get typed out.
    assert!(
        requires(&tools, "approve_pairing", "granted"),
        "approve_pairing must name what it is granting"
    );
}

/// An update with no revision never reaches the core: it is rejected at
/// the schema, where the model can see what it left out.
#[tokio::test]
async fn a_write_without_a_revision_is_rejected() {
    let mut mcp = Mcp::spawn("http://127.0.0.1:1", "bz1.not-a-real-token");
    mcp.start().await;

    let id = "00000000-0000-0000-0000-000000000000";
    let err = mcp.tool_err("update_item", json!({"id": id, "body": {"text": "clobber"}})).await;
    assert!(err.contains("revision"), "{err}");

    let err = mcp.tool_err("revert_item", json!({"id": id, "seq": 1})).await;
    assert!(err.contains("revision"), "{err}");
}

/// Minting hands a live credential to the model, where it lands in the
/// transcript and every copy of it. It is off until the operator says
/// otherwise, and it refuses before it ever asks the core.
#[tokio::test]
async fn minting_is_off_until_the_operator_turns_it_on() {
    let mut mcp = Mcp::spawn("http://127.0.0.1:1", "bz1.not-a-real-token");
    mcp.start().await;

    let err = mcp
        .tool_err("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 60}))
        .await;
    assert!(err.contains("ERISDB_MCP_ALLOW_MINT"), "the refusal names its switch: {err}");
}

/// Approving a pairing is the security boundary of the whole pairing
/// design: it is the step whose entire purpose is that a human performs
/// it. So it is off until the operator says otherwise, and it refuses
/// before it ever asks the core.
#[tokio::test]
async fn approving_a_pairing_is_off_until_the_operator_turns_it_on() {
    let mut mcp = Mcp::spawn("http://127.0.0.1:1", "bz1.not-a-real-token");
    mcp.start().await;

    let id = "00000000-0000-0000-0000-000000000000";
    let err = mcp
        .tool_err("approve_pairing", json!({"id": id, "granted": ["notes:read"]}))
        .await;
    assert!(err.contains("ERISDB_MCP_ALLOW_APPROVE"), "the refusal names its switch: {err}");
}

/// Denying is not gated, and does not need to be: it only ever takes
/// authority away. The worst an agent can do with it is cost you a
/// second pairing code.
#[tokio::test]
async fn denying_a_pairing_needs_no_switch() {
    let mut mcp = Mcp::spawn("http://127.0.0.1:1", "bz1.not-a-real-token");
    mcp.start().await;

    // No core on the other end, so this fails at the transport — which
    // is the proof that it was not refused here first.
    let err = mcp
        .tool_err("deny_pairing", json!({"id": "00000000-0000-0000-0000-000000000000"}))
        .await;
    assert!(err.contains("transport"), "denying reached the network: {err}");
}

// ---------------------------------------------------------------------
// A real core on the other end.
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_mcp_server_drives_a_erisdb() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn(&core.url, &root);
    mcp.start().await;

    // Registering a facet is just a write to the meta-facet.
    mcp.tool(
        "create_item",
        json!({"facet": "facet", "body": {"name": "notes", "version": 1, "strict": false, "schema": {}}}),
    )
    .await;
    let facets = mcp.tool("list_facets", json!({})).await;
    assert!(
        facets["items"].as_array().unwrap().iter().any(|i| i["body"]["name"] == "notes"),
        "{facets}"
    );

    // Create and read back.
    let created = mcp
        .tool("create_item", json!({"facet": "notes", "body": {"title": "groceries", "text": "buy oat milk"}}))
        .await;
    let id = created["id"].as_str().unwrap().to_string();
    let items = mcp.tool("read_items", json!({"facet": "notes"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 1);

    // Search without naming a facet sweeps every registered one,
    // case-insensitively.
    let hits = mcp.tool("search_items", json!({"query": "OAT MILK"})).await;
    assert!(hits["items"].as_array().unwrap().iter().any(|i| i["id"] == id.as_str()), "{hits}");
    let misses = mcp.tool("search_items", json!({"query": "no such thing anywhere"})).await;
    assert_eq!(misses["items"].as_array().unwrap().len(), 0);

    // An update carries the revision it is based on.
    let revision = created["revision"].as_i64().unwrap();
    mcp.tool(
        "update_item",
        json!({
            "id": id,
            "revision": revision,
            "body": {"title": "groceries", "text": "buy oat milk and bread"}
        }),
    )
    .await;
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert!(item["body"]["text"].as_str().unwrap().contains("bread"));

    // The same revision a second time is a conflict, not a clobber: two
    // writers who both read the old state cannot both win.
    let stale = mcp
        .tool_err(
            "update_item",
            json!({"id": id, "revision": revision, "body": {"title": "groceries", "text": "no"}}),
        )
        .await;
    assert!(stale.contains("409"), "{stale}");
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert!(item["body"]["text"].as_str().unwrap().contains("bread"), "the loser changed nothing");

    // Two states so far; revert to the first, which lands as a third.
    let hist = mcp.tool("item_history", json!({"id": id})).await;
    let states = hist["history"].as_array().unwrap();
    assert_eq!(states.len(), 2);
    let first_seq = states[0]["seq"].as_i64().unwrap();
    let current = item["revision"].as_i64().unwrap();
    mcp.tool("revert_item", json!({"id": id, "seq": first_seq, "revision": current})).await;
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert_eq!(item["body"]["text"], "buy oat milk");
    assert_eq!(item["revision"], 3);

    // The change feed saw everything, and pages by cursor.
    let changes = mcp.tool("read_changes", json!({"since": 0})).await;
    assert!(changes["changes"].as_array().unwrap().len() >= 4, "{changes}");
    assert!(changes["next"].as_i64().unwrap() > 0);

    // Delete; the item is gone but its history is not.
    mcp.tool("delete_item", json!({"id": id, "confirm": true})).await;
    let items = mcp.tool("read_items", json!({"facet": "notes"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);
    let hist = mcp.tool("item_history", json!({"id": id})).await;
    assert_eq!(hist["history"].as_array().unwrap().len(), 4);

    // Failures are tool errors, not crashes: the loop keeps serving.
    let err = mcp
        .tool_err("get_item", json!({"id": "00000000-0000-0000-0000-000000000000"}))
        .await;
    assert!(err.contains("404"), "{err}");
    let items = mcp.tool("read_items", json!({"facet": "notes"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);
}

/// `confirm: false` is a dry run: it shows what would go and leaves it
/// there. A model looping over a facet has to say so, item by item.
#[tokio::test]
async fn deleting_takes_confirmation_and_offers_a_dry_run() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn(&core.url, &root);
    mcp.start().await;

    mcp.tool(
        "create_item",
        json!({"facet": "facet", "body": {"name": "notes", "version": 1, "strict": false, "schema": {}}}),
    )
    .await;
    let created =
        mcp.tool("create_item", json!({"facet": "notes", "body": {"text": "keep me"}})).await;
    let id = created["id"].as_str().unwrap().to_string();

    let preview = mcp.tool("delete_item", json!({"id": id, "confirm": false})).await;
    assert_eq!(preview["deleted"], false, "{preview}");
    assert_eq!(preview["item"]["body"]["text"], "keep me", "{preview}");
    let items = mcp.tool("read_items", json!({"facet": "notes"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 1, "the dry run changed nothing");

    mcp.tool("delete_item", json!({"id": id, "confirm": true})).await;
    let items = mcp.tool("read_items", json!({"facet": "notes"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);
}

/// Search finds an item by its id however the id was typed, and honours a
/// ceiling on how much work one call can ask for.
#[tokio::test]
async fn search_matches_ids_case_insensitively_and_stays_bounded() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn(&core.url, &root);
    mcp.start().await;

    mcp.tool(
        "create_item",
        json!({"facet": "facet", "body": {"name": "notes", "version": 1, "strict": false, "schema": {}}}),
    )
    .await;
    for n in 0..5 {
        mcp.tool(
            "create_item",
            json!({"facet": "notes", "body": {"text": format!("zebra {n}")}}),
        )
        .await;
    }
    let created =
        mcp.tool("create_item", json!({"facet": "notes", "body": {"text": "findable"}})).await;
    let id = created["id"].as_str().unwrap().to_string();

    // A UUID is hex: the same id typed either way is the same id.
    for typed in [id.to_lowercase(), id.to_uppercase()] {
        let hits = mcp.tool("search_items", json!({"query": typed})).await;
        let found: Vec<&Value> = hits["items"].as_array().unwrap().iter().collect();
        assert_eq!(found.len(), 1, "{hits}");
        assert_eq!(found[0]["id"], id.as_str());
    }

    // An unbounded limit is not on offer, and the answer says the sweep
    // stopped early rather than pretending it was complete.
    let hits = mcp.tool("search_items", json!({"query": "zebra", "limit": 2})).await;
    assert_eq!(hits["items"].as_array().unwrap().len(), 2, "{hits}");
    assert_eq!(hits["truncated"], true, "{hits}");

    let hits = mcp.tool("search_items", json!({"query": "zebra", "limit": 100_000})).await;
    assert_eq!(hits["items"].as_array().unwrap().len(), 5, "{hits}");
    assert_eq!(hits["truncated"], false, "{hits}");
}

/// The token can come from a file instead of the environment, so it is
/// not sitting in an MCP config in cleartext and not readable out of the
/// process environment by everything else on the machine.
#[tokio::test]
async fn the_token_can_come_from_a_file() {
    let core = real_core!();
    let token = core.mint("meta:facets:read", 3600);
    let path = std::env::temp_dir().join(format!("erisdb-mcp-token-{}", std::process::id()));
    std::fs::write(&path, format!("{token}\n")).expect("write token file");

    let mut mcp = Mcp::spawn_with(&core.url, &[("ERISDB_TOKEN_FILE", path.to_str().unwrap())]);
    mcp.start().await;
    let facets = mcp.tool("list_facets", json!({})).await;
    assert!(facets["items"].is_array(), "{facets}");

    std::fs::remove_file(&path).ok();
}

/// Switched on, minting works — and still refuses a lifetime longer than
/// the operator's ceiling, whatever the model asks for.
#[tokio::test]
async fn minting_when_allowed_is_capped() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn_with(
        &core.url,
        &[("ERISDB_TOKEN", root.as_str()), ("ERISDB_MCP_ALLOW_MINT", "1"), ("ERISDB_MCP_MAX_MINT_TTL", "60")],
    );
    mcp.start().await;

    let minted =
        mcp.tool("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 30})).await;
    let token = minted["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("bz1."), "{minted}");
    assert_eq!(minted["ttl_secs"], 30, "{minted}");

    // The minted token holds what was asked for and nothing more.
    let held = core.api(reqwest::Method::GET, "/v1/permissions", &token, Value::Null).await;
    assert_eq!(held["grants"], json!(["notes:read"]), "{held}");

    // Asking for a year gets the ceiling, and the answer says so.
    let capped = mcp
        .tool("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 31_536_000}))
        .await;
    assert_eq!(capped["ttl_secs"], 60, "{capped}");

    // A lifetime of nothing is not a lifetime.
    let err =
        mcp.tool_err("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 0})).await;
    assert!(err.contains("ttl_secs"), "{err}");

    // Nobody hands out what they do not hold. The core enforces it; the
    // tool reports the refusal rather than swallowing it.
    let narrow = core.mint("notes:read", 3600);
    let mut narrow_mcp = Mcp::spawn_with(
        &core.url,
        &[("ERISDB_TOKEN", narrow.as_str()), ("ERISDB_MCP_ALLOW_MINT", "1")],
    );
    narrow_mcp.start().await;
    let err = narrow_mcp
        .tool_err("mint_capability", json!({"grants": ["notes:delete"], "ttl_secs": 30}))
        .await;
    assert!(err.contains("403"), "{err}");
}

// ---------------------------------------------------------------------
// The dashboard surface: what the store is, who is asking to pair, and
// the answer — which an agent gives only when the operator said it may.
// ---------------------------------------------------------------------

/// Reading server state and one's own permissions: the two questions an
/// agent should ask before it does anything else.
#[tokio::test]
async fn the_agent_can_read_the_server_and_its_own_grants() {
    let core = real_core!();
    let mut mcp = Mcp::spawn(&core.url, &core.mint("meta:server:read,notes:*", 3600));
    mcp.start().await;

    let state = mcp.tool("server_state", json!({})).await;
    assert!(state["version"].is_string(), "{state}");
    assert!(state["feed_head"].is_i64(), "{state}");
    assert!(state["limits"]["streams"].is_i64(), "{state}");

    // Asking what you hold needs no permission at all: the answer is
    // already inside the token doing the asking.
    let held = mcp.tool("my_permissions", json!({})).await;
    assert_eq!(held["grants"], json!(["meta:server:read", "notes:*"]), "{held}");
    assert_eq!(held["user"], "clod", "{held}");

    // And the grants are the bound: this token cannot read pairings.
    let err = mcp.tool_err("list_pairings", json!({})).await;
    assert!(err.contains("403"), "{err}");
}

/// The whole pairing conversation with an agent in the operator's seat:
/// a client asks, the agent reads the request, approves a subset, and
/// the client collects exactly that.
#[tokio::test]
async fn an_allowed_agent_answers_a_pairing_request() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn_with(
        &core.url,
        &[("ERISDB_TOKEN", root.as_str()), ("ERISDB_MCP_ALLOW_APPROVE", "1")],
    );
    mcp.start().await;

    let (id, code) = core.cut_a_pairing(&root).await;
    core.redeem(&code, "Tasks (Android) v0.3", &["tasks:read", "tasks:create", "tasks:delete"])
        .await;

    // The request is visible, with who asked and what for — and never
    // with a token in it, whatever the session holds.
    let pairings = mcp.tool("list_pairings", json!({})).await;
    let session = pairings["pairings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id.as_str())
        .unwrap_or_else(|| panic!("{pairings}"));
    assert_eq!(session["body"]["status"], "requested", "{session}");
    assert_eq!(session["body"]["client"], "Tasks (Android) v0.3", "{session}");
    assert!(session["body"].get("token").is_none(), "{session}");

    let one = mcp.tool("get_pairing", json!({"id": id})).await;
    assert_eq!(one["body"]["requested"], json!(["tasks:read", "tasks:create", "tasks:delete"]));

    // Approve two of the three. Nothing says the answer has to be the
    // question.
    let approved = mcp
        .tool(
            "approve_pairing",
            json!({"id": id, "granted": ["tasks:read", "tasks:create"], "ttl_secs": 3600}),
        )
        .await;
    assert_eq!(approved["body"]["status"], "approved", "{approved}");
    assert_eq!(approved["body"]["granted"], json!(["tasks:read", "tasks:create"]));
    // The token belongs to the client that asked, and to nobody else —
    // least of all to a transcript.
    assert!(approved["body"].get("token").is_none(), "{approved}");

    // The client collects it, and it is exactly what was approved.
    let collected = core.collect(&code).await;
    let token = collected["token"].as_str().expect("a token").to_string();
    let held = core.api(reqwest::Method::GET, "/v1/permissions", &token, Value::Null).await;
    assert_eq!(held["grants"], json!(["tasks:read", "tasks:create"]), "{held}");
}

/// A master key is a keystroke a human makes in front of the pairing
/// screen, not something an agent hands out on request.
#[tokio::test]
async fn an_agent_cannot_approve_a_master_key() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn_with(
        &core.url,
        &[("ERISDB_TOKEN", root.as_str()), ("ERISDB_MCP_ALLOW_APPROVE", "1")],
    );
    mcp.start().await;

    let (id, code) = core.cut_a_pairing(&root).await;
    core.redeem(&code, "Greedy v0", &["*"]).await;

    let err = mcp.tool_err("approve_pairing", json!({"id": id, "granted": ["*"]})).await;
    assert!(err.contains("erisdb mint"), "the refusal says where that is done: {err}");

    // The session is untouched, so a human can still answer it.
    let one = mcp.tool("get_pairing", json!({"id": id})).await;
    assert_eq!(one["body"]["status"], "requested", "{one}");
}

/// An approval's lifetime is bounded by the operator's ceiling too: an
/// approved token is as much a credential as a minted one, and the core
/// would otherwise hand out its own week-long default.
#[tokio::test]
async fn an_approval_is_capped_like_a_mint() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn_with(
        &core.url,
        &[
            ("ERISDB_TOKEN", root.as_str()),
            ("ERISDB_MCP_ALLOW_APPROVE", "1"),
            ("ERISDB_MCP_MAX_MINT_TTL", "60"),
        ],
    );
    mcp.start().await;

    // Asking for a year gets the ceiling — and so does saying nothing,
    // which would otherwise take the core's own week-long default.
    for (n, ttl) in [("Patient v0", Some(31_536_000)), ("Quiet v0", None)] {
        let (id, code) = core.cut_a_pairing(&root).await;
        core.redeem(&code, n, &["tasks:read"]).await;
        let mut args = json!({"id": id, "granted": ["tasks:read"]});
        if let Some(t) = ttl {
            args["ttl_secs"] = json!(t);
        }
        let approved = mcp.tool("approve_pairing", args).await;
        assert_eq!(approved["ttl_secs"], 60, "{approved}");

        let token = core.collect(&code).await["token"].as_str().unwrap().to_string();
        let held = core.api(reqwest::Method::GET, "/v1/permissions", &token, Value::Null).await;
        let life = held["exp"].as_i64().unwrap() - now_secs();
        assert!(life <= 60, "the ceiling did not hold for {n}: {life}s");
    }
}

/// Denying takes authority away, so it needs no switch — and it leaves
/// the client with nothing at all.
#[tokio::test]
async fn an_agent_can_always_deny() {
    let core = real_core!();
    let root = core.mint("*", 3600);
    let mut mcp = Mcp::spawn(&core.url, &root);
    mcp.start().await;

    let (id, code) = core.cut_a_pairing(&root).await;
    core.redeem(&code, "Unwanted v0", &["tasks:read"]).await;

    let denied = mcp.tool("deny_pairing", json!({"id": id})).await;
    assert_eq!(denied["body"]["status"], "denied", "{denied}");

    let collected = core.collect(&code).await;
    assert_eq!(collected["status"], "denied", "{collected}");
    assert!(collected.get("token").is_none(), "{collected}");
}

/// Unix seconds, without a date crate: the assertion only needs a clock.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs() as i64
}

#[tokio::test]
async fn paired_mcp_renews_after_restart_and_stops_when_revoked() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let core = real_core!();
    let admin = core.mint("*", 3600);
    let (id, code) = core.cut_a_pairing(&admin).await;
    let ticket = format!("bezel://pair/{}", URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({"v": 1, "url": core.url, "token": code})).unwrap()));
    let path = std::env::temp_dir().join(format!("erisdb-mcp-{id}.json"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_erisdb-mcp"))
        .args(["pair", &ticket, "--session-file"]).arg(&path)
        .args(["--grant", "tasks:read", "--name", "real MCP installation"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let comparison = tokio::time::timeout(Duration::from_secs(10), stderr.next_line())
        .await.unwrap().unwrap().unwrap();
    let pending = core.api(reqwest::Method::GET, &format!("/v1/pairings/{id}"), &admin, Value::Null).await;
    assert!(comparison.contains(pending["body"]["fingerprint"].as_str().unwrap()));
    core.api(reqwest::Method::POST, &format!("/v1/pairings/{id}/approve"), &admin,
        json!({"granted": ["tasks:read"], "ttl_secs": 1})).await;
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.unwrap().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "credentials must not reach stdout");
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["client_id"], id);
    assert_eq!(saved["refresh_secret"].as_str().unwrap().len(), 43);
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    for _ in 0..2 {
        let mut mcp = Mcp::spawn_with(&core.url, &[("ERISDB_SESSION_FILE", path.to_str().unwrap())]);
        mcp.start().await;
        let permissions = mcp.tool("my_permissions", json!({})).await;
        assert_eq!(permissions["client_id"], id);
        assert_eq!(permissions["grants"], json!(["tasks:read"]));
    }
    core.api(reqwest::Method::POST, &format!("/v1/clients/{id}/revoke"), &admin, json!({})).await;
    let mut mcp = Mcp::spawn_with(&core.url, &[("ERISDB_SESSION_FILE", path.to_str().unwrap())]);
    mcp.start().await;
    assert!(mcp.tool_err("my_permissions", json!({})).await.contains("401"));
    std::fs::remove_file(path).unwrap();
}
