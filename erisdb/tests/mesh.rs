//! ErisDB in its owner's mesh, end to end.
//!
//! A real Postgres (Docker via testcontainers), the real erisdb binary, and
//! real device nodes built from the mesh library in this process, every one
//! of them on loopback Iroh endpoints (`ERIS_NETWORK=loopback`). eon is a
//! stand-in answering the agent socket the way eon does once the person says
//! yes: it signs notes with its device's node, and approving an app to serve
//! `database/1`, which a mesh has a single server of, revokes every other
//! server of it it approved, so the mesh has one database. Devices enroll
//! apps from their own endpoints over the mesh library's `eris-enroll/0`,
//! apps call with nothing but their keys, and devices leave the mesh the way
//! they do in life, by an entry another device writes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use erisoverlaynetwork::agent::{self, Reply, Request};
use erisoverlaynetwork::devices::Op;
use erisoverlaynetwork::enroll::{self, Outcome};
use erisoverlaynetwork::mesh::{self, Config, Local, MemoryStore, Node};
use erisoverlaynetwork::net::{self, Network};
use erisoverlaynetwork::{Error as Refused, Note, NoteId, Serves, Snapshot, Uses};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use iroh::endpoint::Connection;
use iroh::protocol::Router;
use iroh::{EndpointId, SecretKey};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::OnceCell;

const SECRET: &str = "mesh-e2e-secret";
const PASSPHRASE: &str = "correct horse battery staple";
/// What ErisDB serves, and apps use.
const DATABASE: &str = "database/1";
/// The API's ALPN: the database's.
const ALPN: &[u8] = b"eris/database/1";

// ---------------------------------------------------------------- the store

struct Pg {
    _container: ContainerAsync<Postgres>,
    base_url: String,
}

static PG: OnceCell<Pg> = OnceCell::const_new();

/// A fresh, empty database in the shared container: the core migrates it.
async fn database() -> String {
    let pg = PG
        .get_or_init(|| async {
            let container = Postgres::default()
                // One container, kept and used again by every run: a container held in a static is
                // never dropped, so one started afresh each run would outlive it.
                .with_reuse(ReuseDirective::Always)
                .start()
                .await
                .expect("start postgres");
            let port = container.get_host_port_ipv4(5432).await.expect("pg port");
            Pg {
                _container: container,
                base_url: format!("postgres://postgres:postgres@127.0.0.1:{port}"),
            }
        })
        .await;
    let name = format!("erisdb_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{}/postgres", pg.base_url))
        .await
        .unwrap();
    // The name is a uuid made just now.
    sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .execute(&admin)
        .await
        .unwrap();
    format!("{}/{name}", pg.base_url)
}

// ---------------------------------------------------------------- devices

/// A device as eon runs one: its key, its node, its router.
struct Device {
    name: String,
    node: Node,
    _router: Router,
}

impl Device {
    async fn new(name: &str) -> Device {
        let key = SecretKey::generate();
        let endpoint = net::endpoint(key.clone(), &Network::Loopback)
            .await
            .unwrap();
        let node = Node::start(Config {
            endpoint: endpoint.clone(),
            network: Network::Loopback,
            local: Local::Device(key),
            store: Arc::new(MemoryStore::default()),
        })
        .await
        .unwrap();
        let router = Router::builder(endpoint)
            .accept(mesh::ALPN, node.protocol())
            .spawn();
        Device {
            name: name.into(),
            node,
            _router: router,
        }
    }

    /// The first device of a new mesh.
    async fn founder(name: &str) -> Device {
        let device = Device::new(name).await;
        device.node.create(name).await.unwrap();
        device
    }

    /// A device this one adds, the way the ceremony ends: an `add` entry,
    /// and the newcomer taking the mesh in.
    async fn add(&self, name: &str) -> Device {
        let device = Device::new(name).await;
        self.node
            .append(Op::Add {
                device: device.id(),
                name: name.into(),
            })
            .await
            .unwrap();
        device
            .node
            .adopt(self.node.snapshot().await.unwrap())
            .await
            .unwrap();
        device
    }

    fn id(&self) -> EndpointId {
        self.node.id()
    }

    /// A note approving `app` to use the database with `grants`.
    fn approve(&self, app: EndpointId, grants: &[&str]) -> Note {
        self.issue(app, uses(DATABASE, grants), Vec::new())
    }

    fn issue(&self, app: EndpointId, uses: Uses, serves: Vec<Serves>) -> Note {
        self.node
            .issue(app, "Tasks", uses, serves, Some("/usr/bin/tasks".into()))
            .unwrap()
    }

    /// Enroll `note` at ErisDB with `passphrase`, as eon does once the
    /// person typed it: from the device's own endpoint, synced first.
    async fn enroll(
        &self,
        erisdb: EndpointId,
        note: &Note,
        passphrase: &str,
    ) -> erisoverlaynetwork::Result<Outcome> {
        enroll::enroll(&self.node, erisdb, note, passphrase).await
    }

    /// Change ErisDB's passphrase, as eon does for a person who knows it.
    async fn change(
        &self,
        erisdb: EndpointId,
        current: &str,
        new: &str,
    ) -> erisoverlaynetwork::Result<Outcome> {
        enroll::change(&self.node, erisdb, current, new).await
    }
}

/// What an app uses: one capability, with `grants` there.
fn uses(capability: &str, grants: &[&str]) -> Uses {
    Uses::from([(
        capability.parse().unwrap(),
        grants.iter().map(|g| g.to_string()).collect(),
    )])
}

/// How ErisDB serves the database: the mesh's one, taking a passphrase.
fn the_database() -> Serves {
    Serves {
        capability: DATABASE.parse().unwrap(),
        single: true,
        passphrase: true,
    }
}

/// Ask ErisDB at `at` over `eris-enroll/0` from `endpoint`, without syncing
/// first: as a stranger does, or a device the mesh removed.
async fn ask_unsynced(
    endpoint: &iroh::Endpoint,
    at: iroh::EndpointAddr,
    request: enroll::Request,
) -> erisoverlaynetwork::Result<Outcome> {
    let conn = endpoint.connect(at, enroll::ALPN).await.unwrap();
    erisoverlaynetwork::wire::ask(&conn, &request).await
}

/// Ask until the answer is one `want`s, for things the mesh carries a moment
/// later.
async fn until_answered<T: std::fmt::Debug>(
    what: &str,
    want: impl Fn(&T) -> bool,
    mut ask: impl AsyncFnMut() -> T,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let got = ask().await;
        if want(&got) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never the answer wanted for {what}: {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What the stand-in eon says to a request.
#[derive(Clone, Copy, PartialEq)]
enum Person {
    Approves,
    Declines,
}

/// The agent socket of `device`'s eon, under a runtime directory of its own.
/// It answers every request as eon does once the person has answered: an
/// approval signs the note with the device's node, and an app approved to
/// serve a capability a mesh has a single server of replaces every server of
/// it before it, whose notes are revoked.
struct Agent {
    runtime: tempfile::TempDir,
    asked: Arc<Mutex<Vec<Request>>>,
}

impl Agent {
    async fn on(device: &Device, person: Person) -> Agent {
        let runtime = tempfile::tempdir().unwrap();
        let path = agent::socket_in(runtime.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (node, seen) = (device.node.clone(), asked.clone());
        let mut singles: Vec<(String, NoteId)> = Vec::new();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                seen.lock().unwrap().push(request.clone());
                let reply = match request {
                    Request::Approve { ask, .. } if person == Person::Approves => {
                        let note = node
                            .issue(ask.app, &ask.name, ask.uses, ask.serves.clone(), None)
                            .unwrap();
                        for served in ask.serves.iter().filter(|s| s.single) {
                            let capability = served.capability.to_string();
                            let known = node
                                .servers(&capability)
                                .into_iter()
                                .filter_map(|p| p.note().map(|n| n.id));
                            let mine = singles
                                .iter()
                                .filter(|(c, _)| *c == capability)
                                .map(|(_, id)| *id);
                            let before: BTreeSet<NoteId> = mine.chain(known).collect();
                            for old in before.into_iter().filter(|old| *old != note.id) {
                                node.append(Op::Revoke { note: old }).await.unwrap();
                            }
                            singles.retain(|(c, _)| *c != capability);
                            singles.push((capability, note.id));
                        }
                        Reply::Approved {
                            note,
                            snapshot: node.snapshot().await.unwrap(),
                        }
                    }
                    Request::Approve { .. } => Reply::Denied("the person said no".into()),
                    Request::Snapshot { .. } => Reply::Snapshot(node.snapshot().await.unwrap()),
                };
                let mut out = serde_json::to_vec(&reply).unwrap();
                out.push(b'\n');
                write.write_all(&out).await.unwrap();
            }
        });
        Agent { runtime, asked }
    }

    fn runtime(&self) -> &Path {
        self.runtime.path()
    }
}

// ---------------------------------------------------------------- the core

/// The erisdb binary with nothing from this process's environment but what
/// a deployment sets.
fn erisdb(database: &str, runtime: &Path) -> Command {
    erisdb_as(SECRET, database, runtime)
}

/// The same, for a deployment of its own: another secret, another key.
fn erisdb_as(secret: &str, database: &str, runtime: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_erisdb"));
    command
        .env_clear()
        .env("DATABASE_URL", database)
        .env("ERISDB_SECRET", secret)
        .env("ERIS_NETWORK", "loopback")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "erisdb=info,erisoverlaynetwork=debug,iroh=warn")
        .kill_on_drop(true);
    command
}

async fn run(command: &mut Command, stdin: Option<&str>) -> std::process::Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).await.unwrap();
    }
    drop(input);
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("the CLI finishes")
        .unwrap()
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A core: the erisdb process serving one database, its TCP url, and its
/// endpoint id.
struct Core {
    secret: String,
    database: String,
    runtime: PathBuf,
    url: String,
    id: EndpointId,
    log: Arc<Mutex<String>>,
    _child: Child,
}

impl Core {
    /// Join `founder`'s mesh through its eon, set the passphrase, and serve.
    async fn up(founder: &Device) -> (Core, Agent) {
        Core::up_with(founder, &[]).await
    }

    async fn up_with(founder: &Device, serve_args: &[&str]) -> (Core, Agent) {
        let eon = Agent::on(founder, Person::Approves).await;
        // A deployment of its own, as every core is: its own secret, its own key.
        let secret = format!("mesh-e2e-{}", uuid::Uuid::new_v4().simple());
        let core = Core::join(founder, &eon, &secret, serve_args).await;
        (core, eon)
    }

    /// A deployment with `secret`, joined through `eon` on `founder`,
    /// with its passphrase set, serving, and found by `founder` as the mesh's
    /// ErisDB.
    async fn join(founder: &Device, eon: &Agent, secret: &str, serve_args: &[&str]) -> Core {
        let database = database().await;
        let joined = run(
            erisdb_as(secret, &database, eon.runtime()).arg("join"),
            None,
        )
        .await;
        assert!(joined.status.success(), "join: {}", text(&joined));
        let core = Core::serve_as(secret, &database, eon.runtime(), serve_args).await;
        core.passphrase(PASSPHRASE).await;
        core.found_by(founder).await;
        core
    }

    async fn serve(database: &str, runtime: &Path, args: &[&str]) -> Core {
        Core::serve_as(SECRET, database, runtime, args).await
    }

    async fn serve_as(secret: &str, database: &str, runtime: &Path, args: &[&str]) -> Core {
        let mut child = erisdb_as(secret, database, runtime)
            .args(["serve", "--listen", "127.0.0.1:0"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let log = Arc::new(Mutex::new(String::new()));
        let (url_tx, url_rx) = tokio::sync::oneshot::channel();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let kept = log.clone();
        // Keep reading for as long as it runs, so its logging never blocks.
        tokio::spawn(async move {
            let mut url_tx = Some(url_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(at) = line.find("tcp: http://") {
                    if let Some(tx) = url_tx.take() {
                        let _ = tx.send(line[at + 5..].trim().to_string());
                    }
                }
                kept.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
        let url = tokio::time::timeout(Duration::from_secs(30), url_rx)
            .await
            .expect("the core listens")
            .unwrap_or_else(|_| panic!("the core exited: {}", log.lock().unwrap()));
        let id = erisdb::net::endpoint_id(secret.as_bytes());
        Core {
            secret: secret.into(),
            database: database.into(),
            runtime: runtime.into(),
            url,
            id,
            log,
            _child: child,
        }
    }

    fn cli(&self) -> Command {
        let mut command = erisdb_as(&self.secret, &self.database, &self.runtime);
        command.env("ERISDB_URL", &self.url);
        command
    }

    async fn passphrase(&self, passphrase: &str) {
        let set = run(
            self.cli().args(["passphrase", "set"]),
            Some(&format!("{passphrase}\n")),
        )
        .await;
        assert!(set.status.success(), "passphrase set: {}", text(&set));
    }

    /// Wait until `device` knows where this core is, as the mesh's database.
    async fn found_by(&self, device: &Device) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while device.node.server(DATABASE).is_none_or(|p| p.id != self.id) {
            let log = self.log.lock().unwrap().clone();
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} ({}, endpoint closed: {}) never found the database; directory {:?}; its log:\n{log}",
                device.name,
                device.id().fmt_short(),
                device.node.endpoint().is_closed(),
                device
                    .node
                    .directory()
                    .iter()
                    .map(|p| p.id.fmt_short().to_string())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Where this core is, as `device`'s snapshot says: what an app gets
    /// back from eon.
    async fn address(&self, device: &Device) -> iroh::EndpointAddr {
        let snapshot: Snapshot = device.node.snapshot().await.unwrap();
        let server = snapshot.server(DATABASE).expect("the mesh has a database");
        assert_eq!(server.id, self.id, "this core is the mesh's database");
        server.endpoint_addr()
    }

    fn token(&self, grants: &[&str]) -> String {
        erisdb::auth::mint(self.secret.as_bytes(), grants, Some(3600), None).unwrap()
    }

    async fn tcp(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn client(&self, id: &str) -> Value {
        let (status, client) = self
            .tcp(
                reqwest::Method::GET,
                &format!("/v1/clients/{id}"),
                &self.token(&["*"]),
                None,
            )
            .await;
        assert_eq!(status, 200, "{client}");
        client
    }
}

/// An app: its own key and endpoint, nothing else.
struct App {
    key: SecretKey,
    conn: Connection,
    _endpoint: iroh::Endpoint,
}

impl App {
    async fn new(core: &Core, device: &Device) -> App {
        App::with_key(SecretKey::generate(), core, device).await
    }

    async fn with_key(key: SecretKey, core: &Core, device: &Device) -> App {
        let endpoint = net::endpoint(key.clone(), &Network::Loopback)
            .await
            .unwrap();
        let conn = endpoint
            .connect(core.address(device).await, ALPN)
            .await
            .unwrap();
        App {
            key,
            conn,
            _endpoint: endpoint,
        }
    }

    fn id(&self) -> EndpointId {
        self.key.public()
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        http(&self.conn, method, path, body, None).await
    }

    /// What this app holds, once a device enrolled it: its grants, and the
    /// installation they belong to.
    async fn held(&self) -> Value {
        let (status, held) = self.call("GET", "/v1/permissions", None).await;
        assert_eq!(status, 200, "{held}");
        held
    }
}

/// One HTTP/1.1 exchange on a fresh bi-stream: the database's wire.
async fn http(
    conn: &Connection,
    method: &str,
    path: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> (u16, Value) {
    let (send, recv) = conn.open_bi().await.unwrap();
    let (mut sender, driver) =
        hyper::client::conn::http1::handshake(TokioIo::new(tokio::io::join(recv, send)))
            .await
            .unwrap();
    tokio::spawn(driver);
    let mut request = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "erisdb");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let payload = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Bytes::from(body.to_string())
        }
        None => Bytes::new(),
    };
    let response = sender
        .send_request(request.body(Full::new(payload)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A change stream held open over one bi-stream.
struct Stream {
    body: hyper::body::Incoming,
    _sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
}

async fn subscribe(conn: &Connection, path: &str) -> Stream {
    let (send, recv) = conn.open_bi().await.unwrap();
    let (mut sender, driver) =
        hyper::client::conn::http1::handshake(TokioIo::new(tokio::io::join(recv, send)))
            .await
            .unwrap();
    tokio::spawn(driver);
    let request = hyper::Request::get(path)
        .header("host", "erisdb")
        .body(Full::<Bytes>::default())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    Stream {
        body: response.into_body(),
        _sender: sender,
    }
}

/// Read the stream until it ends, failing if it carries a change first.
async fn ends_without_a_change(mut stream: Stream, within: Duration) {
    tokio::time::timeout(within, async {
        while let Some(frame) = stream.body.frame().await {
            let Ok(frame) = frame else { return };
            if let Some(data) = frame.data_ref() {
                assert!(
                    !String::from_utf8_lossy(data).contains("event: change"),
                    "a cut-off app was sent a change"
                );
            }
        }
    })
    .await
    .expect("the stream closes");
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Ask until the answer is `status`, for things the mesh carries a moment later.
async fn until_status(
    what: &str,
    status: u16,
    mut ask: impl AsyncFnMut() -> (u16, Value),
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (got, body) = ask().await;
        if got == status {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never {status} for {what}: {got} {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------- joining

#[tokio::test(flavor = "multi_thread")]
async fn serve_refuses_until_a_device_approves_the_core_with_join() {
    let laptop = Device::founder("laptop").await;
    let database = database().await;
    let nobody = tempfile::tempdir().unwrap();

    let refused = run(
        erisdb(&database, nobody.path()).args(["serve", "--listen", "127.0.0.1:0"]),
        None,
    )
    .await;
    assert!(!refused.status.success());
    assert!(text(&refused).contains("erisdb join"), "{}", text(&refused));

    let alone = run(erisdb(&database, nobody.path()).arg("join"), None).await;
    assert!(!alone.status.success());
    assert!(text(&alone).contains("no eon"), "{}", text(&alone));

    let declined = Agent::on(&laptop, Person::Declines).await;
    let no = run(erisdb(&database, declined.runtime()).arg("join"), None).await;
    assert!(!no.status.success());
    assert!(text(&no).contains("said no"), "{}", text(&no));
    let still = run(
        erisdb(&database, nobody.path()).args(["serve", "--listen", "127.0.0.1:0"]),
        None,
    )
    .await;
    assert!(text(&still).contains("erisdb join"), "{}", text(&still));

    let eon = Agent::on(&laptop, Person::Approves).await;
    let joined = run(erisdb(&database, eon.runtime()).arg("join"), None).await;
    assert!(joined.status.success(), "{}", text(&joined));
    let printed = run(erisdb(&database, nobody.path()).arg("endpoint-id"), None).await;
    let id: EndpointId = String::from_utf8_lossy(&printed.stdout)
        .trim()
        .parse()
        .unwrap();
    match eon.asked.lock().unwrap().as_slice() {
        [Request::Approve { v, ask }] => {
            assert_eq!(*v, agent::VERSION);
            assert_eq!(ask.app, id);
            assert!(ask.uses.is_empty(), "ErisDB uses nothing: {ask:?}");
            assert_eq!(
                ask.serves,
                [the_database()],
                "ErisDB serves the database, the mesh's one, taking a passphrase"
            );
        }
        other => panic!("eon was asked {other:?}"),
    }

    // Serving, it is the mesh's database: the device finds it serving `database/1` as the
    // mesh's one, taking a passphrase, under the key `endpoint-id` names.
    let core = Core::serve(&database, nobody.path(), &[]).await;
    assert_eq!(core.id, id);
    core.found_by(&laptop).await;
    let found = laptop.node.server(DATABASE).unwrap();
    assert_eq!(found.serving(DATABASE), Some(&the_database()));
    // It knows the device, and takes its notes once its operator sets a passphrase.
    let note = laptop.approve(SecretKey::generate().public(), &["tasks:read"]);
    assert_eq!(
        laptop.enroll(id, &note, "x").await.unwrap(),
        Outcome::NoPassphrase
    );

    // Anyone may dial it; nobody reads without an installation or a token.
    let stranger = App::new(&core, &laptop).await;
    assert_eq!(stranger.call("GET", "/v1/health", None).await.0, 200);
    assert_eq!(
        stranger.call("GET", "/v1/items?facet=tasks", None).await.0,
        401
    );
    assert_eq!(stranger.call("GET", "/v1/permissions", None).await.0, 401);
    let token = core.token(&["tasks:read"]);
    assert_eq!(
        http(
            &stranger.conn,
            "GET",
            "/v1/items?facet=tasks",
            None,
            Some(&token)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        http(
            &stranger.conn,
            "GET",
            "/v1/items?facet=tasks",
            None,
            Some("erisdb1.not.real")
        )
        .await
        .0,
        401
    );
}

// ---------------------------------------------------------------- enrolling

#[tokio::test(flavor = "multi_thread")]
async fn an_app_a_device_enrolls_works_by_its_key_alone() {
    let laptop = Device::founder("laptop").await;
    let (core, _eon) = Core::up(&laptop).await;
    let app = App::new(&core, &laptop).await;
    assert_eq!(
        app.call("GET", "/v1/permissions", None).await.0,
        401,
        "an app is nothing before it enrolls"
    );

    // The laptop brings ErisDB the app's note and the passphrase, over `eris-enroll/0`.
    let note = laptop.approve(app.id(), &["tasks:read", "tasks:create", "tasks:update"]);
    assert_eq!(
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );

    let held = app.held().await;
    assert_eq!(
        held["grants"],
        json!(["tasks:read", "tasks:create", "tasks:update"]),
        "it holds what the note grants it in the database"
    );
    assert_eq!(held["exp"], Value::Null, "a key does not expire");
    let client = held["client_id"].as_str().unwrap().to_string();

    // It initializes its own missing schema, and nothing beyond it.
    let tasks = json!({"facet": "facet", "body": {"name": "tasks", "schema": {"type": "object", "required": ["title"]}}});
    let (status, facet) = app.call("POST", "/v1/items", Some(tasks.clone())).await;
    assert_eq!(status, 201, "{facet}");
    assert_eq!(app.call("POST", "/v1/items", Some(tasks)).await.0, 409);
    let lists = json!({"facet": "facet", "body": {"name": "lists", "schema": {}}});
    assert_eq!(app.call("POST", "/v1/items", Some(lists)).await.0, 403);
    let path = format!("/v1/items/{}", facet["id"].as_str().unwrap());
    assert_eq!(
        app.call(
            "PUT",
            &path,
            Some(json!({"revision": 1, "body": {"name": "tasks", "schema": {}}}))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        app.call(
            "POST",
            "/v1/items",
            Some(json!({"facet": "tasks", "body": {}}))
        )
        .await
        .0,
        422
    );

    let (status, item) = app
        .call(
            "POST",
            "/v1/items",
            Some(json!({"facet": "tasks", "body": {"title": "by key"}})),
        )
        .await;
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["source"]["installation"], client);
    assert_eq!(item["source"]["addr"], format!("iroh:{}", app.id()));
    let (status, _) = app
        .call(
            "GET",
            &format!("/v1/items/{}", item["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        app.call(
            "DELETE",
            &format!("/v1/items/{}", item["id"].as_str().unwrap()),
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(app.call("GET", "/v1/clients", None).await.0, 403);

    // A token it does not need to have still works when handed one.
    let reader = core.token(&["meta:server:read"]);
    assert_eq!(
        http(&app.conn, "GET", "/v1/server", None, Some(&reader))
            .await
            .0,
        200
    );
    assert_eq!(app.call("GET", "/v1/server", None).await.0, 403);
    let (status, refused) = app
        .call(
            "POST",
            "/v1/capabilities/refresh",
            Some(json!({"ttl_secs": 60})),
        )
        .await;
    assert_eq!(status, 400, "a key is not traded for a token: {refused}");

    // The registry says who let it in, and the feed says so too.
    let record = core.client(&client).await;
    assert_eq!(record["app"], app.id().to_string());
    assert_eq!(record["device"], laptop.id().to_string());
    assert_eq!(record["note"], note.id.to_string());
    assert_eq!(record["approved"], held["grants"]);
    let listed = run(core.cli().args(["clients", "list"]), None).await;
    assert!(listed.status.success(), "{}", text(&listed));
    assert!(text(&listed).contains(&client));
    let (_, feed) = core
        .tcp(
            reqwest::Method::GET,
            "/v1/changes?facet=system",
            &core.token(&["*"]),
            None,
        )
        .await;
    let created = feed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["body"]["client"]["id"] == client)
        .unwrap();
    assert_eq!(created["op"], "created");
    assert_eq!(created["source"]["addr"], format!("iroh:{}", laptop.id()));
}

#[tokio::test(flavor = "multi_thread")]
async fn enrollment_takes_only_a_member_devices_own_note_for_the_database() {
    let laptop = Device::founder("laptop").await;
    let phone = laptop.add("phone").await;
    let (core, _eon) = Core::up(&laptop).await;
    core.found_by(&phone).await;
    let app = SecretKey::generate().public();
    let note = laptop.approve(app, &["tasks:read"]);
    let enrolling = |note: &Note| enroll::Request::Enroll {
        note: note.clone(),
        passphrase: PASSPHRASE.into(),
    };

    // Not from a device of another mesh, whatever note it brings.
    let stranger = Device::founder("stranger").await;
    let theirs = stranger.approve(app, &["tasks:read"]);
    for note in [&theirs, &note] {
        let at = core.address(&laptop).await;
        let refused = ask_unsynced(stranger.node.endpoint(), at, enrolling(note)).await;
        assert!(
            matches!(refused, Err(Refused::Forbidden { .. })),
            "{refused:?}"
        );
    }

    // Not a member presenting another member's note.
    let refused = phone.enroll(core.id, &note, PASSPHRASE).await;
    assert!(
        matches!(refused, Err(Refused::Forbidden { .. })),
        "{refused:?}"
    );

    // Not a note that does not use the database, nor one that no longer counts, nor one no
    // device signed.
    let files = laptop.issue(app, uses("files/1", &["files:read"]), Vec::new());
    let server = laptop.issue(app, Uses::new(), vec![the_database()]);
    for bad in [&files, &server] {
        let refused = laptop.enroll(core.id, bad, PASSPHRASE).await;
        assert!(
            matches!(refused, Err(Refused::BadRequest(_))),
            "{refused:?}"
        );
    }
    let revoked = laptop.approve(SecretKey::generate().public(), &["tasks:read"]);
    laptop
        .node
        .append(Op::Revoke { note: revoked.id })
        .await
        .unwrap();
    laptop.node.sync_with(core.id).await.unwrap();
    let refused = laptop.enroll(core.id, &revoked, PASSPHRASE).await;
    assert!(
        matches!(refused, Err(Refused::Forbidden { .. })),
        "{refused:?}"
    );
    let conn = laptop.node.connect(core.id, enroll::ALPN).await.unwrap();
    let forged = json!({
        "op": "enroll",
        "note": {"by": laptop.id(), "body": "e30", "sig": "AAAA"},
        "passphrase": PASSPHRASE,
    });
    let refused: erisoverlaynetwork::Result<Outcome> =
        erisoverlaynetwork::wire::ask(&conn, &forged).await;
    assert!(
        matches!(refused, Err(Refused::BadRequest(_))),
        "{refused:?}"
    );

    // None of that cost the laptop a guess, and its own note goes through.
    assert_eq!(
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passphrases_lock_out_one_device_at_a_time() {
    let laptop = Device::founder("laptop").await;
    let phone = laptop.add("phone").await;
    // A one-second first wait in place of a minute; the schedule is the same.
    let (core, _eon) = Core::up_with(&laptop, &["--lockout-secs", "1"]).await;
    core.found_by(&phone).await;
    let enroll = async |device: &Device, passphrase: &str| {
        let note = device.approve(SecretKey::generate().public(), &["tasks:read"]);
        device.enroll(core.id, &note, passphrase).await.unwrap()
    };

    for _ in 0..2 {
        assert_eq!(
            enroll(&laptop, "wrong guess").await,
            Outcome::Wrong { retry_after: None },
            "the first two are free"
        );
    }
    assert_eq!(
        enroll(&laptop, "wrong guess").await,
        Outcome::Wrong {
            retry_after: Some(1)
        },
        "the third locks"
    );
    assert_eq!(
        enroll(&laptop, PASSPHRASE).await,
        Outcome::Locked { retry_after: 1 },
        "even a right guess waits"
    );

    // The phone guesses on its own count.
    assert_eq!(enroll(&phone, PASSPHRASE).await, Outcome::Done);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        enroll(&laptop, "wrong guess").await,
        Outcome::Wrong {
            retry_after: Some(2)
        },
        "each further guess doubles"
    );
    assert!(matches!(
        enroll(&laptop, PASSPHRASE).await,
        Outcome::Locked { .. }
    ));
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(enroll(&laptop, PASSPHRASE).await, Outcome::Done);
    assert_eq!(
        enroll(&laptop, "wrong guess").await,
        Outcome::Wrong { retry_after: None },
        "a right guess cleared the count"
    );

    // The operator unlocks a device on the server.
    enroll(&laptop, "wrong guess").await;
    assert_eq!(
        enroll(&laptop, "wrong guess").await,
        Outcome::Wrong {
            retry_after: Some(1)
        }
    );
    let unlocked = run(
        core.cli()
            .args(["passphrase", "unlock", &laptop.id().to_string()]),
        None,
    )
    .await;
    assert!(unlocked.status.success(), "{}", text(&unlocked));
    assert!(text(&unlocked).contains("1 device"), "{}", text(&unlocked));
    assert_eq!(enroll(&laptop, PASSPHRASE).await, Outcome::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_changes_the_passphrase_by_knowing_it() {
    let laptop = Device::founder("laptop").await;
    let (core, _eon) = Core::up(&laptop).await;
    assert_eq!(
        laptop
            .change(core.id, "not it at all", "a new passphrase")
            .await
            .unwrap(),
        Outcome::Wrong { retry_after: None }
    );
    let refused = laptop.change(core.id, PASSPHRASE, "short").await;
    assert!(
        matches!(refused, Err(Refused::BadRequest(_))),
        "{refused:?}"
    );
    assert_eq!(
        laptop
            .change(core.id, PASSPHRASE, "a new passphrase")
            .await
            .unwrap(),
        Outcome::Done
    );

    let note = laptop.approve(SecretKey::generate().public(), &["tasks:read"]);
    assert_eq!(
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Wrong { retry_after: None }
    );
    assert_eq!(
        laptop
            .enroll(core.id, &note, "a new passphrase")
            .await
            .unwrap(),
        Outcome::Done
    );

    let stranger = Device::founder("stranger").await;
    let change = enroll::Request::Change {
        current: "a new passphrase".into(),
        new: "the strangers own".into(),
    };
    let at = core.address(&laptop).await;
    let refused = ask_unsynced(stranger.node.endpoint(), at, change).await;
    assert!(
        matches!(refused, Err(Refused::Forbidden { .. })),
        "{refused:?}"
    );

    // The server resets it, whatever it was.
    core.passphrase("set on the server").await;
    let note = laptop.approve(SecretKey::generate().public(), &["tasks:read"]);
    assert_eq!(
        laptop
            .enroll(core.id, &note, "set on the server")
            .await
            .unwrap(),
        Outcome::Done
    );
}

// ---------------------------------------------------------------- leaving

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_device_cuts_its_apps_off_at_once() {
    let laptop = Device::founder("laptop").await;
    let phone = laptop.add("phone").await;
    let (core, _eon) = Core::up(&laptop).await;
    core.found_by(&phone).await;
    let admin = core.token(&["*"]);
    core.tcp(
        reqwest::Method::POST,
        "/v1/items",
        &admin,
        Some(json!({"facet": "facet", "body": {"name": "tasks", "schema": {}}})),
    )
    .await;

    let on_phone = App::new(&core, &phone).await;
    let note = phone.approve(on_phone.id(), &["tasks:*", "meta:capabilities:mint"]);
    assert_eq!(
        phone.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );
    let client = on_phone.held().await["client_id"]
        .as_str()
        .unwrap()
        .to_string();
    let on_laptop = App::new(&core, &laptop).await;
    let note = laptop.approve(on_laptop.id(), &["tasks:read"]);
    assert_eq!(
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );

    assert_eq!(
        on_phone
            .call(
                "POST",
                "/v1/items",
                Some(json!({"facet": "tasks", "body": {"n": 1}}))
            )
            .await
            .0,
        201
    );
    let (status, child) = on_phone
        .call(
            "POST",
            "/v1/capabilities",
            Some(json!({"grants": ["tasks:read"], "ttl_secs": 600})),
        )
        .await;
    assert_eq!(status, 201, "{child}");
    let child = child["token"].as_str().unwrap().to_string();
    assert_eq!(
        core.tcp(reqwest::Method::GET, "/v1/items?facet=tasks", &child, None)
            .await
            .0,
        200
    );
    let stream = subscribe(
        &on_phone.conn,
        "/v1/changes/stream?since=1000000&facet=tasks",
    )
    .await;

    // Another device removes the phone. Nobody tells ErisDB but the mesh.
    laptop
        .node
        .append(Op::Remove {
            devices: vec![phone.id()],
        })
        .await
        .unwrap();
    until_status("the phone's app", 401, async || {
        on_phone.call("GET", "/v1/items?facet=tasks", None).await
    })
    .await;
    ends_without_a_change(stream, Duration::from_secs(5)).await;
    assert_eq!(
        on_phone
            .call(
                "POST",
                "/v1/items",
                Some(json!({"facet": "tasks", "body": {"n": 2}}))
            )
            .await
            .0,
        401
    );
    assert_eq!(
        core.tcp(reqwest::Method::GET, "/v1/items?facet=tasks", &child, None)
            .await
            .0,
        401,
        "nor what it delegated"
    );
    assert_eq!(
        on_laptop.call("GET", "/v1/items?facet=tasks", None).await.0,
        200,
        "the laptop's apps carry on"
    );

    // The registry has it revoked, by the mesh, on the feed.
    let record = until_status("revocation", 200, async || {
        let record = core.client(&client).await;
        (
            if record["revoked_at"].is_null() {
                0
            } else {
                200
            },
            record,
        )
    })
    .await;
    assert!(!record["revoked_at"].is_null());
    let (_, feed) = core
        .tcp(
            reqwest::Method::GET,
            "/v1/changes?facet=system",
            &admin,
            None,
        )
        .await;
    let revoked = feed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|c| c["body"]["client"]["id"] == client)
        .unwrap();
    assert_eq!(revoked["source"]["mesh"], "device removed");

    // And a removed device enrolls nothing more.
    let enrolling = enroll::Request::Enroll {
        note: phone.approve(SecretKey::generate().public(), &["tasks:read"]),
        passphrase: PASSPHRASE.into(),
    };
    let at = core.address(&laptop).await;
    let refused = ask_unsynced(phone.node.endpoint(), at, enrolling).await;
    assert!(
        matches!(refused, Err(Refused::Forbidden { .. })),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_note_cuts_its_app_off_and_leaves_the_device_in() {
    let laptop = Device::founder("laptop").await;
    let (core, _eon) = Core::up(&laptop).await;
    let app = App::new(&core, &laptop).await;
    let note = laptop.approve(app.id(), &["tasks:read"]);
    assert_eq!(
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );
    let client = app.held().await["client_id"].as_str().unwrap().to_string();
    assert_eq!(app.call("GET", "/v1/items?facet=tasks", None).await.0, 200);
    let stream = subscribe(&app.conn, "/v1/changes/stream?since=1000000&facet=tasks").await;

    laptop
        .node
        .append(Op::Revoke { note: note.id })
        .await
        .unwrap();
    until_status("the revoked app", 401, async || {
        app.call("GET", "/v1/items?facet=tasks", None).await
    })
    .await;
    ends_without_a_change(stream, Duration::from_secs(5)).await;
    let record = until_status("revocation", 200, async || {
        let record = core.client(&client).await;
        (
            if record["revoked_at"].is_null() {
                0
            } else {
                200
            },
            record,
        )
    })
    .await;
    assert!(!record["revoked_at"].is_null());

    // The device stays, and approving the app anew makes a new installation.
    let again = laptop.approve(app.id(), &["tasks:read", "tasks:create"]);
    assert_eq!(
        laptop.enroll(core.id, &again, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );
    let fresh = app.held().await;
    assert_ne!(fresh["client_id"], client);
    assert_eq!(fresh["grants"], json!(["tasks:read", "tasks:create"]));
    assert_eq!(app.call("GET", "/v1/items?facet=tasks", None).await.0, 200);
}

// ---------------------------------------------------------------- operating

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_manages_enrolled_apps_from_the_cli() {
    let laptop = Device::founder("laptop").await;
    let (core, _eon) = Core::up(&laptop).await;
    let admin = core.token(&["*"]);
    core.tcp(
        reqwest::Method::POST,
        "/v1/items",
        &admin,
        Some(json!({"facet": "facet", "body": {"name": "tasks", "schema": {}}})),
    )
    .await;
    let enroll = async |app: EndpointId, grants: &[&str]| {
        let note = laptop.approve(app, grants);
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap()
    };
    let app = App::new(&core, &laptop).await;
    assert_eq!(
        enroll(app.id(), &["tasks:*", "*:read"]).await,
        Outcome::Done
    );
    let id = app.held().await["client_id"].as_str().unwrap().to_string();
    let (status, item) = app
        .call(
            "POST",
            "/v1/items",
            Some(json!({"facet": "tasks", "body": {"t": "before"}})),
        )
        .await;
    assert_eq!(status, 201, "{item}");

    let cli = async |args: &[&str]| run(core.cli().arg("clients").args(args), None).await;
    for args in [vec!["list"], vec!["show", &id]] {
        let out = cli(&args).await;
        assert!(
            out.status.success() && text(&out).contains(&id),
            "{}",
            text(&out)
        );
    }
    // Narrowing applies to the very next request; it never reaches past the note.
    let narrowed = cli(&["permissions", &id, "--grant", "*:read"]).await;
    assert!(narrowed.status.success(), "{}", text(&narrowed));
    assert_eq!(
        app.call(
            "GET",
            &format!("/v1/items/{}", item["id"].as_str().unwrap()),
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        app.call(
            "POST",
            "/v1/items",
            Some(json!({"facet": "tasks", "body": {"t": "after"}}))
        )
        .await
        .0,
        403
    );
    let widened = cli(&["permissions", &id, "--grant", "lists:create"]).await;
    assert!(
        !widened.status.success() && text(&widened).contains("403"),
        "{}",
        text(&widened)
    );

    // Enrolling again while active gives the same installation the new note's grants.
    assert_eq!(enroll(app.id(), &["tasks:read"]).await, Outcome::Done);
    let held = app.held().await;
    assert_eq!(held["client_id"], id);
    assert_eq!(held["grants"], json!(["tasks:read"]));

    let revoked = cli(&["revoke", &id]).await;
    assert!(revoked.status.success(), "{}", text(&revoked));
    assert_eq!(app.call("GET", "/v1/permissions", None).await.0, 401);
    assert_eq!(enroll(app.id(), &["tasks:read"]).await, Outcome::Done);
    assert_ne!(
        app.held().await["client_id"],
        id,
        "a revoked installation stays revoked"
    );

    // Two enrollments of one new app at once make one installation.
    let twin = SecretKey::generate().public();
    let (first, second) = (
        laptop.approve(twin, &["tasks:read"]),
        laptop.approve(twin, &["tasks:read"]),
    );
    let (a, b) = tokio::join!(
        laptop.enroll(core.id, &first, PASSPHRASE),
        laptop.enroll(core.id, &second, PASSPHRASE)
    );
    assert_eq!((a.unwrap(), b.unwrap()), (Outcome::Done, Outcome::Done));
    let (_, all) = core
        .tcp(reqwest::Method::GET, "/v1/clients", &admin, None)
        .await;
    let active: Vec<&Value> = all["clients"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["revoked_at"].is_null())
        .collect();
    assert_eq!(active.len(), 2);
    assert_eq!(
        active
            .iter()
            .filter(|c| c["app"] == twin.to_string())
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_is_told_which_installation_called_it() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("echo.json"),
        serde_json::to_vec(&json!({
            "protocol": 1, "name": "echo", "executable": "/bin/sh",
            "args": ["-c", "IFS= read -r invocation\nprintf '%s\\n' '{\"protocol\":1,\"status\":200,\"content_type\":\"application/json\"}'\nprintf '%s' \"$invocation\""],
            "environment": {}, "timeout_secs": 5,
            "operations": {"call": {"permission": "echo:call", "request_schema": {"type": "object"}}}
        }))
        .unwrap(),
    )
    .unwrap();
    let laptop = Device::founder("laptop").await;
    let (core, _eon) = Core::up_with(
        &laptop,
        &["--plugin-dir", directory.path().to_str().unwrap()],
    )
    .await;
    let app = App::new(&core, &laptop).await;
    let note = laptop.approve(app.id(), &["echo:call"]);
    assert_eq!(
        laptop.enroll(core.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );
    let installation = app.held().await["client_id"].clone();
    let call = json!({"plugin": "echo", "operation": "call", "input": {}});

    let (status, invocation) = app.call("POST", "/v1/call", Some(call.clone())).await;
    assert_eq!(status, 200, "{invocation} {}", core.log.lock().unwrap());
    assert_eq!(
        invocation["context"],
        json!({ "installation": installation })
    );
    let (status, invocation) = core
        .tcp(
            reqwest::Method::POST,
            "/v1/call",
            &core.token(&["*"]),
            Some(call),
        )
        .await;
    assert_eq!(status, 200, "{invocation}");
    assert_eq!(
        invocation["context"],
        json!({}),
        "a manual token names no installation"
    );
}

// ---------------------------------------------------------------- one database

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_database_takes_the_apps_and_the_old_one_lets_no_more_in() {
    let laptop = Device::founder("laptop").await;
    let (old, eon) = Core::up(&laptop).await;
    let at_old = old.address(&laptop).await;
    let early = App::new(&old, &laptop).await;
    let note = laptop.approve(early.id(), &["tasks:read"]);
    assert_eq!(
        laptop.enroll(old.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );

    // The person approves another database; eon revokes the first one's note.
    let new = Core::join(&laptop, &eon, "another-deployment", &[]).await;
    assert_ne!(new.id, old.id);
    let snapshot = laptop.node.snapshot().await.unwrap();
    assert_eq!(
        snapshot.server(DATABASE).map(|p| p.id),
        Some(new.id),
        "apps find the newer one"
    );
    assert_eq!(snapshot.servers(DATABASE).len(), 1, "the mesh has one");
    assert_eq!(eon.asked.lock().unwrap().len(), 2);

    // The old one hears of it, says so, and lets no app in.
    let enrolling = enroll::Request::Enroll {
        note: laptop.approve(SecretKey::generate().public(), &["tasks:read"]),
        passphrase: PASSPHRASE.into(),
    };
    until_answered(
        "enrolling at the old database",
        |answer| matches!(answer, Err(Refused::Forbidden { .. })),
        async || ask_unsynced(laptop.node.endpoint(), at_old.clone(), enrolling.clone()).await,
    )
    .await;
    let change = enroll::Request::Change {
        current: PASSPHRASE.into(),
        new: "a passphrase no one uses".into(),
    };
    let refused = ask_unsynced(laptop.node.endpoint(), at_old.clone(), change).await;
    assert!(
        matches!(refused, Err(Refused::Forbidden { .. })),
        "{refused:?}"
    );
    eventually("the old database says it is no longer the mesh's", || {
        old.log
            .lock()
            .unwrap()
            .contains("this ErisDB's approval no longer counts")
    })
    .await;

    // The newer one is where apps get in.
    let app = App::new(&new, &laptop).await;
    let note = laptop.approve(app.id(), &["tasks:read"]);
    assert_eq!(
        laptop.enroll(new.id, &note, PASSPHRASE).await.unwrap(),
        Outcome::Done
    );
    assert_eq!(app.held().await["grants"], json!(["tasks:read"]));
}
