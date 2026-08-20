//! End-to-end tests for the bezel v1 contract.
//!
//! Zero mocking: every test runs against a real Postgres (Docker via
//! testcontainers), a real bezel instance serving real HTTP on a real TCP
//! socket, and — for the transport test — a real Iroh QUIC connection.
//! Each test gets its own freshly-migrated database inside a shared
//! Postgres container.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::sync::OnceCell;

const SECRET: &[u8] = b"e2e-test-secret";

// ---------------------------------------------------------------- infra

struct Pg {
    _container: ContainerAsync<Postgres>,
    base_url: String, // postgres://postgres:postgres@host:port  (no db)
}

static PG: OnceCell<Pg> = OnceCell::const_new();

async fn pg() -> &'static Pg {
    PG.get_or_init(|| async {
        let container = Postgres::default().start().await.expect("start postgres");
        let port = container.get_host_port_ipv4(5432).await.expect("pg port");
        Pg {
            _container: container,
            base_url: format!("postgres://postgres:postgres@127.0.0.1:{port}"),
        }
    })
    .await
}

/// A fresh, migrated database in the shared container.
async fn fresh_pool() -> PgPool {
    let pg = pg().await;
    let db = format!("bezel_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{}/postgres", pg.base_url))
        .await
        .expect("admin connect");
    // db is a uuid we just generated; safe by construction.
    sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{db}""#)))
        .execute(&admin)
        .await
        .expect("create db");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("{}/{db}", pg.base_url))
        .await
        .expect("db connect");
    bezel::MIGRATOR.run(&pool).await.expect("migrate");
    pool
}

/// Serve a bezel core over real TCP on an ephemeral port; return its base URL.
async fn spawn_core(pool: PgPool) -> String {
    let app = bezel::app(pool, SECRET.to_vec());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Connect info feeds source.addr stamping.
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .unwrap();
    });
    format!("http://{addr}")
}

/// One fresh db + one core, plus a root capability token.
async fn setup() -> (String, String, PgPool) {
    let pool = fresh_pool().await;
    let url = spawn_core(pool.clone()).await;
    let root = bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), None).unwrap();
    (url, root, pool)
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    /// Sent as X-Bezel-Client; the server stamps it into source.client.
    client_name: Option<String>,
}

impl Client {
    fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.to_string(),
            token: token.to_string(),
            client_name: None,
        }
    }
    fn with_client(base: &str, token: &str, client_name: &str) -> Self {
        Self { client_name: Some(client_name.to_string()), ..Self::new(base, token) }
    }
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some(name) = &self.client_name {
            r = r.header("x-bezel-client", name);
        }
        r
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.req(reqwest::Method::POST, path).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.req(reqwest::Method::PUT, path).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn delete(&self, path: &str) -> u16 {
        self.req(reqwest::Method::DELETE, path).send().await.unwrap().status().as_u16()
    }
}

/// Register the canonical tasks facet used across tests.
async fn register_tasks_facet(c: &Client) {
    let (status, body) = c
        .post(
            "/v1/items",
            json!({
                "facet": "facet",
                "body": {
                    "name": "tasks/v1",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "required": ["title", "done"],
                        "properties": {
                            "title": {"type": "string", "minLength": 1},
                            "done": {"type": "boolean"},
                            "due": {"type": "string"}
                        },
                        "additionalProperties": false
                    }
                }
            }),
        )
        .await;
    assert_eq!(status, 201, "facet registration failed: {body}");
}

/// Register the canonical lists facet: the contract apps/lists/index.html
/// carries a copy of. An entry is `list` + `name`, optional description,
/// link, and a flat frontmatter-style attributes map. Timestamps live on
/// the item envelope (created_at / updated_at), never in the body.
async fn register_lists_facet(c: &Client) {
    let (status, body) = c
        .post(
            "/v1/items",
            json!({
                "facet": "facet",
                "body": {
                    "name": "lists/v1",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "required": ["list", "name"],
                        "properties": {
                            "list": {"type": "string", "minLength": 1},
                            "name": {"type": "string", "minLength": 1},
                            "description": {"type": "string"},
                            "link": {"type": "string"},
                            "attributes": {
                                "type": "object",
                                "additionalProperties": {
                                    "anyOf": [
                                        {"type": ["string", "number", "boolean", "null"]},
                                        {"type": "array"}
                                    ]
                                }
                            }
                        },
                        "additionalProperties": false
                    }
                }
            }),
        )
        .await;
    assert_eq!(status, 201, "lists facet registration failed: {body}");
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn health_needs_no_auth() {
    let (url, _root, _pool) = setup().await;
    let r = reqwest::get(format!("{url}/v1/health")).await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

#[tokio::test]
async fn missing_or_garbage_token_is_401() {
    let (url, _root, _pool) = setup().await;
    let http = reqwest::Client::new();
    let r = http.get(format!("{url}/v1/items?facet=tasks/v1")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let r = http
        .get(format!("{url}/v1/items?facet=tasks/v1"))
        .bearer_auth("bz1.not.real")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
}

#[tokio::test]
async fn facet_schemas_are_enforced() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_tasks_facet(&c).await;

    // Conforming item is accepted and returned with identity + revision.
    let (status, item) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "water plants", "done": false}}))
        .await;
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["facet"], "tasks/v1");
    assert_eq!(item["revision"], 1);
    assert!(item["id"].is_string());
    assert_eq!(item["body"]["title"], "water plants");

    // Nonconforming item is rejected with a schema violation.
    let (status, err) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "", "done": "nope"}}))
        .await;
    assert_eq!(status, 422);
    assert_eq!(err["error"], "schema_violation");

    // Writes to an unregistered facet are rejected.
    let (status, err) = c
        .post("/v1/items", json!({"facet": "nonexistent/v1", "body": {"x": 1}}))
        .await;
    assert_eq!(status, 422);
    assert_eq!(err["error"], "unknown_facet");

    // Facet definitions themselves are schema-checked (meta-facet).
    let (status, err) = c
        .post("/v1/items", json!({"facet": "facet", "body": {"nameless": true}}))
        .await;
    assert_eq!(status, 422);
    assert_eq!(err["error"], "schema_violation");

    // Duplicate facet names conflict.
    let (status, _) = c
        .post(
            "/v1/items",
            json!({"facet": "facet", "body": {"name": "tasks/v1", "schema": {"type": "object"}}}),
        )
        .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn lists_facet_contract_is_pinned() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_lists_facet(&c).await;

    // Minimal entry: list + name is enough.
    let (status, item) = c
        .post("/v1/items", json!({"facet": "lists/v1", "body": {"list": "books", "name": "Piranesi"}}))
        .await;
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["body"]["list"], "books");
    // Timestamps are the item envelope's, minted server-side.
    assert!(item["created_at"].is_string());
    assert!(item["updated_at"].is_string());

    // Full entry: description, link, and frontmatter-style attributes
    // (scalars and arrays, mixed types).
    let (status, full) = c
        .post(
            "/v1/items",
            json!({"facet": "lists/v1", "body": {
                "list": "books",
                "name": "A Memory Called Empire",
                "description": "Teixcalaan #1",
                "link": "https://en.wikipedia.org/wiki/A_Memory_Called_Empire",
                "attributes": {
                    "author": "Arkady Martine",
                    "rating": 5,
                    "read": true,
                    "tags": ["sf", "politics"],
                    "loaned_to": null
                }
            }}),
        )
        .await;
    assert_eq!(status, 201, "{full}");
    let id = full["id"].as_str().unwrap().to_string();

    // Editing bumps updated_at but never created_at; the client owns neither.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut body = full["body"].clone();
    body["attributes"]["rating"] = json!(4);
    let (status, edited) = c
        .put(&format!("/v1/items/{id}"), json!({"body": body, "revision": 1}))
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["created_at"], full["created_at"]);
    assert_ne!(edited["updated_at"], full["updated_at"]);

    // Rejected: missing name, missing list, empty list, unknown top-level
    // key, nested object inside attributes.
    for bad in [
        json!({"list": "books"}),
        json!({"name": "orphan"}),
        json!({"list": "", "name": "x"}),
        json!({"list": "books", "name": "x", "addDate": "2020-01-01"}),
        json!({"list": "books", "name": "x", "attributes": {"nested": {"deep": 1}}}),
    ] {
        let (status, err) = c.post("/v1/items", json!({"facet": "lists/v1", "body": bad})).await;
        assert_eq!(status, 422, "accepted invalid entry: {err}");
        assert_eq!(err["error"], "schema_violation");
    }
}

#[tokio::test]
async fn writes_carry_their_source() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    // A token minted with a user identity, a client announcing itself.
    let alice_token =
        bezel::auth::mint(SECRET, &["tasks/v1"], &["read", "write"], Some(3600), Some("alice")).unwrap();
    let alice = Client::with_client(&url, &alice_token, "Tests - Desktop v0");

    let (status, item) = alice
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "attributed", "done": false}}))
        .await;
    assert_eq!(status, 201, "{item}");
    // source = {addr: observed, user: signed into the token, client: claimed}.
    assert_eq!(item["source"]["user"], "alice");
    assert_eq!(item["source"]["client"], "Tests - Desktop v0");
    let addr = item["source"]["addr"].as_str().unwrap();
    assert!(!addr.is_empty(), "addr must be observed from the connection");
    let id = item["id"].as_str().unwrap().to_string();

    // A different writer overwrites the item's source: it names the last writer.
    let bot_token =
        bezel::auth::mint(SECRET, &["tasks/v1"], &["read", "write"], Some(3600), Some("agent-1")).unwrap();
    let bot = Client::with_client(&url, &bot_token, "Agent v0");
    let (status, edited) = bot
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "attributed", "done": true}, "revision": 1}))
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["source"]["user"], "agent-1");
    assert_eq!(edited["source"]["client"], "Agent v0");

    // A bare token and no client header: addr still observed, the rest null.
    let anon_token = bezel::auth::mint(SECRET, &["tasks/v1"], &["read", "write"], Some(3600), None).unwrap();
    let anon = Client::new(&url, &anon_token);
    let (_, plain) = anon
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "anon", "done": false}}))
        .await;
    assert_eq!(plain["source"]["user"], Value::Null);
    assert_eq!(plain["source"]["client"], Value::Null);
    assert!(plain["source"]["addr"].is_string());

    // The change feed is the audit log: every row carries the body snapshot
    // it produced and the source that produced it.
    let (_, feed) = admin.get("/v1/changes?since=0&facet=tasks/v1").await;
    let changes = feed["changes"].as_array().unwrap();
    let created = changes.iter().find(|ch| ch["op"] == "created" && ch["item_id"] == json!(id)).unwrap();
    assert_eq!(created["body"]["title"], "attributed");
    assert_eq!(created["source"]["user"], "alice");
    let updated = changes.iter().find(|ch| ch["op"] == "updated" && ch["item_id"] == json!(id)).unwrap();
    assert_eq!(updated["body"]["done"], true);
    assert_eq!(updated["source"]["user"], "agent-1");
    // Rows carry the revision they produced: a sync client can apply the
    // feed directly, no per-item refetch.
    assert_eq!(created["revision"], 1);
    assert_eq!(updated["revision"], 2);
}

#[tokio::test]
async fn history_is_kept_and_reverts_roll_forward() {
    let (url, root, _pool) = setup().await;
    let c = Client::with_client(&url, &root, "Tests v0");
    register_tasks_facet(&c).await;

    // Three states of one item.
    let (_, v1) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "draft", "done": false}}))
        .await;
    let id = v1["id"].as_str().unwrap().to_string();
    c.put(&format!("/v1/items/{id}"), json!({"body": {"title": "draft 2", "done": false}, "revision": 1}))
        .await;
    c.put(&format!("/v1/items/{id}"), json!({"body": {"title": "final", "done": true}, "revision": 2}))
        .await;

    // History: every state the item has been in, oldest first, with sources.
    let (status, hist) = c.get(&format!("/v1/items/{id}/history")).await;
    assert_eq!(status, 200, "{hist}");
    let rows = hist["history"].as_array().unwrap();
    let ops: Vec<&str> = rows.iter().map(|r| r["op"].as_str().unwrap()).collect();
    assert_eq!(ops, vec!["created", "updated", "updated"]);
    assert_eq!(rows[0]["body"]["title"], "draft");
    assert_eq!(rows[1]["body"]["title"], "draft 2");
    assert_eq!(rows[2]["body"]["title"], "final");
    assert!(rows.iter().all(|r| r["source"]["client"] == "Tests v0"));
    let first_seq = rows[0]["seq"].as_i64().unwrap();

    // Revert rolls FORWARD: the old body lands as a new revision, and the
    // feed shows it as an ordinary update. History is append-only.
    let (status, reverted) = c
        .post(&format!("/v1/items/{id}/revert"), json!({"seq": first_seq, "revision": 3}))
        .await;
    assert_eq!(status, 200, "{reverted}");
    assert_eq!(reverted["revision"], 4);
    assert_eq!(reverted["body"]["title"], "draft");
    let (_, hist) = c.get(&format!("/v1/items/{id}/history")).await;
    assert_eq!(hist["history"].as_array().unwrap().len(), 4);

    // A stale revision loses, exactly like any other write.
    let (status, err) = c
        .post(&format!("/v1/items/{id}/revert"), json!({"seq": first_seq, "revision": 3}))
        .await;
    assert_eq!(status, 409);
    assert_eq!(err["error"], "revision_conflict");

    // A seq that isn't a snapshot of this item is a bad request.
    let (status, _) = c
        .post(&format!("/v1/items/{id}/revert"), json!({"seq": 999_999, "revision": 4}))
        .await;
    assert_eq!(status, 400);

    // Deleted items keep their history; the last snapshot survives the delete.
    assert_eq!(c.delete(&format!("/v1/items/{id}")).await, 204);
    let (status, hist) = c.get(&format!("/v1/items/{id}/history")).await;
    assert_eq!(status, 200);
    let rows = hist["history"].as_array().unwrap();
    assert_eq!(rows.last().unwrap()["op"], "deleted");
    assert_eq!(rows.last().unwrap()["body"], Value::Null);
    assert_eq!(rows[rows.len() - 2]["body"]["title"], "draft");
    // But reverting a deleted item is a 404: recovery is a new create from history.
    let (status, _) = c
        .post(&format!("/v1/items/{id}/revert"), json!({"seq": first_seq, "revision": 4}))
        .await;
    assert_eq!(status, 404);

    // History of an unknown item is a 404.
    let (status, _) = c.get(&format!("/v1/items/{}/history", uuid::Uuid::new_v4())).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn capabilities_scope_facet_access() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    // Register a second facet with private data.
    let (status, _) = admin
        .post(
            "/v1/items",
            json!({"facet": "facet", "body": {"name": "exercise/v1", "schema": {"type": "object"}}}),
        )
        .await;
    assert_eq!(status, 201);
    let (status, secret_item) = admin
        .post("/v1/items", json!({"facet": "exercise/v1", "body": {"kind": "run", "km": 5}}))
        .await;
    assert_eq!(status, 201);
    let secret_id = secret_item["id"].as_str().unwrap();

    // A tasks-only token…
    let tasks_token = bezel::auth::mint(SECRET, &["tasks/v1"], &["read", "write"], Some(3600), None).unwrap();
    let tasks = Client::new(&url, &tasks_token);

    // …can use its own facet…
    let (status, _) = tasks
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "ok", "done": false}}))
        .await;
    assert_eq!(status, 201);
    let (status, _) = tasks.get("/v1/items?facet=tasks/v1").await;
    assert_eq!(status, 200);

    // …but literally cannot touch exercise data.
    let (status, _) = tasks
        .post("/v1/items", json!({"facet": "exercise/v1", "body": {"kind": "run", "km": 1}}))
        .await;
    assert_eq!(status, 403);
    let (status, _) = tasks.get("/v1/items?facet=exercise/v1").await;
    assert_eq!(status, 403);
    let (status, _) = tasks.get(&format!("/v1/items/{secret_id}")).await;
    assert_eq!(status, 403);
    // Nor register facets or tail the global change feed.
    let (status, _) = tasks
        .post("/v1/items", json!({"facet": "facet", "body": {"name": "sneaky/v1", "schema": {}}}))
        .await;
    assert_eq!(status, 403);
    let (status, _) = tasks.get("/v1/changes?since=0").await;
    assert_eq!(status, 403);
    // A facet-scoped change tail is fine.
    let (status, _) = tasks.get("/v1/changes?since=0&facet=tasks/v1").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn changes_feed_records_every_mutation_in_order() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_tasks_facet(&c).await;

    let (_, item) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "a", "done": false}}))
        .await;
    let id = item["id"].as_str().unwrap().to_string();
    let (status, updated) = c
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "a!", "done": true}, "revision": 1}))
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(c.delete(&format!("/v1/items/{id}")).await, 204);

    let (status, feed) = c.get("/v1/changes?since=0&facet=tasks/v1").await;
    assert_eq!(status, 200);
    let changes = feed["changes"].as_array().unwrap();
    let ops: Vec<&str> = changes.iter().map(|ch| ch["op"].as_str().unwrap()).collect();
    assert_eq!(ops, vec!["created", "updated", "deleted"]);
    // Seqs strictly increase and every row names the item.
    let seqs: Vec<i64> = changes.iter().map(|ch| ch["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    assert!(changes.iter().all(|ch| ch["item_id"] == json!(id)));
    // Cursor: `next` resumes past everything seen.
    let next = feed["next"].as_i64().unwrap();
    assert_eq!(next, *seqs.last().unwrap());
    let (_, tail) = c.get(&format!("/v1/changes?since={next}&facet=tasks/v1")).await;
    assert_eq!(tail["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn stale_revision_writes_conflict() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_tasks_facet(&c).await;
    let (_, item) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "x", "done": false}}))
        .await;
    let id = item["id"].as_str().unwrap().to_string();

    let (status, v2) = c
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "x2", "done": false}, "revision": 1}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(v2["revision"], 2);

    // A second writer still holding revision 1 loses.
    let (status, err) = c
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "x3", "done": false}, "revision": 1}))
        .await;
    assert_eq!(status, 409);
    assert_eq!(err["error"], "revision_conflict");
    // Updated bodies are still schema-checked.
    let (status, _) = c
        .put(&format!("/v1/items/{id}"), json!({"body": {"done": false}, "revision": 2}))
        .await;
    assert_eq!(status, 422);
}

#[tokio::test]
async fn two_stateless_replicas_share_one_store() {
    let pool = fresh_pool().await;
    let url_a = spawn_core(pool.clone()).await;
    let url_b = spawn_core(pool.clone()).await;
    let root = bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), None).unwrap();
    let a = Client::new(&url_a, &root);
    let b = Client::new(&url_b, &root);

    register_tasks_facet(&a).await;
    let (status, item) = a
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "via A", "done": false}}))
        .await;
    assert_eq!(status, 201);
    let id = item["id"].as_str().unwrap();

    // Replica B sees the item and the change immediately: no replica-local state.
    let (status, got) = b.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(got["body"]["title"], "via A");
    let (_, feed) = b.get("/v1/changes?since=0&facet=tasks/v1").await;
    assert_eq!(feed["changes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn sse_stream_delivers_live_changes() {
    use futures::StreamExt;
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_tasks_facet(&c).await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{url}/v1/changes/stream?since=0&facet=tasks/v1"))
        .bearer_auth(&root)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut stream = resp.bytes_stream();

    // Write an item after subscribing; the event must arrive without re-polling.
    let (status, item) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "live", "done": false}}))
        .await;
    assert_eq!(status, 201);
    let id = item["id"].as_str().unwrap().to_string();

    let deadline = tokio::time::Duration::from_secs(10);
    let mut buf = String::new();
    let received = tokio::time::timeout(deadline, async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if buf.contains(&id) && buf.contains("created") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(received, "no SSE change event within {deadline:?}; got: {buf}");
}

#[tokio::test]
async fn tick_lands_on_the_change_feed() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    let (status, tick) = c.post("/v1/tick", json!({})).await;
    assert_eq!(status, 200, "{tick}");
    let seq = tick["seq"].as_i64().unwrap();

    let (_, feed) = c.get(&format!("/v1/changes?since={}", seq - 1)).await;
    let changes = feed["changes"].as_array().unwrap();
    assert_eq!(changes[0]["op"], "tick");
    assert_eq!(changes[0]["facet"], "system");

    // Tick requires write on the system facet — a scoped token can't fire it.
    let tasks_token = bezel::auth::mint(SECRET, &["tasks/v1"], &["read", "write"], Some(3600), None).unwrap();
    let t = Client::new(&url, &tasks_token);
    let (status, _) = t.post("/v1/tick", json!({})).await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn tick_sweeps_declared_lapse_rules() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);

    // A facet that declares a lapse rule: due field, done field.
    let (status, body) = c
        .post(
            "/v1/items",
            json!({
                "facet": "facet",
                "body": {
                    "name": "tasks/v1",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "required": ["title", "done"],
                        "properties": {
                            "title": {"type": "string"},
                            "done": {"type": "boolean"},
                            "due": {"type": "string", "format": "date-time"}
                        }
                    },
                    "lapse": {"due": "due", "done": "done"}
                }
            }),
        )
        .await;
    assert_eq!(status, 201, "{body}");

    // One overdue task, one future task, one overdue-but-done task.
    let (_, overdue) = c
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "late", "done": false, "due": "2020-01-01T00:00:00Z"}}))
        .await;
    let overdue_id = overdue["id"].as_str().unwrap().to_string();
    c.post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "future", "done": false, "due": "2999-01-01T00:00:00Z"}}))
        .await;
    c.post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "done", "done": true, "due": "2020-01-01T00:00:00Z"}}))
        .await;

    // Tick: exactly the overdue, un-done task lapses.
    let (status, tick) = c.post("/v1/tick", json!({})).await;
    assert_eq!(status, 200, "{tick}");
    assert_eq!(tick["lapsed"], 1);
    let (_, feed) = c.get("/v1/changes?since=0&facet=tasks/v1").await;
    let lapses: Vec<&Value> = feed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|ch| ch["op"] == "lapsed")
        .collect();
    assert_eq!(lapses.len(), 1);
    assert_eq!(lapses[0]["item_id"], json!(overdue_id));

    // Re-poke: idempotent, nothing new fires.
    let (_, tick) = c.post("/v1/tick", json!({})).await;
    assert_eq!(tick["lapsed"], 0);

    // Completing the task keeps it quiet…
    let (status, item) = c
        .put(&format!("/v1/items/{overdue_id}"), json!({"body": {"title": "late", "done": true, "due": "2020-01-01T00:00:00Z"}, "revision": 1}))
        .await;
    assert_eq!(status, 200, "{item}");
    let (_, tick) = c.post("/v1/tick", json!({})).await;
    assert_eq!(tick["lapsed"], 0);

    // …but editing it back overdue re-arms the lapse.
    let (status, _) = c
        .put(&format!("/v1/items/{overdue_id}"), json!({"body": {"title": "late", "done": false, "due": "2020-01-01T00:00:00Z"}, "revision": 2}))
        .await;
    assert_eq!(status, 200);
    let (_, tick) = c.post("/v1/tick", json!({})).await;
    assert_eq!(tick["lapsed"], 1);
}

#[tokio::test]
async fn browser_clients_get_cors_headers() {
    let (url, _root, _pool) = setup().await;
    let http = reqwest::Client::new();
    let r = http
        .get(format!("{url}/v1/health"))
        .header("origin", "http://localhost:5173")
        .send()
        .await
        .unwrap();
    assert!(r.headers().contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn minting_is_delegated_and_bounded() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    // Admin mints a narrower token over HTTP.
    let (status, minted) = admin
        .post("/v1/capabilities", json!({"facets": ["tasks/v1"], "verbs": ["read"], "ttl_secs": 600}))
        .await;
    assert_eq!(status, 201, "{minted}");
    let token = minted["token"].as_str().unwrap();
    let reader = Client::new(&url, token);
    let (status, _) = reader.get("/v1/items?facet=tasks/v1").await;
    assert_eq!(status, 200);
    let (status, _) = reader
        .post("/v1/items", json!({"facet": "tasks/v1", "body": {"title": "no", "done": false}}))
        .await;
    assert_eq!(status, 403); // read-only

    // Non-admin tokens cannot mint at all.
    let (status, _) = reader
        .post("/v1/capabilities", json!({"facets": ["tasks/v1"], "verbs": ["read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);

    // A scoped admin cannot escalate beyond its own facets.
    let scoped_admin =
        bezel::auth::mint(SECRET, &["tasks/v1"], &["read", "write", "admin"], Some(3600), None).unwrap();
    let sa = Client::new(&url, &scoped_admin);
    let (status, _) = sa
        .post("/v1/capabilities", json!({"facets": ["*"], "verbs": ["read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);

    // Expired tokens are dead.
    let expired = bezel::auth::mint(SECRET, &["*"], &["read"], Some(-10), None).unwrap();
    let e = Client::new(&url, &expired);
    let (status, _) = e.get("/v1/items?facet=tasks/v1").await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn iroh_identity_is_derived_from_the_secret() -> Result<()> {
    // Same secret → same endpoint id, across restarts: clients hold one
    // address forever. Different secret → different identity.
    let a = bezel::net::endpoint(SECRET).await?;
    let id_a = a.id();
    a.close().await;
    let b = bezel::net::endpoint(SECRET).await?;
    assert_eq!(id_a, b.id(), "endpoint id must survive a restart");
    b.close().await;
    let other = bezel::net::endpoint(b"a different secret").await?;
    assert_ne!(id_a, other.id(), "different deployments must not share an identity");
    other.close().await;
    Ok(())
}

#[tokio::test]
async fn the_core_speaks_http_over_iroh() -> Result<()> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;

    let pool = fresh_pool().await;
    let app = bezel::app(pool, SECRET.to_vec());
    let server_ep = bezel::net::endpoint(SECRET).await?;
    let server_addr = bezel::net::advertised_addr(&server_ep).await?;
    tokio::spawn(bezel::net::serve(server_ep, app));

    // The client is its own endpoint with its own identity.
    let client_ep = bezel::net::endpoint(b"client-side-secret").await?;
    let conn = client_ep.connect(server_addr, bezel::net::ALPN).await?;
    let root = bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), None).unwrap();

    // One HTTP/1.1 exchange per QUIC bi-stream.
    async fn request(
        conn: &iroh::endpoint::Connection,
        req: hyper::Request<Full<Bytes>>,
    ) -> Result<(u16, Value)> {
        let (send, recv) = conn.open_bi().await?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(driver);
        let resp = sender.send_request(req).await?;
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await?.to_bytes();
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    let (status, _) = request(
        &conn,
        hyper::Request::get("/v1/health").header("host", "bezel").body(Full::default())?,
    )
    .await?;
    assert_eq!(status, 200);

    let facet_req = hyper::Request::post("/v1/items")
        .header("host", "bezel")
        .header("authorization", format!("Bearer {root}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            json!({"facet": "facet", "body": {"name": "tasks/v1", "schema": {"type": "object"}}}).to_string(),
        )))?;
    let (status, body) = request(&conn, facet_req).await?;
    assert_eq!(status, 201, "{body}");

    let item_req = hyper::Request::post("/v1/items")
        .header("host", "bezel")
        .header("authorization", format!("Bearer {root}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            json!({"facet": "tasks/v1", "body": {"title": "over quic", "done": false}}).to_string(),
        )))?;
    let (status, item) = request(&conn, item_req).await?;
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["body"]["title"], "over quic");
    // Over Iroh, source.addr is the remote endpoint id — a cryptographic
    // identity, stamped by the transport.
    let addr = item["source"]["addr"].as_str().unwrap();
    assert!(addr.starts_with("iroh:"), "expected iroh:<endpoint id>, got {addr}");
    assert!(addr.contains(&client_ep.id().to_string()), "addr should name the caller: {addr}");
    Ok(())
}
