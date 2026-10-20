//! The erisdb-client interface, proven against a real core in a real mesh:
//! Postgres in a container, the erisdb binary joined to a mesh and serving
//! over Iroh, a device node built from the mesh library in this process,
//! and this client dialing as apps the device let in. ErisAuth is a stand-in
//! on its agent socket that does what ErisAuth does once the person says
//! yes, through this crate: it signs the note and enrolls it at the mesh's
//! ErisDB from the device's own endpoint. Every endpoint is on loopback.

// The facade lock is held across awaits on purpose: it serializes whole
// tests against a process-wide client, which is exactly the span it has
// to cover.
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use erisdb_client::{Client, Enrolled, Guarded, SignIn};
use erisoverlaynetwork::agent::{self, Ask, Reply, Request};
use erisoverlaynetwork::devices::Op;
use erisoverlaynetwork::mesh::{self, Config, Local, MemoryStore, Node};
use erisoverlaynetwork::net::{self, Network};
use erisoverlaynetwork::NoteId;
use iroh::protocol::Router;
use iroh::{EndpointAddr, EndpointId, SecretKey};
use serde_json::{json, Value};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PASSPHRASE: &str = "correct horse battery staple";

/// The blocking facade is process-wide by design — one runtime, one
/// client slot — so the tests that drive it take turns.
fn facade() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The erisdb binary: `ERISDB_BIN`, or `../erisdb` built once for the run.
fn erisdb_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            if let Ok(path) = std::env::var("ERISDB_BIN") {
                return PathBuf::from(path);
            }
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../erisdb/Cargo.toml");
            let built = std::process::Command::new(env!("CARGO"))
                .args(["build", "--bin", "erisdb", "--manifest-path"])
                .arg(&manifest)
                .status()
                .expect("build the erisdb binary");
            assert!(built.success(), "a real ErisDB binary is required; build ../erisdb or set ERISDB_BIN");
            manifest.with_file_name("target/debug/erisdb")
        })
        .clone()
}

/// A device as ErisAuth runs one.
struct Device {
    node: Node,
    _router: Router,
}

impl Device {
    async fn founder(name: &str) -> Device {
        let key = SecretKey::generate();
        let endpoint = net::endpoint(key.clone(), &Network::Loopback).await.unwrap();
        let node = Node::start(Config {
            endpoint: endpoint.clone(),
            network: Network::Loopback,
            local: Local::Device(key),
            store: Arc::new(MemoryStore::default()),
        })
        .await
        .unwrap();
        node.create(name).await.unwrap();
        let router = Router::builder(endpoint).accept(mesh::ALPN, node.protocol()).spawn();
        Device { node, _router: router }
    }

    /// ErisDB as ErisAuth reaches it: from the device's own endpoint, synced first.
    async fn erisdb(&self) -> Client {
        erisauth_client(&self.node).await
    }
}

async fn erisauth_client(node: &Node) -> Client {
    let erisdb = node.provider("erisdb").expect("the mesh has an ErisDB");
    node.sync_with(erisdb.id).await.unwrap();
    Client::over(node.endpoint().clone(), erisdb.endpoint_addr(), "ErisAuth")
}

/// What the person at the stand-in ErisAuth does with a request: approve
/// these grants, or say no.
type Person = Arc<dyn Fn(&Ask) -> Option<Vec<String>> + Send + Sync>;

fn approves_as_asked() -> Person {
    Arc::new(|ask: &Ask| Some(ask.grants.clone()))
}

/// ErisAuth's agent socket on `device`, answering as ErisAuth does once the
/// person has: an app approved to serve `erisdb` replaces every ErisDB before
/// it, whose notes are revoked; an app asking to call it is enrolled at the
/// mesh's ErisDB with the passphrase, and approved only once ErisDB took it.
struct Agent {
    runtime: tempfile::TempDir,
}

impl Agent {
    async fn on(device: &Device, person: Person) -> Agent {
        let runtime = tempfile::tempdir().unwrap();
        let path = agent::socket_in(runtime.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let node = device.node.clone();
        let erisdbs: Arc<tokio::sync::Mutex<Vec<NoteId>>> = Arc::default();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (node, person, erisdbs) = (node.clone(), person.clone(), erisdbs.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    BufReader::new(read).read_line(&mut line).await.unwrap();
                    let reply = match serde_json::from_str::<Request>(&line).unwrap() {
                        Request::Approve { ask, .. } => answer(&node, &erisdbs, &ask, person(&ask)).await,
                        Request::Snapshot { .. } => Reply::Snapshot(node.snapshot().await.unwrap()),
                    };
                    let mut out = serde_json::to_vec(&reply).unwrap();
                    out.push(b'\n');
                    let _ = write.write_all(&out).await;
                });
            }
        });
        Agent { runtime }
    }

    fn socket(&self) -> PathBuf {
        agent::socket_in(self.runtime.path())
    }
}

async fn answer(
    node: &Node,
    erisdbs: &tokio::sync::Mutex<Vec<NoteId>>,
    ask: &Ask,
    granted: Option<Vec<String>>,
) -> Reply {
    let Some(grants) = granted else { return Reply::Denied("the person said no".into()) };
    let note = node.issue(ask.app, &ask.name, &ask.service, grants, ask.serves, None).unwrap();
    if ask.serves {
        let mut approved = erisdbs.lock().await;
        let known = node.providers("erisdb").into_iter().filter_map(|p| p.note().map(|n| n.id));
        let before: Vec<NoteId> = approved.drain(..).chain(known).collect();
        for old in before.into_iter().filter(|old| *old != note.id) {
            node.append(Op::Revoke { note: old }).await.unwrap();
        }
        approved.push(note.id);
    } else {
        match erisauth_client(node).await.enroll(&note, PASSPHRASE).await {
            Ok(Guarded::Passed(_)) => {}
            other => return Reply::Error(format!("ErisDB did not take the note: {other:?}")),
        }
    }
    Reply::Approved { note, snapshot: node.snapshot().await.unwrap() }
}

/// An ErisDB deployment: its store, its secret, and the core serving it.
struct Core {
    _pg: ContainerAsync<Postgres>,
    secret: String,
    database: String,
    id: EndpointId,
    _process: tokio::process::Child,
}

impl Core {
    /// A deployment of its own, joined to `laptop`'s mesh through `erisauth`,
    /// its passphrase set, serving, and found by the laptop as the mesh's ErisDB.
    async fn join(laptop: &Device, erisauth: &Agent) -> Core {
        let pg = Postgres::default().start().await.expect("start postgres");
        let port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let database = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let secret = format!("client-e2e-{}", SecretKey::generate().public());
        let core_cli = |args: &[&str]| cli(&secret, &database, erisauth.runtime.path(), args);
        let joined = core_cli(&["join"]).output().await.unwrap();
        assert!(joined.status.success(), "{}", String::from_utf8_lossy(&joined.stderr));
        let mut set = core_cli(&["passphrase", "set"]).stdin(Stdio::piped()).spawn().unwrap();
        set.stdin.take().unwrap().write_all(format!("{PASSPHRASE}\n").as_bytes()).await.unwrap();
        assert!(set.wait().await.unwrap().success());
        let process = core_cli(&["serve", "--listen", "127.0.0.1:0"]).stdout(Stdio::null()).spawn().unwrap();
        let printed = core_cli(&["endpoint-id"]).output().await.unwrap();
        let id: EndpointId = String::from_utf8_lossy(&printed.stdout).trim().parse().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while laptop.node.provider("erisdb").is_none_or(|p| p.id != id) {
            assert!(tokio::time::Instant::now() < deadline, "the laptop never found ErisDB");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Core { _pg: pg, secret, database, id, _process: process }
    }

    fn cli(&self, runtime: &Path, args: &[&str]) -> tokio::process::Command {
        cli(&self.secret, &self.database, runtime, args)
    }
}

/// A mesh with an ErisDB in it: the device that let it in, its ErisAuth, and
/// the core.
struct World {
    laptop: Device,
    erisauth: Agent,
    core: Core,
    id: EndpointId,
}

impl World {
    async fn new() -> World {
        let laptop = Device::founder("laptop").await;
        let erisauth = Agent::on(&laptop, approves_as_asked()).await;
        let core = Core::join(&laptop, &erisauth).await;
        World { id: core.id, laptop, erisauth, core }
    }

    /// Where ErisDB is, as the device's snapshot says: what ErisAuth hands an app.
    async fn addr(&self) -> EndpointAddr {
        erisdb_client::locate(&self.laptop.node.snapshot().await.unwrap()).unwrap()
    }

    /// Let the app holding `key` in, as the laptop does.
    async fn enroll(&self, key: &SecretKey, grants: &[&str]) -> Enrolled {
        let note = self.laptop.node.issue(key.public(), "Tasks", "erisdb", strings(grants), false, None).unwrap();
        match self.laptop.erisdb().await.enroll(&note, PASSPHRASE).await.unwrap() {
            Guarded::Passed(enrolled) => enrolled,
            other => panic!("enrollment refused: {other:?}"),
        }
    }

    /// An app the laptop let in with `grants`, dialing as `name`.
    async fn app(&self, seed: u8, grants: &[&str], name: &str) -> Client {
        let key = SecretKey::from_bytes(&[seed; 32]);
        self.enroll(&key, grants).await;
        Client::dial_on(self.addr().await, key, name, &Network::Loopback).await.unwrap()
    }
}

fn cli(secret: &str, database: &str, runtime: &Path, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(erisdb_binary());
    command
        .env_clear()
        .env("DATABASE_URL", database)
        .env("ERISDB_SECRET", secret)
        .env("ERIS_NETWORK", "loopback")
        .env("XDG_RUNTIME_DIR", runtime)
        .args(args)
        .kill_on_drop(true);
    command
}

fn strings(grants: &[&str]) -> Vec<String> {
    grants.iter().map(|g| g.to_string()).collect()
}

async fn register(client: &Client, facet: &str) {
    let (status, body) = client
        .request("POST", "/v1/items", Some(json!({"facet": "facet", "body": {"name": facet, "schema": {}}})))
        .await
        .unwrap();
    assert_eq!(status, 201, "{body}");
}

// ---------------------------------------------------------------- the client

#[tokio::test(flavor = "multi_thread")]
async fn the_client_speaks_erisdb_over_iroh_by_its_key() {
    let world = World::new().await;
    let key = SecretKey::from_bytes(&[7u8; 32]);
    let enrolled = world.enroll(&key, &["lists:read", "lists:create"]).await;
    assert_eq!(enrolled.grants, ["lists:read", "lists:create"]);
    let client = Client::dial_on(world.addr().await, key.clone(), "Lists (Android) v0.1", &Network::Loopback).await.unwrap();
    assert_eq!(client.server().id, world.id);
    assert_eq!(client.id(), key.public());

    let (status, body) = client.request("GET", "/v1/health", None).await.unwrap();
    assert_eq!((status, &body["ok"]), (200, &json!(true)));
    register(&client, "lists").await;
    let (status, item) = client
        .request("POST", "/v1/items", Some(json!({"facet": "lists", "body": {"list": "books", "name": "Piranesi"}})))
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");
    // The source pipeline holds over QUIC: the claimed client, the observed
    // key as the address, and the installation that key belongs to.
    assert_eq!(item["source"]["client"], "Lists (Android) v0.1");
    assert_eq!(item["source"]["addr"], format!("iroh:{}", key.public()));
    assert_eq!(item["source"]["installation"], enrolled.client);

    let (status, listed) = client.request("GET", "/v1/items?facet=lists", None).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    // Several requests share one connection, one bi-stream each.
    for _ in 0..3 {
        assert_eq!(client.request("GET", "/v1/health", None).await.unwrap().0, 200);
    }
    let (status, _) = client.request("DELETE", &format!("/v1/items/{}", item["id"].as_str().unwrap()), None).await.unwrap();
    assert_eq!(status, 403, "lists:delete was never approved");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_client_reads_its_own_permissions() {
    let world = World::new().await;
    let client = world.app(16, &["tasks:read", "lists:*"], "Curious v0").await;
    let held = client.permissions().await.unwrap();
    assert_eq!(held.grants, ["tasks:read", "lists:*"]);
    assert!(held.client.is_some(), "an installation says which it is");
    assert_eq!(held.exp, None, "a key does not expire");
}

/// The answers ErisAuth branches on come back typed: a wrong passphrase
/// (and whether it locked the device), a lockout and how long, no
/// passphrase at all.
#[tokio::test(flavor = "multi_thread")]
async fn enrollment_and_passphrase_answers_are_typed() {
    let world = World::new().await;
    let erisdb = world.laptop.erisdb().await;
    let note = |seed: u8| {
        let app = SecretKey::from_bytes(&[seed; 32]).public();
        world.laptop.node.issue(app, "Tasks", "erisdb", strings(&["tasks:read"]), false, None).unwrap()
    };
    assert!(matches!(erisdb.enroll(&note(1), "not it").await.unwrap(), Guarded::Wrong { retry_after: None }));
    assert!(matches!(erisdb.enroll(&note(1), "not it").await.unwrap(), Guarded::Wrong { retry_after: None }));
    assert!(matches!(erisdb.enroll(&note(1), "not it").await.unwrap(), Guarded::Wrong { retry_after: Some(60) }));
    assert!(matches!(erisdb.enroll(&note(1), PASSPHRASE).await.unwrap(), Guarded::Locked { retry_after: 1..=60 }));
    assert!(matches!(erisdb.change_passphrase(PASSPHRASE, "a new one").await.unwrap(), Guarded::Locked { .. }));
    let unlocked = world.core.cli(world.erisauth.runtime.path(), &["passphrase", "unlock"]).output().await.unwrap();
    assert!(unlocked.status.success());

    assert!(matches!(erisdb.change_passphrase(PASSPHRASE, "a new one").await.unwrap(), Guarded::Passed(())));
    assert!(matches!(erisdb.enroll(&note(2), PASSPHRASE).await.unwrap(), Guarded::Wrong { .. }));
    let Guarded::Passed(enrolled) = erisdb.enroll(&note(2), "a new one").await.unwrap() else { panic!("refused") };
    assert_eq!(enrolled.grants, ["tasks:read"]);

    // A refusal that is not about the passphrase is an error carrying its status.
    let refused = erisdb.change_passphrase("a new one", "short").await.unwrap_err();
    assert_eq!(refused.downcast_ref::<erisdb_client::Refused>().map(|r| r.status), Some(400));
    let stranger = Device::founder("stranger").await;
    let theirs = Client::over(stranger.node.endpoint().clone(), world.addr().await, "ErisAuth");
    let app = SecretKey::generate().public();
    let their_note = stranger.node.issue(app, "Tasks", "erisdb", strings(&["tasks:read"]), false, None).unwrap();
    let refused = theirs.enroll(&their_note, "a new one").await.unwrap_err();
    let refused = refused.downcast_ref::<erisdb_client::Refused>().expect("a refusal");
    assert_eq!((refused.status, &refused.body["error"]), (403, &json!("not_a_device")));
}

// Multi-threaded: the subscription holds one bi-stream open while a
// second connection writes through the same core.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_streams_the_change_feed() {
    let world = World::new().await;
    let reader = world.app(3, &["notes:read"], "Reader v0").await;
    let writer = world.app(4, &["notes:*"], "Writer v0").await;
    register(&writer, "notes").await;

    // The caller owns the cursor: start from where the feed is now.
    let (status, feed) = reader.request("GET", "/v1/changes?facet=notes&since=0", None).await.unwrap();
    assert_eq!(status, 200, "{feed}");
    let since = feed["next"].as_i64().unwrap();
    let mut sub = reader.subscribe_changes(since, Some("notes")).await.expect("subscribe");

    let (_, item) = writer.request("POST", "/v1/items", Some(json!({"facet": "notes", "body": {"text": "hi"}}))).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), sub.next()).await.unwrap().unwrap().unwrap();
    assert!(event.seq > since);
    assert_eq!((event.facet.as_str(), event.op.as_str()), ("notes", "created"));
    assert_eq!(event.body.as_ref().unwrap()["text"], "hi");
    assert_eq!(event.item_id.as_deref(), item["id"].as_str());
    assert_eq!(event.raw["seq"], event.seq);

    let (_, second) = writer.request("POST", "/v1/items", Some(json!({"facet": "notes", "body": {"text": "again"}}))).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(10), sub.next()).await.unwrap().unwrap().unwrap();
    assert!(next.seq > event.seq);
    assert_eq!(next.item_id.as_deref(), second["id"].as_str());
}

/// When the mesh ends an app — its device removed, here — the app hears
/// 401 on its next call and its open feed ends.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_the_mesh_ends_hears_401_and_its_feed_ends() {
    let world = World::new().await;
    let phone_key = SecretKey::generate();
    let phone_endpoint = net::endpoint(phone_key.clone(), &Network::Loopback).await.unwrap();
    let phone = Node::start(Config {
        endpoint: phone_endpoint.clone(),
        network: Network::Loopback,
        local: Local::Device(phone_key),
        store: Arc::new(MemoryStore::default()),
    })
    .await
    .unwrap();
    let _router = Router::builder(phone_endpoint).accept(mesh::ALPN, phone.protocol()).spawn();
    world.laptop.node.append(Op::Add { device: phone.id(), name: "phone".into() }).await.unwrap();
    phone.adopt(world.laptop.node.snapshot().await.unwrap()).await.unwrap();

    let key = SecretKey::generate();
    let note = phone.issue(key.public(), "Tasks", "erisdb", strings(&["tasks:read"]), false, None).unwrap();
    let erisdb = Client::over(phone.endpoint().clone(), world.addr().await, "ErisAuth");
    phone.sync_with(world.id).await.unwrap();
    assert!(matches!(erisdb.enroll(&note, PASSPHRASE).await.unwrap(), Guarded::Passed(_)));
    let app = Client::dial_on(world.addr().await, key, "Tasks", &Network::Loopback).await.unwrap();
    let mut sub = app.subscribe_changes(1_000_000, Some("tasks")).await.unwrap();

    world.laptop.node.append(Op::Remove { devices: vec![phone.id()] }).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), sub.next()).await.expect("the feed ends");
    assert!(matches!(ended, Ok(None)), "{ended:?}");
    let (status, _) = app.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(status, 401);
}

// ---------------------------------------------------------------- signing in

#[tokio::test(flavor = "multi_thread")]
async fn an_app_signs_in_through_erisauth_with_one_call() {
    let world = World::new().await;
    // The person approves less than was asked.
    let narrowing = Agent::on(&world.laptop, Arc::new(|_: &Ask| Some(vec!["tasks:read".to_string()]))).await;
    let key = SecretKey::from_bytes(&[41u8; 32]);
    let signed_in =
        erisdb_client::sign_in_at(&narrowing.socket(), &Network::Loopback, key.clone(), "Tasks (Linux)", &strings(&["tasks:read", "tasks:create"]))
            .await
            .unwrap()
            .expect("an ErisAuth was listening");
    let SignIn::Approved { client, note, snapshot } = signed_in else { panic!("denied") };
    assert_eq!(client.server().id, world.id, "the app learns where ErisDB is");
    assert_eq!((note.app(), note.body.grants.clone()), (key.public(), strings(&["tasks:read"])));
    assert_eq!(erisdb_client::locate(&snapshot).unwrap().id, world.id);
    assert_eq!(client.permissions().await.unwrap().grants, ["tasks:read"]);

    // Dialing again later takes nothing but the saved address and the key.
    let again = Client::dial_on(client.server().clone(), key, "Tasks (Linux)", &Network::Loopback).await.unwrap();
    assert_eq!(again.permissions().await.unwrap().grants, ["tasks:read"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_no_through_erisauth_is_a_denial() {
    let world = World::new().await;
    let declining = Agent::on(&world.laptop, Arc::new(|_: &Ask| None)).await;
    let outcome = erisdb_client::sign_in_at(&declining.socket(), &Network::Loopback, SecretKey::generate(), "Tasks", &strings(&["tasks:read"]))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, SignIn::Denied(why) if why.contains("said no")));
}

#[tokio::test]
async fn without_erisauth_signing_in_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = erisdb_client::sign_in_at(
        &agent::socket_in(dir.path()),
        &Network::Loopback,
        SecretKey::generate(),
        "Tasks",
        &strings(&["tasks:read"]),
    )
    .await
    .unwrap();
    assert!(outcome.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mesh_s_erisdb_is_read_from_a_snapshot() {
    let world = World::new().await;
    let snapshot = world.laptop.node.snapshot().await.unwrap();
    let read: Value = serde_json::from_str(&erisdb_client::blocking::locate(&serde_json::to_string(&snapshot).unwrap())).unwrap();
    assert_eq!(read["ok"], true, "{read}");
    let addr: EndpointAddr = serde_json::from_value(read["server"].clone()).unwrap();
    assert_eq!(addr.id, world.id);

    let without = Device::founder("elsewhere").await.node.snapshot().await.unwrap();
    assert!(erisdb_client::locate(&without).is_err());
    let read: Value = serde_json::from_str(&erisdb_client::blocking::locate(&serde_json::to_string(&without).unwrap())).unwrap();
    assert_eq!(read["ok"], false, "{read}");
    let read: Value = serde_json::from_str(&erisdb_client::blocking::locate("not a snapshot")).unwrap();
    assert_eq!(read["ok"], false, "{read}");
}

/// A mesh has one ErisDB. Approving another revokes the first's note: apps
/// signing in land on the newer one, and the first lets no more apps in.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_erisdb_is_where_apps_go() {
    let world = World::new().await;
    let first = world.addr().await;
    let newer = Core::join(&world.laptop, &world.erisauth).await;
    assert_ne!(newer.id, world.id);
    assert_eq!(erisdb_client::locate(&world.laptop.node.snapshot().await.unwrap()).unwrap().id, newer.id);

    let key = SecretKey::generate();
    let signed_in = erisdb_client::sign_in_at(&world.erisauth.socket(), &Network::Loopback, key, "Tasks", &strings(&["tasks:read"]))
        .await
        .unwrap()
        .expect("an ErisAuth was listening");
    let SignIn::Approved { client, .. } = signed_in else { panic!("denied") };
    assert_eq!(client.server().id, newer.id);
    assert_eq!(client.permissions().await.unwrap().grants, ["tasks:read"]);

    let at_first = Client::over(world.laptop.node.endpoint().clone(), first, "ErisAuth");
    let note = world.laptop.node.issue(SecretKey::generate().public(), "Tasks", "erisdb", strings(&["tasks:read"]), false, None).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let refused = loop {
        match at_first.enroll(&note, PASSPHRASE).await {
            Err(e) if e.downcast_ref::<erisdb_client::Refused>().is_some_and(|r| r.status == 503) => break e,
            other => {
                assert!(tokio::time::Instant::now() < deadline, "the first ErisDB still lets apps in: {other:?}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let refused = refused.downcast_ref::<erisdb_client::Refused>().unwrap();
    assert_eq!(refused.body["error"], "not_the_erisdb", "{}", refused.body);
}

// ---------------------------------------------------------------- the blocking facade

// Multi-threaded: the test thread blocks in join() while the core and the
// device keep serving on the other workers.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_works_from_sync_code() {
    let _facade = facade();
    let world = World::new().await;
    let key = [9u8; 32];
    world.enroll(&SecretKey::from_bytes(&key), &["tasks:read"]).await;
    let server = serde_json::to_string(&world.addr().await).unwrap();

    // The facade owns its runtime: callable from a plain thread, exactly
    // like a JNI entry point.
    let (health, refused, held) = std::thread::spawn(move || {
        erisdb_client::blocking::dial_on(&server, "Test (Blocking) v0", &key, &Network::Loopback).expect("dial");
        (
            erisdb_client::blocking::request("GET", "/v1/health", None),
            erisdb_client::blocking::request("GET", "/v1/items?facet=lists", None),
            erisdb_client::blocking::permissions(),
        )
    })
    .join()
    .unwrap();
    let v: Value = serde_json::from_str(&health).unwrap();
    assert_eq!((v["status"].as_u64(), &v["body"]["ok"]), (Some(200), &json!(true)), "{health}");
    // Refusals come back as data, with the status to branch on.
    let v: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(v["status"], 403, "{refused}");
    let v: Value = serde_json::from_str(&held).unwrap();
    assert_eq!(v["permissions"]["grants"], json!(["tasks:read"]), "{held}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_subscription_pulls_changes() {
    let _facade = facade();
    let world = World::new().await;
    let writer = world.app(5, &["memo:*"], "Writer v0").await;
    register(&writer, "memo").await;
    let key = [6u8; 32];
    world.enroll(&SecretKey::from_bytes(&key), &["memo:read"]).await;
    let server = serde_json::to_string(&world.addr().await).unwrap();

    let sub = std::thread::spawn(move || {
        erisdb_client::blocking::dial_on(&server, "Test (Blocking) v0", &key, &Network::Loopback).expect("dial");
        erisdb_client::blocking::subscribe_changes(0, Some("memo")).expect("subscribe")
    })
    .join()
    .unwrap();

    // Nothing on the feed yet for this facet: the pull times out as data.
    let idle = std::thread::spawn(move || erisdb_client::blocking::next_change(sub, 500)).join().unwrap();
    let v: Value = serde_json::from_str(&idle).unwrap();
    assert_eq!(v["ok"], true, "{idle}");
    assert!(v.get("change").is_none(), "{idle}");

    let (_, item) = writer.request("POST", "/v1/items", Some(json!({"facet": "memo", "body": {"text": "yo"}}))).await.unwrap();
    let got = std::thread::spawn(move || erisdb_client::blocking::next_change(sub, 10_000)).join().unwrap();
    let v: Value = serde_json::from_str(&got).unwrap();
    assert_eq!(v["ok"], true, "{got}");
    assert_eq!(v["change"]["item_id"], item["id"], "{got}");

    erisdb_client::blocking::close_subscription(sub);
    // A closed handle answers as data, not a panic.
    let v: Value = serde_json::from_str(&erisdb_client::blocking::next_change(sub, 100)).unwrap();
    assert_eq!(v["ok"], false);
}

/// A caught panic leaves the facade usable: the next call is answered
/// normally.
#[tokio::test(flavor = "multi_thread")]
async fn the_facade_survives_a_panicking_call() {
    let _facade = facade();
    let world = World::new().await;
    let key = [14u8; 32];
    world.enroll(&SecretKey::from_bytes(&key), &["tasks:read"]).await;
    let server = serde_json::to_string(&world.addr().await).unwrap();
    let response = std::thread::spawn(move || {
        erisdb_client::blocking::dial_on(&server, "Guarded v0", &key, &Network::Loopback).expect("dial");
        erisdb_client::blocking::guard(|| panic!("boom"), erisdb_client::blocking::panic_envelope);
        erisdb_client::blocking::request("GET", "/v1/health", None)
    })
    .join()
    .unwrap();
    let v: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["status"], 200, "{response}");
}

/// Concurrent callers all make progress while a subscription is parked:
/// the facade's runtime is sized for a live feed plus a handful of calls,
/// which is what an app with a sync loop and a UI actually does.
#[tokio::test(flavor = "multi_thread")]
async fn the_facade_serves_calls_while_a_subscription_is_parked() {
    let _facade = facade();
    let world = World::new().await;
    let key = [15u8; 32];
    world.enroll(&SecretKey::from_bytes(&key), &["meta:feed:read"]).await;
    let server = serde_json::to_string(&world.addr().await).unwrap();
    let sub = std::thread::spawn(move || {
        erisdb_client::blocking::dial_on(&server, "Busy v0", &key, &Network::Loopback).expect("dial");
        erisdb_client::blocking::subscribe_changes(0, None).expect("subscribe")
    })
    .join()
    .unwrap();

    let parked = std::thread::spawn(move || erisdb_client::blocking::next_change(sub, 3_000));
    let callers: Vec<_> =
        (0..4).map(|_| std::thread::spawn(|| erisdb_client::blocking::request("GET", "/v1/health", None))).collect();
    for caller in callers {
        let response = caller.join().unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["status"], 200, "{response}");
    }
    parked.join().unwrap();
    erisdb_client::blocking::close_subscription(sub);
}

#[test]
fn a_dial_that_cannot_start_says_why() {
    let _facade = facade();
    assert!(erisdb_client::blocking::dial("not an address", "Nobody", &[1u8; 32]).unwrap_err().contains("endpoint id"));
    assert!(erisdb_client::blocking::dial(&"ab".repeat(32), "Nobody", &[1u8; 31]).unwrap_err().contains("32 bytes"));
}

// ---------------------------------------------------------------------
// The retry rule, watched from the other end of the wire.
//
// Whether a failed call is safe to repeat is invisible from the calling
// side: both a lost request and a lost response look like one error. The
// only witness is the server, which knows how many requests actually
// arrived. So these tests put a real Iroh peer speaking the erisdb ALPN on
// the other end and have it fail in specific, chosen ways.
// ---------------------------------------------------------------------

/// The proxy only changes delivery; every response comes from the real core.
#[derive(Clone, Copy, PartialEq)]
enum Misbehaviour {
    DieHoldingTheRequest,
    DieOnceThenAnswer,
    AnswerThenClose,
    PassThrough,
}

struct Peer {
    _world: World,
    core: Client,
    addr: EndpointAddr,
    requests: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
}

impl Peer {
    fn requests(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// A QUIC edge forwards complete HTTP exchanges to the actual ErisDB, as an
/// app of its own the laptop let in. It can discard the answer only AFTER
/// the core completed the transaction, reproducing an ambiguous write
/// without inventing any service responses.
async fn misbehaving_peer(how: Misbehaviour) -> Peer {
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper_util::rt::TokioIo;
    let world = World::new().await;
    let core = world.app(30, &["*"], "Dashboard v0").await;
    register(&core, "tasks").await;
    let proxy = SecretKey::generate();
    world.enroll(&proxy, &["*"]).await;
    let ep = net::endpoint(proxy, &Network::Loopback).await.unwrap();
    ep.set_alpns(vec![erisdb_client::ALPN.to_vec()]);
    let upstream = ep.connect(world.addr().await, erisdb_client::ALPN).await.unwrap();
    let addr = net::advertised(&ep, &Network::Loopback).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let (seen, count) = (requests.clone(), connections.clone());
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let Ok(conn) = incoming.await else { continue };
            let nth = count.fetch_add(1, Ordering::SeqCst);
            let (seen, upstream) = (seen.clone(), upstream.clone());
            tokio::spawn(async move {
                while let Ok((send, recv)) = conn.accept_bi().await {
                    let (seen, upstream, conn) = (seen.clone(), upstream.clone(), conn.clone());
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                            let (seen, upstream, conn) = (seen.clone(), upstream.clone(), conn.clone());
                            async move {
                                seen.lock().unwrap().push(format!("{} {}", request.method(), request.uri()));
                                let (parts, body) = request.into_parts();
                                let request = hyper::Request::from_parts(parts, Full::new(body.collect().await?.to_bytes()));
                                let (send, recv) = upstream.open_bi().await?;
                                let (mut sender, driver) =
                                    hyper::client::conn::http1::handshake(TokioIo::new(tokio::io::join(recv, send))).await?;
                                tokio::spawn(driver);
                                let response = sender.send_request(request).await?;
                                let (parts, body) = response.into_parts();
                                let body = body.collect().await?.to_bytes();
                                if how == Misbehaviour::DieHoldingTheRequest || how == Misbehaviour::DieOnceThenAnswer && nth == 0 {
                                    conn.close(1u32.into(), b"lost response after real core completed");
                                } else if how == Misbehaviour::AnswerThenClose {
                                    tokio::spawn(async move {
                                        tokio::time::sleep(Duration::from_millis(300)).await;
                                        conn.close(0u32.into(), b"one request per connection");
                                    });
                                }
                                Ok::<hyper::Response<Full<Bytes>>, anyhow::Error>(hyper::Response::from_parts(parts, Full::new(body)))
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(tokio::io::join(recv, send)), service)
                            .await;
                    });
                }
            });
        }
    });
    Peer { _world: world, core, addr, requests, connections }
}

async fn client_for(peer: &Peer, seed: u8) -> Client {
    Client::dial_on(peer.addr.clone(), SecretKey::from_bytes(&[seed; 32]), "Retry v0", &Network::Loopback).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_never_sent_twice() {
    let peer = misbehaving_peer(Misbehaviour::DieHoldingTheRequest).await;
    let client = client_for(&peer, 20).await;
    let outcome = client.request("POST", "/v1/items", Some(json!({"facet": "tasks", "body": {"text": "once"}}))).await;
    assert!(outcome.is_err(), "a lost answer is an error, not a silent retry");
    assert_eq!(peer.requests(), 1);
    let (status, stored) = peer.core.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(stored["items"].as_array().unwrap().len(), 1, "the real write committed exactly once");
    assert_eq!(stored["items"][0]["body"]["text"], "once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_is_retried_on_a_broken_connection() {
    let peer = misbehaving_peer(Misbehaviour::DieOnceThenAnswer).await;
    let client = client_for(&peer, 21).await;
    let (status, body) = client.request("GET", "/v1/health", None).await.unwrap();
    assert_eq!((status, &body["ok"]), (200, &json!(true)));
    assert_eq!(peer.requests(), 2);
    assert_eq!(peer.connections(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_cached_connection_is_redialled_for_a_write() {
    let peer = misbehaving_peer(Misbehaviour::AnswerThenClose).await;
    let client = client_for(&peer, 22).await;
    client.request("GET", "/v1/health", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (status, item) =
        client.request("POST", "/v1/items", Some(json!({"facet": "tasks", "body": {"text": "redialled"}}))).await.unwrap();
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["body"]["text"], "redialled");
    assert_eq!(peer.requests(), 2);
    assert_eq!(peer.connections(), 2);
    let (_, stored) = peer.core.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(stored["items"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_json_arrives_as_text() {
    let peer = misbehaving_peer(Misbehaviour::PassThrough).await;
    let client = client_for(&peer, 23).await;
    let (status, body) = client.request("GET", "/v1/items/not-a-uuid", None).await.unwrap();
    assert_eq!(status, 400);
    assert!(body.as_str().is_some_and(|text| text.contains("UUID")), "{body}");
}

// ---------------------------------------------------------------------
// The FFI safety net.
// ---------------------------------------------------------------------

/// A panic inside the FFI surface comes back as the same envelope every
/// other failure uses. Unwinding out of an `extern "system"` fn aborts the
/// process, so this is what keeps a bug in the client from killing the
/// app around it.
#[test]
fn a_panic_becomes_an_envelope() {
    let response = erisdb_client::blocking::guard(|| panic!("boom"), erisdb_client::blocking::panic_envelope);
    let v: Value = serde_json::from_str(&response).expect("the envelope is JSON");
    assert_eq!(v["ok"], false, "{response}");
    assert_eq!(v["status"], 0, "{response}");
    assert!(v["error"].as_str().unwrap().contains("boom"), "{response}");
}

/// Key hex is decoded by character, not by byte offset: whatever 64-byte
/// string the JVM hands over, the answer is data.
#[test]
fn key_hex_never_panics() {
    let decode = erisdb_client::blocking::decode_key_hex;
    assert_eq!(decode(&"0".repeat(64)), Some([0u8; 32]));
    assert_eq!(decode(&"AB".repeat(32)), Some([0xABu8; 32]), "uppercase hex decodes too");
    // 64 bytes, 22 characters: byte-slicing this lands inside a character.
    let multibyte = format!("{}0", "€".repeat(21));
    assert_eq!(multibyte.len(), 64);
    assert_eq!(decode(&multibyte), None);
    assert_eq!(decode(""), None);
    assert_eq!(decode(&"z".repeat(64)), None);
    assert_eq!(decode(&"0".repeat(63)), None);
}

/// What an app tells ErisAuth it is: the endpoint id its key dials as.
#[test]
fn a_key_names_the_endpoint_it_dials_as() {
    let key = [7u8; 32];
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(erisdb_client::blocking::endpoint_id(&hex), Some(SecretKey::from_bytes(&key).public().to_string()));
    assert_eq!(erisdb_client::blocking::endpoint_id("not hex"), None);
}
