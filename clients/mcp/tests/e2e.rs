//! The MCP client interface, proven end to end: real Postgres (Docker via
//! testcontainers), a real erisdb binary joined to a mesh and serving over
//! Iroh, a device node built from the mesh library in this process, and the
//! erisdb-mcp binary as a real subprocess speaking JSON-RPC over its stdio.
//! eon is a stand-in on its agent socket that does what eon does once the
//! person says yes: it signs the note and enrolls it at the mesh's database
//! from the device's endpoint, over the mesh library's `enroll`. Every
//! endpoint is on loopback.
//!
//! The core is driven as a process and built from the server crate when
//! needed. Missing prerequisites fail the suite rather than silently skipping.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use erisdb_client::CAPABILITY;
use erisoverlaynetwork::agent::{self, Ask, Reply, Request};
use erisoverlaynetwork::devices::Op;
use erisoverlaynetwork::enroll::{self, Outcome};
use erisoverlaynetwork::mesh::{self, Config, Local, MemoryStore, Node};
use erisoverlaynetwork::net::{self, Network};
use erisoverlaynetwork::Uses;
use iroh::protocol::Router;
use iroh::SecretKey;
use serde_json::{json, Value};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const PASSPHRASE: &str = "correct horse battery staple";

/// The erisdb binary to run a core with: `ERISDB_BIN` if the operator named
/// one, else `../../erisdb` built once for the run.
fn erisdb_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            if let Ok(path) = std::env::var("ERISDB_BIN") {
                return PathBuf::from(path);
            }
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../erisdb/Cargo.toml");
            let built = std::process::Command::new(env!("CARGO"))
                .args(["build", "--bin", "erisdb", "--manifest-path"])
                .arg(&manifest)
                .status()
                .expect("build the erisdb binary");
            assert!(built.success(), "a real ErisDB binary is required; build ../../erisdb or set ERISDB_BIN");
            manifest.with_file_name("target/debug/erisdb")
        })
        .clone()
}

// ---------------------------------------------------------------------
// The mesh: a device, its eon, and the database it let in.
// ---------------------------------------------------------------------

/// What the person at the stand-in eon does with a request: approve these
/// grants in the database, or say no.
type Person = Arc<dyn Fn(&Ask) -> Option<Vec<String>> + Send + Sync>;

fn approves_as_asked() -> Person {
    Arc::new(|ask: &Ask| Some(ask.uses.get(CAPABILITY).cloned().unwrap_or_default()))
}

/// Using the database with `grants`.
fn database(grants: Vec<String>) -> Uses {
    Uses::from([(CAPABILITY.parse().unwrap(), grants)])
}

/// eon's agent socket on the device, under a runtime directory of its own:
/// an app approved to serve a capability a mesh has a single server of
/// replaces the server of it before it, whose note is revoked; an app asking
/// to use the database is enrolled there, and approved only once the
/// database took it.
struct Agent {
    runtime: tempfile::TempDir,
    asked: Arc<std::sync::Mutex<Vec<Ask>>>,
}

impl Agent {
    async fn on(node: &Node, person: Person) -> Agent {
        let runtime = tempfile::tempdir().unwrap();
        let path = agent::socket_in(runtime.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (node, seen) = (node.clone(), asked.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (node, person, seen) = (node.clone(), person.clone(), seen.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    BufReader::new(read).read_line(&mut line).await.unwrap();
                    let reply = match serde_json::from_str::<Request>(&line).unwrap() {
                        Request::Approve { ask, .. } => {
                            seen.lock().unwrap().push(ask.clone());
                            answer(&node, &ask, person(&ask)).await
                        }
                        Request::Snapshot { .. } => Reply::Snapshot(node.snapshot().await.unwrap()),
                    };
                    let mut out = serde_json::to_vec(&reply).unwrap();
                    out.push(b'\n');
                    let _ = write.write_all(&out).await;
                });
            }
        });
        Agent { runtime, asked }
    }
}

async fn answer(node: &Node, ask: &Ask, granted: Option<Vec<String>>) -> Reply {
    let Some(grants) = granted else { return Reply::Denied("the person said no".into()) };
    if ask.serves.iter().any(|s| s.single) {
        let note = node.issue(ask.app, &ask.name, ask.uses.clone(), ask.serves.clone(), None).unwrap();
        let before = node.servers(CAPABILITY).into_iter().filter_map(|p| p.note().map(|n| n.id));
        for old in before.filter(|old| *old != note.id).collect::<Vec<_>>() {
            node.append(Op::Revoke { note: old }).await.unwrap();
        }
        return Reply::Approved { note, snapshot: node.snapshot().await.unwrap() };
    }
    let note = node.issue(ask.app, &ask.name, database(grants), Vec::new(), None).unwrap();
    let server = node.server(CAPABILITY).expect("the mesh has a database");
    match enroll::enroll(node, server.id, &note, PASSPHRASE).await {
        Ok(Outcome::Done) => {}
        other => return Reply::Error(format!("the database did not take the note: {other:?}")),
    }
    Reply::Approved { note, snapshot: node.snapshot().await.unwrap() }
}

/// A mesh with a database in it: Postgres, the device and its eon, and the
/// core joined as the database, its passphrase set, serving TCP and Iroh.
struct Core {
    _pg: ContainerAsync<Postgres>,
    _server: Child,
    secret: String,
    url: String,
    laptop: Node,
    _router: Router,
    eon: Agent,
}

impl Core {
    async fn new() -> Core {
        let pg = Postgres::default().start().await.expect("start postgres");
        let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let database = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");
        // A deployment of its own: its own secret, its own key.
        let secret = format!("mcp-e2e-{}", SecretKey::generate().public());

        let key = SecretKey::generate();
        let endpoint = net::endpoint(key.clone(), &Network::Loopback).await.unwrap();
        let laptop = Node::start(Config {
            endpoint: endpoint.clone(),
            network: Network::Loopback,
            local: Local::Device(key),
            store: Arc::new(MemoryStore::default()),
        })
        .await
        .unwrap();
        laptop.create("laptop").await.unwrap();
        let router = Router::builder(endpoint).accept(mesh::ALPN, laptop.protocol()).spawn();
        let eon = Agent::on(&laptop, approves_as_asked()).await;

        let joined = erisdb(&secret, &database, eon.runtime.path()).arg("join").output().await.unwrap();
        assert!(joined.status.success(), "join: {}", String::from_utf8_lossy(&joined.stderr));
        let mut set = erisdb(&secret, &database, eon.runtime.path())
            .args(["passphrase", "set"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        set.stdin.take().unwrap().write_all(format!("{PASSPHRASE}\n").as_bytes()).await.unwrap();
        assert!(set.wait().await.unwrap().success());

        // Claim a port by binding it and letting go: the core takes it next.
        let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let server = erisdb(&secret, &database, eon.runtime.path())
            .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn erisdb serve");
        let http = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !http.get(format!("{url}/v1/health")).send().await.is_ok_and(|r| r.status() == 200)
            || laptop.server(CAPABILITY).is_none()
        {
            assert!(tokio::time::Instant::now() < deadline, "the core never came up on {url}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Core { _pg: pg, _server: server, secret, url, laptop, _router: router, eon }
    }

    /// Cut a token with the same CLI an operator would use. `grants` is
    /// what `--grant` takes: permissions, comma-separated.
    fn mint(&self, grants: &str, ttl: i64) -> String {
        let out = std::process::Command::new(erisdb_binary())
            .args(["mint", "--grant", grants, "--ttl", &ttl.to_string(), "--user", "clod", "--secret", &self.secret])
            .output()
            .expect("run erisdb mint");
        assert!(out.status.success(), "mint failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    async fn api(&self, method: reqwest::Method, path: &str, token: &str, body: Value) -> Value {
        let mut req = reqwest::Client::new().request(method.clone(), format!("{}{path}", self.url)).bearer_auth(token);
        if method != reqwest::Method::GET {
            req = req.json(&body);
        }
        let resp = req.send().await.expect("reach the core");
        let status = resp.status();
        let text = resp.text().await.expect("read the body");
        assert!(status.is_success(), "{method} {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// `erisdb-mcp sign-in` against this mesh's eon, keeping its profile
    /// under `config`.
    fn sign_in(&self, config: &Path, args: &[&str]) -> Command {
        self.sign_in_through(&self.eon, config, args)
    }

    fn sign_in_through(&self, eon: &Agent, config: &Path, args: &[&str]) -> Command {
        let mut command = mcp_command(config);
        command.env("XDG_RUNTIME_DIR", eon.runtime.path()).arg("sign-in").args(args);
        command
    }

    /// An MCP signed in under `config` with `grants`, started and handshaken.
    async fn mcp(&self, config: &Path, grants: &str, env: &[(&str, &str)]) -> Mcp {
        let out = self.sign_in(config, &["--grant", grants]).output().await.unwrap();
        assert!(out.status.success(), "sign-in: {}", String::from_utf8_lossy(&out.stderr));
        let mut mcp = Mcp::spawn(config, env);
        mcp.start().await;
        mcp
    }
}

fn erisdb(secret: &str, database: &str, runtime: &Path) -> Command {
    let mut command = Command::new(erisdb_binary());
    command
        .env_clear()
        .env("DATABASE_URL", database)
        .env("ERISDB_SECRET", secret)
        .env("ERIS_NETWORK", "loopback")
        .env("XDG_RUNTIME_DIR", runtime)
        .kill_on_drop(true);
    command
}

/// The erisdb-mcp binary with its configuration under `config`, on loopback.
fn mcp_command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_erisdb-mcp"));
    for key in ["ERISDB_PROFILE_FILE", "ERISDB_PROFILE", "XDG_RUNTIME_DIR"] {
        command.env_remove(key);
    }
    command.env("XDG_CONFIG_HOME", config).env("ERIS_NETWORK", "loopback").kill_on_drop(true);
    command
}

fn profile(config: &Path, name: &str) -> PathBuf {
    config.join("erisdb/mcp").join(format!("{name}.json"))
}

/// A profile for an app no ErisDB knows, at an address nobody answers:
/// enough to run the server and every guard that refuses before the network.
fn offline(config: &Path) {
    let path = profile(config, "default");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let key: String = SecretKey::generate().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let server = iroh::EndpointAddr::from(SecretKey::generate().public());
    std::fs::write(&path, serde_json::to_vec(&json!({"key": key, "server": server})).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

// ---------------------------------------------------------------------
// The erisdb-mcp binary under test, driven over its stdio.
// ---------------------------------------------------------------------

struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: i64,
}

impl Mcp {
    fn spawn(config: &Path, env: &[(&str, &str)]) -> Self {
        let mut command = mcp_command(config);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("spawn erisdb-mcp");
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
            let line = tokio::time::timeout(Duration::from_secs(60), self.stdout.next_line())
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
    tools["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("no tool {name}"))
}

fn requires(tools: &Value, name: &str, field: &str) -> bool {
    schema_of(tools, name)["inputSchema"]["required"].as_array().into_iter().flatten().any(|r| r == field)
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
    let config = tempfile::tempdir().unwrap();
    offline(config.path());
    let mut mcp = Mcp::spawn(config.path(), &[]);
    let tools = mcp.start().await;

    let names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    let mut expected = vec![
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
    ];
    let mut have = names.clone();
    expected.sort();
    have.sort();
    assert_eq!(have, expected);
    for t in tools["tools"].as_array().unwrap() {
        assert!(t["inputSchema"]["type"] == "object", "tool {} lacks a schema", t["name"]);
    }

    // Omitting a revision would mean "fetch whatever is current and
    // overwrite it", which turns a partial view into silent data loss.
    assert!(requires(&tools, "update_item", "revision"), "update_item must demand a revision");
    assert!(requires(&tools, "revert_item", "revision"), "revert_item must demand a revision");
    // Deleting is not something to arrive at by omission.
    assert!(requires(&tools, "delete_item", "confirm"), "delete_item must demand confirmation");
    // A token with no expiry is forever; asking for one has to be deliberate.
    assert!(requires(&tools, "mint_capability", "ttl_secs"), "mint_capability must demand a ttl");
    assert!(requires(&tools, "mint_capability", "grants"), "mint_capability mints grants");

    // With nobody at the saved address, a tool says so and the session goes on.
    let err = mcp.tool_err("my_permissions", json!({})).await;
    assert!(err.contains("transport"), "{err}");
    assert!(mcp.tool_err("list_facets", json!({})).await.contains("transport"));
}

/// An update with no revision never reaches the core: it is rejected at
/// the schema, where the model can see what it left out.
#[tokio::test]
async fn a_write_without_a_revision_is_rejected() {
    let config = tempfile::tempdir().unwrap();
    offline(config.path());
    let mut mcp = Mcp::spawn(config.path(), &[]);
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
    let config = tempfile::tempdir().unwrap();
    offline(config.path());
    let mut mcp = Mcp::spawn(config.path(), &[]);
    mcp.start().await;
    let err = mcp.tool_err("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 60})).await;
    assert!(err.contains("ERISDB_MCP_ALLOW_MINT"), "the refusal names its switch: {err}");
}

/// A profile is credentials: readable by others, the server refuses to use it.
#[cfg(unix)]
#[tokio::test]
async fn a_profile_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let config = tempfile::tempdir().unwrap();
    offline(config.path());
    let path = profile(config.path(), "default");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = mcp_command(config.path()).stdin(Stdio::null()).output().await.unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("chmod 600"), "{}", String::from_utf8_lossy(&out.stderr));

    let empty = tempfile::tempdir().unwrap();
    let out = mcp_command(empty.path()).stdin(Stdio::null()).output().await.unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("erisdb-mcp sign-in"), "{}", String::from_utf8_lossy(&out.stderr));
}

// ---------------------------------------------------------------------
// Signing in, through the device's eon.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn signing_in_through_eon_keeps_a_private_profile() {
    let core = Core::new().await;
    let config = tempfile::tempdir().unwrap();
    let path = profile(config.path(), "default");

    // No eon listening: nothing is asked, nothing is written.
    let nowhere = tempfile::tempdir().unwrap();
    let out = mcp_command(config.path())
        .env("XDG_RUNTIME_DIR", nowhere.path())
        .args(["sign-in", "--grant", "tasks:read"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no eon"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!path.exists());

    // The person approves less than was asked.
    let narrowing = Agent::on(&core.laptop, Arc::new(|_: &Ask| Some(vec!["tasks:read".to_string()]))).await;
    let out = core.sign_in_through(&narrowing, config.path(), &["--grant", "tasks:read,tasks:create"]).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "credentials must not reach stdout");
    match narrowing.asked.lock().unwrap().as_slice() {
        [ask] => {
            assert_eq!(ask.name, "erisdb-mcp (default)");
            assert_eq!(ask.uses, database(vec!["tasks:read".into(), "tasks:create".into()]), "it asks to use a database");
            assert!(ask.serves.is_empty(), "it serves nothing: {ask:?}");
        }
        other => panic!("eon was asked {other:?}"),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["key"].as_str().unwrap().len(), 64);
    assert_eq!(saved["server"]["id"], core.laptop.server(CAPABILITY).unwrap().id.to_string());

    // Restarting needs nothing but the profile.
    let mut mcp = Mcp::spawn(config.path(), &[]);
    mcp.start().await;
    let held = mcp.tool("my_permissions", json!({})).await;
    assert_eq!(held["grants"], json!(["tasks:read"]), "{held}");
    let client = held["client_id"].as_str().unwrap().to_string();

    // A denial leaves the saved profile as it was.
    let declining = Agent::on(&core.laptop, Arc::new(|_: &Ask| None)).await;
    let before = std::fs::read(&path).unwrap();
    let out = core.sign_in_through(&declining, config.path(), &["--grant", "tasks:*"]).output().await.unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("said no"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    // Signing in again keeps the key, so the installation is the same one,
    // with what the person approved this time.
    let out = core.sign_in(config.path(), &["--grant", "tasks:read,tasks:create"]).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let again: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(again["key"], saved["key"]);
    let held = mcp.tool("my_permissions", json!({})).await;
    assert_eq!((held["client_id"].as_str(), &held["grants"]), (Some(client.as_str()), &json!(["tasks:read", "tasks:create"])));

    // Another profile is another app, with a key and an installation of its own.
    let out = core.sign_in(config.path(), &["--profile", "chat", "--grant", "notes:read"]).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let other: Value = serde_json::from_slice(&std::fs::read(profile(config.path(), "chat")).unwrap()).unwrap();
    assert_ne!(other["key"], saved["key"]);
    let mut chat = Mcp::spawn(config.path(), &[("ERISDB_PROFILE", "chat")]);
    chat.start().await;
    assert_eq!(chat.tool("my_permissions", json!({})).await["grants"], json!(["notes:read"]));

    // The operator revokes one; it hears 401, and the other carries on.
    let admin = core.mint("*", 3600);
    core.api(reqwest::Method::POST, &format!("/v1/clients/{client}/revoke"), &admin, json!({})).await;
    assert!(mcp.tool_err("my_permissions", json!({})).await.contains("401"));
    assert_eq!(chat.tool("my_permissions", json!({})).await["grants"], json!(["notes:read"]));
}

// ---------------------------------------------------------------------
// A real core on the other end.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_server_drives_a_erisdb() {
    let core = Core::new().await;
    let config = tempfile::tempdir().unwrap();
    let mut mcp = core.mcp(config.path(), "notes:*,meta:facets:read,meta:feed:read", &[]).await;

    // Registering a facet is just a write to the meta-facet; the app's own
    // namespace's create grant lets it initialize the missing schema.
    mcp.tool("create_item", json!({"facet": "facet", "body": {"name": "notes", "version": 1, "strict": false, "schema": {}}}))
        .await;
    let facets = mcp.tool("list_facets", json!({})).await;
    assert!(facets["items"].as_array().unwrap().iter().any(|i| i["body"]["name"] == "notes"), "{facets}");

    // Create and read back; the write says which installation made it.
    let created =
        mcp.tool("create_item", json!({"facet": "notes", "body": {"title": "groceries", "text": "buy oat milk"}})).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["source"]["client"], "erisdb-mcp");
    assert!(created["source"]["installation"].is_string(), "{created}");
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
        json!({"id": id, "revision": revision, "body": {"title": "groceries", "text": "buy oat milk and bread"}}),
    )
    .await;
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert!(item["body"]["text"].as_str().unwrap().contains("bread"));

    // The same revision a second time is a conflict, not a clobber.
    let stale = mcp
        .tool_err("update_item", json!({"id": id, "revision": revision, "body": {"title": "groceries", "text": "no"}}))
        .await;
    assert!(stale.contains("409"), "{stale}");

    // Two states so far; revert to the first, which lands as a third.
    let hist = mcp.tool("item_history", json!({"id": id})).await;
    let states = hist["history"].as_array().unwrap();
    assert_eq!(states.len(), 2);
    let first_seq = states[0]["seq"].as_i64().unwrap();
    let current = item["revision"].as_i64().unwrap();
    mcp.tool("revert_item", json!({"id": id, "seq": first_seq, "revision": current})).await;
    let item = mcp.tool("get_item", json!({"id": id})).await;
    assert_eq!((&item["body"]["text"], &item["revision"]), (&json!("buy oat milk"), &json!(3)));

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
    let err = mcp.tool_err("get_item", json!({"id": "00000000-0000-0000-0000-000000000000"})).await;
    assert!(err.contains("404"), "{err}");
    let err = mcp.tool_err("server_state", json!({})).await;
    assert!(err.contains("403"), "the grants are the bound: {err}");
    let items = mcp.tool("read_items", json!({"facet": "notes"})).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);
}

/// `confirm: false` is a dry run: it shows what would go and leaves it
/// there. A model looping over a facet has to say so, item by item.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_takes_confirmation_and_offers_a_dry_run() {
    let core = Core::new().await;
    let config = tempfile::tempdir().unwrap();
    let mut mcp = core.mcp(config.path(), "notes:*", &[]).await;

    mcp.tool("create_item", json!({"facet": "facet", "body": {"name": "notes", "version": 1, "strict": false, "schema": {}}}))
        .await;
    let created = mcp.tool("create_item", json!({"facet": "notes", "body": {"text": "keep me"}})).await;
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
#[tokio::test(flavor = "multi_thread")]
async fn search_matches_ids_case_insensitively_and_stays_bounded() {
    let core = Core::new().await;
    let config = tempfile::tempdir().unwrap();
    let mut mcp = core.mcp(config.path(), "notes:*,meta:facets:read", &[]).await;

    mcp.tool("create_item", json!({"facet": "facet", "body": {"name": "notes", "version": 1, "strict": false, "schema": {}}}))
        .await;
    for n in 0..5 {
        mcp.tool("create_item", json!({"facet": "notes", "body": {"text": format!("zebra {n}")}})).await;
    }
    let created = mcp.tool("create_item", json!({"facet": "notes", "body": {"text": "findable"}})).await;
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
    assert_eq!((hits["items"].as_array().unwrap().len(), &hits["truncated"]), (2, &json!(true)), "{hits}");
    let hits = mcp.tool("search_items", json!({"query": "zebra", "limit": 100_000})).await;
    assert_eq!((hits["items"].as_array().unwrap().len(), &hits["truncated"]), (5, &json!(false)), "{hits}");
}

/// Switched on, minting works — and still refuses a lifetime longer than
/// the operator's ceiling, whatever the model asks for.
#[tokio::test(flavor = "multi_thread")]
async fn minting_when_allowed_is_capped() {
    let core = Core::new().await;
    let config = tempfile::tempdir().unwrap();
    let allowed = [("ERISDB_MCP_ALLOW_MINT", "1"), ("ERISDB_MCP_MAX_MINT_TTL", "60")];
    let mut mcp = core.mcp(config.path(), "notes:read,meta:capabilities:mint", &allowed).await;

    let minted = mcp.tool("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 30})).await;
    let token = minted["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("erisdb1."), "{minted}");
    assert_eq!(minted["ttl_secs"], 30, "{minted}");

    // The minted token holds what was asked for and nothing more, and
    // belongs to the installation that minted it.
    let held = core.api(reqwest::Method::GET, "/v1/permissions", &token, Value::Null).await;
    assert_eq!(held["grants"], json!(["notes:read"]), "{held}");
    assert!(held["client_id"].is_string(), "{held}");

    // Asking for a year gets the ceiling, and the answer says so.
    let capped = mcp.tool("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 31_536_000})).await;
    assert_eq!(capped["ttl_secs"], 60, "{capped}");

    // A lifetime of nothing is not a lifetime.
    let err = mcp.tool_err("mint_capability", json!({"grants": ["notes:read"], "ttl_secs": 0})).await;
    assert!(err.contains("ttl_secs"), "{err}");

    // Nobody hands out what they do not hold. The core enforces it; the
    // tool reports the refusal rather than swallowing it.
    let err = mcp.tool_err("mint_capability", json!({"grants": ["notes:delete"], "ttl_secs": 30})).await;
    assert!(err.contains("403"), "{err}");
}

/// Reading server state and one's own permissions: the two questions an
/// agent should ask before it does anything else.
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_can_read_the_server_and_its_own_grants() {
    let core = Core::new().await;
    let config = tempfile::tempdir().unwrap();
    let mut mcp = core.mcp(config.path(), "meta:server:read,notes:*", &[]).await;

    let state = mcp.tool("server_state", json!({})).await;
    assert!(state["version"].is_string(), "{state}");
    assert!(state["feed_head"].is_i64(), "{state}");
    assert!(state["limits"]["streams"].is_i64(), "{state}");

    // Asking what you hold needs no permission at all.
    let held = mcp.tool("my_permissions", json!({})).await;
    assert_eq!(held["grants"], json!(["meta:server:read", "notes:*"]), "{held}");
    assert!(held["client_id"].is_string(), "{held}");

    // And the grants are the bound: this app cannot read the facet registrations.
    let err = mcp.tool_err("list_facets", json!({})).await;
    assert!(err.contains("403"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_search_reports_unscanned_items_and_authentication_failures() {
    let core = Core::new().await;
    let root = core.mint("*", 3600);
    core.api(
        reqwest::Method::POST,
        "/v1/items",
        &root,
        json!({"facet": "facet", "body": {"name": "searchbound", "strict": false, "schema": {}}}),
    )
    .await;
    let http = reqwest::Client::new();
    for index in 0..1001 {
        let response = http
            .post(format!("{}/v1/items", core.url))
            .bearer_auth(&root)
            .json(&json!({"facet": "searchbound", "body": {"index": index}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
    }
    let config = tempfile::tempdir().unwrap();
    let mut mcp = core.mcp(config.path(), "searchbound:read", &[]).await;
    let result = mcp.tool("search_items", json!({"facet": "searchbound", "query": "absent-marker"})).await;
    assert_eq!(result["items"], json!([]));
    assert_eq!(result["scanned_facets"], 1);
    assert_eq!(result["truncated"], true, "a full scan page cannot prove there are no later matches");

    // A key ErisDB never enrolled, at the right address: refused, not empty.
    let stranger = tempfile::tempdir().unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(profile(config.path(), "default")).unwrap()).unwrap();
    offline(stranger.path());
    let path = profile(stranger.path(), "default");
    let mut forged: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    forged["server"] = saved["server"].clone();
    std::fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
    let mut unauthorized = Mcp::spawn(stranger.path(), &[]);
    unauthorized.start().await;
    let error = unauthorized.tool_err("search_items", json!({"facet": "searchbound", "query": "index"})).await;
    assert!(error.contains("401"), "authentication failures must not become empty results: {error}");
}
