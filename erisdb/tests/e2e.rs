//! End-to-end tests for the erisdb v1 contract.
//!
//! Zero mocking: every test runs against a real Postgres (Docker via
//! testcontainers), a real erisdb instance serving real HTTP on a real TCP
//! socket, and — for the transport test — a real Iroh QUIC connection.
//! Each test gets its own freshly-migrated database inside a shared
//! Postgres container.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    fresh_pool_with(&erisdb::MIGRATOR).await
}

async fn fresh_pool_with(migrations: &sqlx::migrate::Migrator) -> PgPool {
    let pg = pg().await;
    let db = format!("erisdb_{}", uuid::Uuid::new_v4().simple());
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
    migrations.run(&pool).await.expect("migrate");
    pool
}

/// Serve an ErisDB core over real TCP on an ephemeral port; return its base URL.
async fn spawn_core(pool: PgPool) -> String {
    let app = erisdb::app(pool, SECRET.to_vec());
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
    let root = erisdb::auth::mint(SECRET, &["*"], Some(3600), None).unwrap();
    (url, root, pool)
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    /// Sent as X-Bezel-Client; the server stamps it into source.client.
    client_name: Option<String>,
    proof: String,
}

impl Client {
    fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.to_string(),
            token: token.to_string(),
            client_name: None,
            proof: format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()),
        }
    }
    fn with_client(base: &str, token: &str, client_name: &str) -> Self {
        Self { client_name: Some(client_name.to_string()), ..Self::new(base, token) }
    }
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token)
            .header("X-ErisDB-Client-Proof", &self.proof);
        if let Some(name) = &self.client_name {
            r = r.header("x-bezel-client", name);
        }
        r
    }
    async fn post(&self, path: &str, mut body: Value) -> (u16, Value) {
        if path == "/v1/pair/redeem" && body.get("challenge").is_none() {
            use base64::Engine;
            use sha2::Digest;
            body["challenge"] = json!(base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(self.proof.as_bytes())));
        }
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
                    "name": "tasks",
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
                    "name": "lists",
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

async fn register_browser(operator: &Client, requested: &[&str], ttl: i64) -> (Client, String) {
    let (status, cut) = operator.post("/v1/pairings", json!({})).await;
    assert_eq!(status, 201, "{cut}");
    let id = cut["id"].as_str().unwrap().to_string();
    let mut browser = Client::new(&operator.base, cut["secret"].as_str().unwrap());
    let (status, asked) = browser.post("/v1/pair/redeem", json!({"client": "Browser", "requested": requested})).await;
    assert_eq!(status, 200, "{asked}");
    let (status, approved) = operator.post(&format!("/v1/pairings/{id}/approve"), json!({"ttl_secs": ttl})).await;
    assert_eq!(status, 200, "{approved}");
    let (status, collected) = browser.get("/v1/pair/status").await;
    assert_eq!(status, 200, "{collected}");
    assert_eq!(collected["client_id"], id);
    browser.token = collected["token"].as_str().unwrap().to_string();
    (browser, id)
}

#[tokio::test]
async fn registered_browsers_renew_after_access_expiry_until_revoked() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (mut browser, id) = register_browser(&operator, &["tasks:read"], 1).await;
    let original = browser.token.clone();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(browser.get("/v1/permissions").await.0, 401);
    let (status, renewed) = browser.post(&format!("/v1/clients/{id}/refresh"), json!({})).await;
    assert_eq!(status, 200, "{renewed}");
    browser.token = renewed["token"].as_str().unwrap().into();
    assert_ne!(browser.token, original);
    assert_eq!(browser.get("/v1/permissions").await.0, 200);
    let cap = erisdb::auth::verify(SECRET, &browser.token).unwrap();
    assert!(cap.max_exp.is_none(), "an unasked-for chain deadline forces re-pairing");

    assert_eq!(operator.post(&format!("/v1/clients/{id}/revoke"), json!({})).await.0, 200);
    assert_eq!(browser.get("/v1/permissions").await.0, 401);
    assert_eq!(browser.post(&format!("/v1/clients/{id}/refresh"), json!({})).await.0, 401);
    assert_eq!(operator.post(&format!("/v1/clients/{id}/revoke"), json!({})).await.0, 200, "revocation is idempotent");
}

#[tokio::test]
async fn revocation_reaches_other_replicas_and_closes_idle_subscriptions() {
    use futures::StreamExt;
    let (url, root, pool) = setup().await;
    let operator = Client::new(&url, &root);
    register_tasks_facet(&operator).await;
    let (mut browser, id) = register_browser(&operator, &["tasks:read"], 600).await;
    browser.base = spawn_core(pool).await;
    let response = browser.req(reqwest::Method::GET, "/v1/changes/stream?since=0&facet=tasks")
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();
    assert_eq!(browser.get("/v1/items?facet=tasks").await.0, 200);
    assert_eq!(operator.post(&format!("/v1/clients/{id}/revoke"), json!({})).await.0, 200);
    assert_eq!(browser.get("/v1/items?facet=tasks").await.0, 401);
    assert_eq!(browser.post(&format!("/v1/clients/{id}/refresh"), json!({})).await.0, 401);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(chunk) = stream.next().await {
            assert!(!String::from_utf8_lossy(&chunk.unwrap()).contains("event: change"));
        }
    }).await.expect("revocation must close an idle stream");
}

#[tokio::test]
async fn client_permissions_apply_immediately_and_cannot_exceed_the_request() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    register_tasks_facet(&operator).await;
    let (browser, id) = register_browser(&operator, &["tasks:*"], 600).await;
    let (_, record) = operator.get(&format!("/v1/clients/{id}")).await;
    let revision = record["revision"].as_i64().unwrap();
    assert_eq!(operator.put(&format!("/v1/clients/{id}"), json!({"grants": ["*"], "revision": revision})).await.0, 403);
    assert_eq!(browser.put(&format!("/v1/clients/{id}"), json!({"grants": ["*"], "revision": revision})).await.0, 403);
    assert_eq!(browser.get("/v1/clients").await.0, 403);
    let (status, changed) = operator.put(&format!("/v1/clients/{id}"), json!({"grants": ["tasks:read"], "revision": revision})).await;
    assert_eq!(status, 200, "{changed}");
    assert_eq!(operator.put(&format!("/v1/clients/{id}"), json!({"grants": ["tasks:create"], "revision": revision})).await.0, 409);
    let (_, permissions) = browser.get("/v1/permissions").await;
    assert_eq!(permissions["grants"], json!(["tasks:read"]));
    assert_eq!(browser.post("/v1/items", json!({"facet": "tasks", "body": {"title": "no", "done": false}})).await.0, 403);
    let (status, renewed) = browser.post(&format!("/v1/clients/{id}/refresh"), json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(renewed["grants"], json!(["tasks:read"]));
}

#[tokio::test]
async fn delegated_tokens_do_not_escape_their_installations_revocation() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (browser, id) = register_browser(&operator, &["tasks:read", "meta:capabilities:mint"], 600).await;
    let (status, child) = browser.post("/v1/capabilities", json!({"grants": ["tasks:read"], "ttl_secs": 60})).await;
    assert_eq!(status, 201, "{child}");
    let child = Client::new(&url, child["token"].as_str().unwrap());
    assert_eq!(child.get("/v1/permissions").await.0, 200);
    assert_eq!(child.post(&format!("/v1/clients/{id}/refresh"), json!({})).await.0, 401, "a delegated bearer cannot renew full installation authority");
    assert_eq!(operator.post(&format!("/v1/clients/{id}/revoke"), json!({})).await.0, 200);
    assert_eq!(child.get("/v1/permissions").await.0, 401);
    assert_eq!(child.post("/v1/capabilities/refresh", json!({"ttl_secs": 60})).await.0, 401);
}

#[tokio::test]
async fn browser_renewal_requires_the_secret_and_never_persists_it() {
    let (url, root, pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (browser, id) = register_browser(&operator, &["tasks:read"], 600).await;
    let thief = Client::new(&url, &browser.token);
    assert_eq!(thief.post(&format!("/v1/clients/{id}/refresh"), json!({})).await.0, 401);
    let rows: Vec<String> = sqlx::query_scalar("SELECT row_to_json(c)::text FROM clients c")
        .fetch_all(&pool).await.unwrap();
    let (_, feed) = operator.get("/v1/changes?since=0&limit=5000").await;
    let persisted = format!("{rows:?}{feed}");
    assert!(!persisted.contains(&browser.proof), "renewal secret reached persistent state");
    assert!(!persisted.contains(&browser.token), "access token reached persistent state");
}

#[tokio::test]
async fn pairing_rejects_scope_expansion_and_malformed_browser_proofs() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());
    for challenge in [json!(null), json!(""), json!("a".repeat(43)), json!("untrusted")] {
        let (status, _) = scanner.post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"], "challenge": challenge})).await;
        assert_eq!(status, 400);
    }
    assert_eq!(scanner.post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"]})).await.0, 200);
    let route = format!("/v1/pairings/{}/approve", cut["id"].as_str().unwrap());
    assert_eq!(operator.post(&route, json!({"granted": ["*"]})).await.0, 403);
    assert_eq!(operator.post(&route, json!({"granted": ["tasks:create"]})).await.0, 403);
    assert_eq!(operator.post(&route, json!({})).await.0, 200);
}

#[tokio::test]
async fn concurrent_collection_registers_one_client_and_recovers_both_responses() {
    let (url, root, pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());
    assert_eq!(scanner.post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"]})).await.0, 200);
    assert_eq!(operator.post(&format!("/v1/pairings/{}/approve", cut["id"].as_str().unwrap()), json!({})).await.0, 200);
    let (a, b) = tokio::join!(scanner.get("/v1/pair/status"), scanner.get("/v1/pair/status"));
    for (status, result) in [a, b] {
        assert_eq!(status, 200, "{result}");
        assert!(result["token"].is_string());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM clients").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn missing_or_garbage_token_is_401() {
    let (url, _root, _pool) = setup().await;
    let http = reqwest::Client::new();
    let r = http.get(format!("{url}/v1/items?facet=tasks")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let r = http
        .get(format!("{url}/v1/items?facet=tasks"))
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
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "water plants", "done": false}}))
        .await;
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["facet"], "tasks");
    assert_eq!(item["revision"], 1);
    assert!(item["id"].is_string());
    assert_eq!(item["body"]["title"], "water plants");

    // Nonconforming item is rejected with a schema violation.
    let (status, err) = c
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "", "done": "nope"}}))
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
            json!({"facet": "facet", "body": {"name": "tasks", "schema": {"type": "object"}}}),
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
        .post("/v1/items", json!({"facet": "lists", "body": {"list": "books", "name": "Piranesi"}}))
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
            json!({"facet": "lists", "body": {
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
        let (status, err) = c.post("/v1/items", json!({"facet": "lists", "body": bad})).await;
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
        erisdb::auth::mint(SECRET, &["tasks:*"], Some(3600), Some("alice")).unwrap();
    let alice = Client::with_client(&url, &alice_token, "Tests - Desktop v0");

    let (status, item) = alice
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "attributed", "done": false}}))
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
        erisdb::auth::mint(SECRET, &["tasks:*"], Some(3600), Some("agent-1")).unwrap();
    let bot = Client::with_client(&url, &bot_token, "Agent v0");
    let (status, edited) = bot
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "attributed", "done": true}, "revision": 1}))
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["source"]["user"], "agent-1");
    assert_eq!(edited["source"]["client"], "Agent v0");

    // A bare token and no client header: addr still observed, the rest null.
    let anon_token = erisdb::auth::mint(SECRET, &["tasks:*"], Some(3600), None).unwrap();
    let anon = Client::new(&url, &anon_token);
    let (_, plain) = anon
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "anon", "done": false}}))
        .await;
    assert_eq!(plain["source"]["user"], Value::Null);
    assert_eq!(plain["source"]["client"], Value::Null);
    assert!(plain["source"]["addr"].is_string());

    // The change feed is the audit log: every row carries the body snapshot
    // it produced and the source that produced it.
    let (_, feed) = admin.get("/v1/changes?since=0&facet=tasks").await;
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
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "draft", "done": false}}))
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
            json!({"facet": "facet", "body": {"name": "exercise", "schema": {"type": "object"}}}),
        )
        .await;
    assert_eq!(status, 201);
    let (status, secret_item) = admin
        .post("/v1/items", json!({"facet": "exercise", "body": {"kind": "run", "km": 5}}))
        .await;
    assert_eq!(status, 201);
    let secret_id = secret_item["id"].as_str().unwrap();

    // A tasks-only token…
    let tasks_token = erisdb::auth::mint(SECRET, &["tasks:*"], Some(3600), None).unwrap();
    let tasks = Client::new(&url, &tasks_token);

    // …can use its own facet…
    let (status, _) = tasks
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "ok", "done": false}}))
        .await;
    assert_eq!(status, 201);
    let (status, _) = tasks.get("/v1/items?facet=tasks").await;
    assert_eq!(status, 200);

    // …but literally cannot touch exercise data.
    let (status, _) = tasks
        .post("/v1/items", json!({"facet": "exercise", "body": {"kind": "run", "km": 1}}))
        .await;
    assert_eq!(status, 403);
    let (status, _) = tasks.get("/v1/items?facet=exercise").await;
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
    let (status, _) = tasks.get("/v1/changes?since=0&facet=tasks").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn changes_feed_records_every_mutation_in_order() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_tasks_facet(&c).await;

    let (_, item) = c
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "a", "done": false}}))
        .await;
    let id = item["id"].as_str().unwrap().to_string();
    let (status, updated) = c
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "a!", "done": true}, "revision": 1}))
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(c.delete(&format!("/v1/items/{id}")).await, 204);

    let (status, feed) = c.get("/v1/changes?since=0&facet=tasks").await;
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
    let (_, tail) = c.get(&format!("/v1/changes?since={next}&facet=tasks")).await;
    assert_eq!(tail["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn stale_revision_writes_conflict() {
    let (url, root, _pool) = setup().await;
    let c = Client::new(&url, &root);
    register_tasks_facet(&c).await;
    let (_, item) = c
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "x", "done": false}}))
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
    let root = erisdb::auth::mint(SECRET, &["*"], Some(3600), None).unwrap();
    let a = Client::new(&url_a, &root);
    let b = Client::new(&url_b, &root);

    register_tasks_facet(&a).await;
    let (status, item) = a
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "via A", "done": false}}))
        .await;
    assert_eq!(status, 201);
    let id = item["id"].as_str().unwrap();

    // Replica B sees the item and the change immediately: no replica-local state.
    let (status, got) = b.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(got["body"]["title"], "via A");
    let (_, feed) = b.get("/v1/changes?since=0&facet=tasks").await;
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
        .get(format!("{url}/v1/changes/stream?since=0&facet=tasks"))
        .bearer_auth(&root)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut stream = resp.bytes_stream();

    // Write an item after subscribing; the event must arrive without re-polling.
    let (status, item) = c
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "live", "done": false}}))
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
    let tasks_token = erisdb::auth::mint(SECRET, &["tasks:*"], Some(3600), None).unwrap();
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
                    "name": "tasks",
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
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "late", "done": false, "due": "2020-01-01T00:00:00Z"}}))
        .await;
    let overdue_id = overdue["id"].as_str().unwrap().to_string();
    c.post("/v1/items", json!({"facet": "tasks", "body": {"title": "future", "done": false, "due": "2999-01-01T00:00:00Z"}}))
        .await;
    c.post("/v1/items", json!({"facet": "tasks", "body": {"title": "done", "done": true, "due": "2020-01-01T00:00:00Z"}}))
        .await;

    // Tick: exactly the overdue, un-done task lapses.
    let (status, tick) = c.post("/v1/tick", json!({})).await;
    assert_eq!(status, 200, "{tick}");
    assert_eq!(tick["lapsed"], 1);
    let (_, feed) = c.get("/v1/changes?since=0&facet=tasks").await;
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
        .post("/v1/capabilities", json!({"grants": ["tasks:read"], "ttl_secs": 600}))
        .await;
    assert_eq!(status, 201, "{minted}");
    let token = minted["token"].as_str().unwrap();
    let reader = Client::new(&url, token);
    let (status, _) = reader.get("/v1/items?facet=tasks").await;
    assert_eq!(status, 200);
    let (status, _) = reader
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "no", "done": false}}))
        .await;
    assert_eq!(status, 403); // read-only

    // Non-admin tokens cannot mint at all.
    let (status, _) = reader
        .post("/v1/capabilities", json!({"grants": ["tasks:read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);

    // A scoped admin cannot escalate beyond its own facets.
    let scoped_admin =
        erisdb::auth::mint(SECRET, &["tasks:*", "meta:capabilities:mint"], Some(3600), None).unwrap();
    let sa = Client::new(&url, &scoped_admin);
    let (status, _) = sa
        .post("/v1/capabilities", json!({"grants": ["*:read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);

    // Expired tokens are dead.
    let expired = erisdb::auth::mint(SECRET, &["*:read"], Some(-10), None).unwrap();
    let e = Client::new(&url, &expired);
    let (status, _) = e.get("/v1/items?facet=tasks").await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn refresh_extends_expiry_without_touching_scope() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    // A plain, non-admin app token near the end of its life.
    let old = erisdb::auth::mint(SECRET, &["tasks:*"], Some(30), Some("alice")).unwrap();
    let old_cap = erisdb::auth::verify(SECRET, &old).unwrap();
    let app = Client::new(&url, &old);

    // Any valid token can refresh itself — no admin verb required.
    let (status, body) = app.post("/v1/capabilities/refresh", json!({"ttl_secs": 600})).await;
    assert_eq!(status, 201, "{body}");
    let fresh = body["token"].as_str().unwrap();

    // Same scope, same signed user, later expiry. Nothing widens, nothing
    // narrows: refresh moves time, not privilege.
    let cap = erisdb::auth::verify(SECRET, fresh).unwrap();
    assert_eq!(cap.grants, old_cap.grants);
    assert_eq!(cap.user, old_cap.user);
    assert!(cap.exp.unwrap() > old_cap.exp.unwrap());

    // The fresh token works, and carries the same limits.
    let c = Client::new(&url, fresh);
    let (status, _) = c
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "via fresh", "done": false}}))
        .await;
    assert_eq!(status, 201);
    let (status, _) = c
        .post("/v1/capabilities", json!({"grants": ["tasks:read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403); // still no admin

    // An expired token is dead for refresh too: refresh keeps sessions
    // alive, it does not resurrect them.
    let expired = erisdb::auth::mint(SECRET, &["tasks:read"], Some(-10), None).unwrap();
    let e = Client::new(&url, &expired);
    let (status, _) = e.post("/v1/capabilities/refresh", json!({"ttl_secs": 600})).await;
    assert_eq!(status, 401);

    // A never-expiring token refreshes into a bounded one if asked.
    let eternal = erisdb::auth::mint(SECRET, &["tasks:read"], None, None).unwrap();
    let et = Client::new(&url, &eternal);
    let (status, body) = et.post("/v1/capabilities/refresh", json!({"ttl_secs": 60})).await;
    assert_eq!(status, 201, "{body}");
    let cap = erisdb::auth::verify(SECRET, body["token"].as_str().unwrap()).unwrap();
    assert!(cap.exp.is_some());
}

/// Delegation must not launder a deadline away. A token that dies in a
/// minute is a minute of authority, including everything it hands out.
#[tokio::test]
async fn a_short_lived_admin_cannot_mint_a_longer_lived_token() {
    let (url, _root, _pool) = setup().await;
    let brief = erisdb::auth::mint_chain(SECRET, &["*"], Some(60), Some(60), None).unwrap();
    let admin = Client::new(&url, &brief);

    // The escalation: omit the lifetime and take a token that never dies.
    let (status, body) =
        admin.post("/v1/capabilities", json!({"grants": ["*:read"]})).await;
    assert_ne!(status, 201, "minted a token with no lifetime: {body}");

    // The same escalation spelled with a number.
    let (status, body) = admin
        .post("/v1/capabilities", json!({"grants": ["*:read"], "ttl_secs": 86_400}))
        .await;
    assert_eq!(status, 403, "minted a token outliving its parent: {body}");

    // And by way of the refresh chain rather than the expiry.
    let (status, body) = admin
        .post(
            "/v1/capabilities",
            json!({"grants": ["*:read"], "ttl_secs": 30, "max_ttl_secs": 86_400}),
        )
        .await;
    assert_eq!(status, 403, "minted a chain outliving its parent: {body}");

    // Inside its own life, delegation still works.
    let (status, body) = admin
        .post("/v1/capabilities", json!({"grants": ["*:read"], "ttl_secs": 30}))
        .await;
    assert_eq!(status, 201, "{body}");
}

/// Refresh moves time inside a fixed window. Without the ceiling a leaked
/// token renews itself forever and "revocation is expiry" means nothing.
#[tokio::test]
async fn refresh_cannot_outrun_the_chain() {
    let (url, _root, _pool) = setup().await;
    // Lives 30s, may keep renewing for 120s, and no longer.
    let token =
        erisdb::auth::mint_chain(SECRET, &["tasks:read"], Some(30), Some(120), None).unwrap();
    let chain_end = erisdb::auth::verify(SECRET, &token).unwrap().deadline().unwrap();

    let app = Client::new(&url, &token);
    let (status, body) = app.post("/v1/capabilities/refresh", json!({"ttl_secs": 86_400})).await;
    assert_eq!(status, 201, "{body}");

    let fresh = erisdb::auth::verify(SECRET, body["token"].as_str().unwrap()).unwrap();
    assert_eq!(fresh.deadline().unwrap(), chain_end, "refresh moved the ceiling");
    assert!(fresh.exp.unwrap() <= chain_end, "refresh reached past the ceiling");

    // Refreshing the refreshed token does not walk the ceiling forward either.
    let again = Client::new(&url, body["token"].as_str().unwrap());
    let (status, body) = again.post("/v1/capabilities/refresh", json!({"ttl_secs": 86_400})).await;
    assert_eq!(status, 201, "{body}");
    let twice = erisdb::auth::verify(SECRET, body["token"].as_str().unwrap()).unwrap();
    assert_eq!(twice.deadline().unwrap(), chain_end, "the chain grew by being used");
}

/// A token whose chain has run out is finished, even if its own expiry is
/// still in the future — that is what ends the line.
#[tokio::test]
async fn a_token_past_its_chain_is_refused() {
    let (url, _root, _pool) = setup().await;
    let now = chrono::Utc::now().timestamp();
    let stale = erisdb::auth::mint_capability(
        SECRET,
        &erisdb::auth::Capability {
            grants: vec!["tasks:read".into()],
            exp: Some(now + 3600),
            user: None,
            max_exp: Some(now - 1),
            pair: None,
            client: None,
        },
    )
    .unwrap();
    let c = Client::new(&url, &stale);
    let (status, _) = c.get("/v1/items?facet=tasks").await;
    assert_eq!(status, 401);
    let (status, _) = c.post("/v1/capabilities/refresh", json!({"ttl_secs": 60})).await;
    assert_eq!(status, 401);
}

/// A facet schema is data the core later runs. A `$ref` out of the document
/// asks it to go and fetch something on every write to that facet.
#[tokio::test]
async fn a_facet_schema_cannot_point_out_of_the_document() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);

    for hostile in [
        json!({"$ref": "http://169.254.169.254/latest/meta-data/"}),
        json!({"$ref": "file:///etc/passwd"}),
        json!({"type": "object", "properties": {"x": {"$ref": "https://example.com/s.json"}}}),
        json!({"type": "object", "$defs": {"a": {"$ref": "file:///etc/hosts"}}}),
    ] {
        let (status, body) = admin
            .post(
                "/v1/items",
                json!({"facet": "facet", "body": {"name": "evil/v1", "schema": hostile}}),
            )
            .await;
        assert_eq!(status, 400, "registered a schema reaching outside: {body}");
    }

    // A local ref is the normal way to factor a schema and still works.
    let (status, body) = admin
        .post(
            "/v1/items",
            json!({"facet": "facet", "body": {"name": "local", "schema": {
                "type": "object",
                "properties": {"who": {"$ref": "#/$defs/name"}},
                "$defs": {"name": {"type": "string", "minLength": 1}}
            }}}),
        )
        .await;
    assert_eq!(status, 201, "{body}");

    let (status, _) = admin
        .post("/v1/items", json!({"facet": "local", "body": {"who": "ok"}}))
        .await;
    assert_eq!(status, 201);
    let (status, _) = admin.post("/v1/items", json!({"facet": "local", "body": {"who": ""}})).await;
    assert_eq!(status, 422, "the local ref must actually be enforced");
}

/// Items sharing an `updated_at` come back in one fixed order.
///
/// `updated_since` paging is a timestamp cursor, so a tie group is only
/// safe to split across pages if the order within it never moves. Ties are
/// near-impossible in practice — each write is its own transaction with its
/// own `now()` — but "near-impossible" and "ordered" are different claims,
/// and a client paging a store it did not write cannot tell them apart.
#[tokio::test]
async fn items_sharing_a_timestamp_have_a_stable_order() {
    let (url, root, pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    for i in 0..12 {
        let (status, body) = admin
            .post(
                "/v1/items",
                json!({"facet": "tasks", "body": {"title": format!("tie {i}"), "done": false}}),
            )
            .await;
        assert_eq!(status, 201, "{body}");
    }
    // Collapse them onto one instant, which is the case the tiebreaker is for.
    sqlx::query("UPDATE items SET updated_at = now() WHERE facet = 'tasks'")
        .execute(&pool)
        .await
        .unwrap();

    let mut orders = Vec::new();
    for _ in 0..4 {
        let (status, page) = admin.get("/v1/items?facet=tasks&limit=1000").await;
        assert_eq!(status, 200, "{page}");
        let ids: Vec<String> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect();
        orders.push(ids);
    }
    assert!(orders.windows(2).all(|w| w[0] == w[1]), "tied items came back in a shifting order");

    // And paging through the tie group reaches every one of them.
    let mut seen = std::collections::HashSet::new();
    for page_start in (0..12).step_by(5) {
        let (_, page) = admin.get("/v1/items?facet=tasks&limit=1000").await;
        for item in page["items"].as_array().unwrap().iter().skip(page_start).take(5) {
            seen.insert(item["id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(seen.len(), 12);
}

/// A cursor walking the feed while writers commit sees every write.
///
/// This pins the property, not the mechanism. The gap it guards against —
/// `seq` handed out at INSERT, rows visible at COMMIT, so a reader that sees
/// a later `seq` steps permanently over an earlier one still in flight —
/// needs the reader to poll inside a window this harness cannot reliably
/// hit; the writers finish first. The append lock closes it by construction
/// instead, by making seq order and commit order the same order.
#[tokio::test]
async fn concurrent_writes_leave_no_gap_in_the_feed() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    // The reader has to be running *while* the writers commit. A cursor
    // that only walks a settled feed can never observe the gap: the hole
    // opens between one transaction taking a seq and another committing
    // ahead of it, and it closes as soon as both have landed.
    const WRITERS: usize = 40;
    let done = Arc::new(AtomicBool::new(false));

    let reader = {
        let (url, root, done) = (url.clone(), root.clone(), done.clone());
        tokio::spawn(async move {
            let c = Client::new(&url, &root);
            let mut cursor = 0i64;
            let mut seen = std::collections::HashSet::new();
            loop {
                // Sampled before draining, so a change that lands mid-drain
                // still gets one more pass and the reader is never blamed
                // for writes that had not happened yet.
                let finished = done.load(Ordering::SeqCst);
                loop {
                    let (status, page) =
                        c.get(&format!("/v1/changes?since={cursor}&facet=tasks&limit=3")).await;
                    assert_eq!(status, 200, "{page}");
                    let rows = page["changes"].as_array().unwrap();
                    let empty = rows.is_empty();
                    for row in rows {
                        if row["op"] == "created" {
                            seen.insert(row["body"]["title"].as_str().unwrap().to_string());
                        }
                    }
                    cursor = page["next"].as_i64().unwrap();
                    if empty {
                        break;
                    }
                }
                if finished {
                    return seen;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
    };

    let mut writes = Vec::new();
    for i in 0..WRITERS {
        let c = Client::new(&url, &root);
        writes.push(tokio::spawn(async move {
            c.post(
                "/v1/items",
                json!({"facet": "tasks", "body": {"title": format!("t{i}"), "done": false}}),
            )
            .await
        }));
    }
    for w in writes {
        let (status, body) = w.await.unwrap();
        assert_eq!(status, 201, "{body}");
    }
    done.store(true, Ordering::SeqCst);

    let seen = reader.await.unwrap();
    assert_eq!(seen.len(), WRITERS, "the feed lost writes a live cursor walked past");
}

/// Overlapping pokes are the normal case — a timer fires while the last one
/// is still running. An item must still lapse once per edit, not once per
/// poke that happens to be in flight.
#[tokio::test]
async fn overlapping_ticks_lapse_an_item_once() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    let (status, body) = admin
        .post(
            "/v1/items",
            json!({"facet": "facet", "body": {
                "name": "tasks",
                "strict": true,
                "schema": {
                    "type": "object",
                    "required": ["title", "done"],
                    "properties": {
                        "title": {"type": "string"},
                        "done": {"type": "boolean"},
                        "due": {"type": "string"}
                    }
                },
                "lapse": {"due": "due", "done": "done"}
            }}),
        )
        .await;
    assert_eq!(status, 201, "{body}");

    // Enough overdue items that a sweep takes real time, and enough pokes
    // to land inside one another. Two sweeps that each check "has this
    // lapsed already?" before either commits both answer no.
    const OVERDUE: usize = 60;
    const POKES: usize = 12;
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    for i in 0..OVERDUE {
        let (status, item) = admin
            .post(
                "/v1/items",
                json!({"facet": "tasks", "body": {
                    "title": format!("overdue {i}"), "done": false, "due": past
                }}),
            )
            .await;
        assert_eq!(status, 201, "{item}");
    }

    // Warm each connection first, then release every poke at once: the
    // race needs the sweeps genuinely inside one another, and a lazily
    // dialled client staggers them by more than a sweep takes.
    let gate = Arc::new(tokio::sync::Barrier::new(POKES));
    let mut pokes = Vec::new();
    for _ in 0..POKES {
        let c = Client::new(&url, &root);
        let gate = gate.clone();
        c.get("/v1/health").await;
        pokes.push(tokio::spawn(async move {
            gate.wait().await;
            c.post("/v1/tick", json!({})).await
        }));
    }
    let mut lapsed_reported = 0i64;
    for p in pokes {
        let (status, body) = p.await.unwrap();
        assert_eq!(status, 200, "{body}");
        lapsed_reported += body["lapsed"].as_i64().unwrap();
    }
    assert_eq!(
        lapsed_reported, OVERDUE as i64,
        "the sweep fired more than once per item across overlapping pokes"
    );

    // And the audit log agrees: one lapse per item, not one per poke that
    // happened to be in flight.
    let (status, feed) = admin.get("/v1/changes?since=0&facet=tasks&limit=5000").await;
    assert_eq!(status, 200, "{feed}");
    let mut per_item = std::collections::HashMap::new();
    for row in feed["changes"].as_array().unwrap() {
        if row["op"] == "lapsed" {
            *per_item.entry(row["item_id"].as_str().unwrap().to_string()).or_insert(0) += 1;
        }
    }
    assert_eq!(per_item.len(), OVERDUE, "not every overdue item lapsed");
    let duplicated: Vec<_> = per_item.iter().filter(|(_, n)| **n > 1).collect();
    assert!(duplicated.is_empty(), "duplicate lapsed rows: {duplicated:?}");
}

/// Every by-id write path checks the facet, not just the read paths.
#[tokio::test]
async fn capabilities_scope_every_write_path() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;
    register_lists_facet(&admin).await;

    let (_, item) = admin
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "mine", "done": false}}))
        .await;
    let id = item["id"].as_str().unwrap().to_string();
    let revision = item["revision"].as_i64().unwrap();

    // A token for a different facet entirely.
    let other = erisdb::auth::mint(SECRET, &["lists:*"], Some(3600), None).unwrap();
    let stranger = Client::new(&url, &other);

    let (status, _) = stranger
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "yours", "done": true}, "revision": revision}))
        .await;
    assert_eq!(status, 403, "updated across facets");

    let (status, _) = stranger.get(&format!("/v1/items/{id}/history")).await;
    assert_eq!(status, 403, "read history across facets");

    let (status, _) = stranger
        .post(&format!("/v1/items/{id}/revert"), json!({"seq": 1, "revision": revision}))
        .await;
    assert_eq!(status, 403, "reverted across facets");

    let status = stranger.delete(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 403, "deleted across facets");

    // The item is untouched.
    let (status, still) = admin.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(still["body"]["title"], "mine");
}

/// Delete takes the same optimistic-concurrency contract as update when the
/// caller offers one, instead of silently winning a race.
#[tokio::test]
async fn delete_honours_a_revision_when_given_one() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    let (_, item) = admin
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "contested", "done": false}}))
        .await;
    let id = item["id"].as_str().unwrap().to_string();
    let stale = item["revision"].as_i64().unwrap();

    // Someone else edits it first.
    let (status, _) = admin
        .put(&format!("/v1/items/{id}"), json!({"body": {"title": "edited", "done": false}, "revision": stale}))
        .await;
    assert_eq!(status, 200);

    let status = admin.delete(&format!("/v1/items/{id}?revision={stale}")).await;
    assert_eq!(status, 409, "a delete on a stale revision won the race");

    let (status, _) = admin.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 200, "the item was deleted anyway");

    let status = admin.delete(&format!("/v1/items/{id}?revision={}", stale + 1)).await;
    assert_eq!(status, 204);
}

/// The claimed rung of the trust gradient is bounded: it lands in every
/// change row this caller writes.
#[tokio::test]
async fn an_oversized_client_name_is_refused() {
    let (url, root, _pool) = setup().await;
    register_tasks_facet(&Client::new(&url, &root)).await;

    let huge = "x".repeat(erisdb::api::MAX_CLIENT_LEN + 1);
    let c = Client::with_client(&url, &root, &huge);
    let (status, _) = c
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "big", "done": false}}))
        .await;
    assert_eq!(status, 400);
}

/// A store failure is the operator's to read. The caller gets a code, not
/// the constraint names and query fragments Postgres puts in its messages.
#[tokio::test]
async fn store_errors_do_not_leak_their_internals() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);
    register_tasks_facet(&admin).await;

    // Two facets with the same name: a unique-index violation underneath.
    let (status, body) = admin
        .post(
            "/v1/items",
            json!({"facet": "facet", "body": {"name": "tasks", "schema": {"type": "object"}}}),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    for leak in ["items_", "duplicate key", "pkey", "Key (", "SELECT", "INSERT"] {
        assert!(!detail.contains(leak), "leaked {leak:?} in {detail:?}");
    }
}

/// Authorization lives in the router, so it holds on the QUIC path exactly
/// as it does on TCP. Nothing pinned that before.
#[tokio::test]
async fn iroh_enforces_capabilities_too() -> Result<()> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;

    let pool = fresh_pool().await;
    let app = erisdb::app(pool, SECRET.to_vec());
    let server_ep = erisdb::net::endpoint(b"iroh-authz-test").await?;
    let server_addr = erisdb::net::advertised_addr(&server_ep).await?;
    tokio::spawn(erisdb::net::serve(server_ep, app));

    let client_ep = erisdb::net::endpoint(b"iroh-authz-client").await?;
    let conn = client_ep.connect(server_addr, erisdb::net::ALPN).await?;

    async fn status_of(
        conn: &iroh::endpoint::Connection,
        req: hyper::Request<Full<Bytes>>,
    ) -> Result<u16> {
        let (send, recv) = conn.open_bi().await?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(driver);
        let resp = sender.send_request(req).await?;
        let status = resp.status().as_u16();
        let _ = resp.into_body().collect().await?;
        Ok(status)
    }

    // No token at all.
    let status = status_of(
        &conn,
        hyper::Request::get("/v1/items?facet=tasks").header("host", "erisdb").body(Full::default())?,
    )
    .await?;
    assert_eq!(status, 401, "the QUIC path served an unauthenticated read");

    // A garbage token.
    let status = status_of(
        &conn,
        hyper::Request::get("/v1/items?facet=tasks")
            .header("host", "erisdb")
            .header("authorization", "Bearer bz1.not.real")
            .body(Full::default())?,
    )
    .await?;
    assert_eq!(status, 401);

    // A real token for the wrong facet.
    let narrow = erisdb::auth::mint(SECRET, &["lists:read"], Some(3600), None).unwrap();
    let status = status_of(
        &conn,
        hyper::Request::get("/v1/items?facet=tasks")
            .header("host", "erisdb")
            .header("authorization", format!("Bearer {narrow}"))
            .body(Full::default())?,
    )
    .await?;
    assert_eq!(status, 403, "the QUIC path ignored facet scope");

    // An expired token.
    let expired = erisdb::auth::mint(SECRET, &["*:read"], Some(-10), None).unwrap();
    let status = status_of(
        &conn,
        hyper::Request::get("/v1/items?facet=tasks")
            .header("host", "erisdb")
            .header("authorization", format!("Bearer {expired}"))
            .body(Full::default())?,
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

/// The whole point of an open namespace: standing up a new client touches
/// no backend. One token holding `meta:facets:write` registers the facet,
/// and a grant naming it works immediately — no deploy, no allowlist, no
/// core release anywhere in this path.
#[tokio::test]
async fn a_new_facet_and_client_need_nothing_but_meta_facets_write() {
    let (url, root, _pool) = setup().await;

    // A token that can register facets and nothing else whatsoever.
    let registrar_token = erisdb::auth::mint(SECRET, &["meta:facets:write"], Some(3600), None).unwrap();
    let registrar = Client::new(&url, &registrar_token);

    // A grant for a namespace that does not exist yet is legal, and inert.
    let early = erisdb::auth::mint(SECRET, &["sensors:create"], Some(3600), None).unwrap();
    let logger = Client::new(&url, &early);
    let (status, _) = logger
        .post("/v1/items", json!({"facet": "sensors", "body": {"c": 21}}))
        .await;
    assert_eq!(status, 422, "an unregistered facet is unknown, not forbidden");

    // Register it. This is the only privileged step there is.
    let (status, body) = registrar
        .post(
            "/v1/items",
            json!({"facet": "facet", "body": {
                "name": "sensors",
                "version": 1,
                "schema": {"type": "object", "required": ["c"], "properties": {"c": {"type": "number"}}},
                "permissions": {"create": "record a reading"}
            }}),
        )
        .await;
    assert_eq!(status, 201, "{body}");

    // The same token, unchanged, now works. Nothing was restarted.
    let (status, reading) = logger
        .post("/v1/items", json!({"facet": "sensors", "body": {"c": 21}}))
        .await;
    assert_eq!(status, 201, "{reading}");

    // And it is append-only, which read/write verbs could not express.
    let id = reading["id"].as_str().unwrap().to_string();
    let (status, _) = logger.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 403, "a create-only grant could read");
    let (status, _) = logger
        .put(&format!("/v1/items/{id}"), json!({"body": {"c": 99}, "revision": 1}))
        .await;
    assert_eq!(status, 403, "a create-only grant could edit");
    assert_eq!(logger.delete(&format!("/v1/items/{id}")).await, 403, "a create-only grant could erase");

    // The registrar itself cannot read the data it made room for.
    let (status, _) = registrar.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 403);

    // Nor can it mint itself anything: registering facets is not admin.
    let (status, _) = registrar
        .post("/v1/capabilities", json!({"grants": ["sensors:read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);

    // The root token can still see everything, as a sanity check that the
    // reading really landed.
    let (status, item) = Client::new(&url, &root).get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(item["body"]["c"], 21);
}

/// Reading everything and administering everything are different grants.
#[tokio::test]
async fn read_everything_does_not_reach_into_meta() {
    let (url, root, _pool) = setup().await;
    register_tasks_facet(&Client::new(&url, &root)).await;

    let reader = Client::new(&url, &erisdb::auth::mint(SECRET, &["*:read"], Some(3600), None).unwrap());

    // It reads any facet.
    let (status, _) = reader.get("/v1/items?facet=tasks").await;
    assert_eq!(status, 200);

    // It does not read the facet table, the server, or pairings, and it
    // cannot mint or tick.
    let (status, _) = reader.get("/v1/items?facet=facet").await;
    assert_eq!(status, 403, "*:read reached the facet registrations");
    let (status, _) = reader.post("/v1/tick", json!({})).await;
    assert_eq!(status, 403);
    let (status, _) = reader
        .post("/v1/capabilities", json!({"grants": ["tasks:read"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);

    // Nor the cross-facet feed, which is its own permission.
    let (status, _) = reader.get("/v1/changes?since=0").await;
    assert_eq!(status, 403);
    let (status, _) = reader.get("/v1/changes?since=0&facet=tasks").await;
    assert_eq!(status, 200, "a per-facet feed is just a read");
}

/// A facet name is a permission namespace, so naming one `meta` would be a
/// way to mint a meta permission by registering something.
#[tokio::test]
async fn a_facet_cannot_name_itself_into_meta() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);

    for name in ["meta", "facet", "system", "pair", "*", "meta:facets", "Tasks", "tasks:read"] {
        let (status, body) = admin
            .post(
                "/v1/items",
                json!({"facet": "facet", "body": {"name": name, "schema": {"type": "object"}}}),
            )
            .await;
        assert_eq!(status, 400, "registered a facet named {name:?}: {body}");
    }
}

/// Grants the core has no meaning for are carried and enclosed like any
/// other, so a bridge can define its own and enforce them itself.
#[tokio::test]
async fn the_core_delegates_permissions_it_does_not_understand() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);

    let (status, minted) = admin
        .post("/v1/capabilities", json!({"grants": ["imap:sync", "imap:read"], "ttl_secs": 600}))
        .await;
    assert_eq!(status, 201, "{minted}");
    let cap = erisdb::auth::verify(SECRET, minted["token"].as_str().unwrap()).unwrap();
    assert_eq!(cap.grants, ["imap:sync", "imap:read"]);
    assert!(cap.granted("imap:sync"));
    assert!(!cap.granted("imap:delete"));

    // And that token cannot widen its own bridge scope.
    let bridge = Client::new(&url, minted["token"].as_str().unwrap());
    let (status, _) = bridge
        .post("/v1/capabilities", json!({"grants": ["imap:*"], "ttl_secs": 60}))
        .await;
    assert_eq!(status, 403);
}

/// The whole conversation: a code is cut, a client redeems it saying what
/// it wants, a human approves, and only then does a token exist.
#[tokio::test]
async fn pairing_grants_nothing_until_a_human_approves() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    register_tasks_facet(&operator).await;

    // 1. Cut a code.
    let (status, cut) = operator.post("/v1/pairings", json!({})).await;
    assert_eq!(status, 201, "{cut}");
    let id = cut["id"].as_str().unwrap().to_string();
    let code = cut["secret"].as_str().unwrap().to_string();

    // The code is not a capability over anything. It is worth nothing on
    // its own, which is the entire point of putting it in a QR.
    let scanner = Client::new(&url, &code);
    let (status, _) = scanner.get("/v1/items?facet=tasks").await;
    assert_eq!(status, 403, "the pairing code could read data");
    let (status, _) = scanner
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "no", "done": false}}))
        .await;
    assert_eq!(status, 403, "the pairing code could write data");

    // 2. Redeem it with a manifest.
    let (status, asked) = scanner
        .post(
            "/v1/pair/redeem",
            json!({"client": "Tasks (Android) v0.3",
                   "requested": ["tasks:read", "tasks:create", "tasks:update"]}),
        )
        .await;
    assert_eq!(status, 200, "{asked}");
    assert_eq!(asked["body"]["status"], "requested");

    // Still nothing granted, and nothing to collect yet.
    let (status, waiting) = scanner.get("/v1/pair/status").await;
    assert_eq!(status, 200);
    assert_eq!(waiting["status"], "requested");
    assert!(waiting["token"].is_null(), "a token appeared before approval");

    // 3. The operator sees exactly what was asked, and by whom.
    let (status, pending) = operator.get("/v1/pairings").await;
    assert_eq!(status, 200, "{pending}");
    let session = &pending["pairings"].as_array().unwrap()[0];
    assert_eq!(session["body"]["client"], "Tasks (Android) v0.3");
    assert_eq!(session["body"]["requested"][1], "tasks:create");
    assert!(session["body"]["token"].is_null(), "the list leaked a token");

    // 4. Approve less than was asked for.
    let (status, approved) = operator
        .post(&format!("/v1/pairings/{id}/approve"), json!({"granted": ["tasks:read", "tasks:create"]}))
        .await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["body"]["status"], "approved");
    assert!(approved["body"]["token"].is_null(), "the approval response leaked the token");

    // 5. The client collects, once.
    let (status, collected) = scanner.get("/v1/pair/status").await;
    assert_eq!(status, 200, "{collected}");
    assert_eq!(collected["status"], "approved");
    let token = collected["token"].as_str().expect("a token to collect").to_string();

    let (status, again) = scanner.get("/v1/pair/status").await;
    assert_eq!(status, 200);
    assert_eq!(again["status"], "approved");
    assert!(again["token"].is_string(), "the bound client must recover a lost response");

    // And it grants exactly what the human said, not what was asked.
    let app = Client::new(&url, &token);
    let (status, item) = app
        .post("/v1/items", json!({"facet": "tasks", "body": {"title": "paired", "done": false}}))
        .await;
    assert_eq!(status, 201, "{item}");
    let item_id = item["id"].as_str().unwrap();
    let (status, _) = app.get(&format!("/v1/items/{item_id}")).await;
    assert_eq!(status, 200);
    let (status, _) = app
        .put(&format!("/v1/items/{item_id}"), json!({"body": {"title": "x", "done": true}, "revision": 1}))
        .await;
    assert_eq!(status, 403, "update was requested but not granted");
}

/// Seeing the QR must not let another browser collect the approved client's token.
#[tokio::test]
async fn a_photographed_qr_cannot_collect_another_clients_approval() {
    use base64::Engine;
    use sha2::Digest;
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let code = cut["secret"].as_str().unwrap();
    let verifier = "legitimate-client-secret-with-at-least-43-characters";
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let scanner = Client::new(&url, code);
    let (status, asked) = scanner.post("/v1/pair/redeem", json!({
        "client": "My browser", "requested": ["tasks:read"], "challenge": challenge,
    })).await;
    assert_eq!(status, 200, "{asked}");
    let (status, approved) = operator.post(
        &format!("/v1/pairings/{}/approve", cut["id"].as_str().unwrap()), json!({}),
    ).await;
    assert_eq!(status, 200, "{approved}");

    let thief = reqwest::Client::new();
    let stolen = thief.get(format!("{url}/v1/pair/status")).bearer_auth(code).send().await.unwrap();
    assert_eq!(stolen.status(), 401, "the QR holder collected someone else's token");

    let collected = scanner.http.get(format!("{url}/v1/pair/status"))
        .bearer_auth(code).header("X-ErisDB-Client-Proof", verifier).send().await.unwrap();
    assert_eq!(collected.status(), 200);
    assert!(collected.json::<Value>().await.unwrap()["token"].is_string());
}

#[tokio::test]
async fn a_denied_pairing_yields_no_token() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let id = cut["id"].as_str().unwrap().to_string();
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());

    let (status, _) = scanner
        .post("/v1/pair/redeem", json!({"client": "Nosy", "requested": ["*"]}))
        .await;
    assert_eq!(status, 200);

    let (status, denied) = operator.post(&format!("/v1/pairings/{id}/deny"), json!({})).await;
    assert_eq!(status, 200, "{denied}");
    assert_eq!(denied["body"]["status"], "denied");

    let (status, after) = scanner.get("/v1/pair/status").await;
    assert_eq!(status, 200);
    assert_eq!(after["status"], "denied");
    assert!(after["token"].is_null());

    // A denied code cannot be redeemed again to try for a better answer.
    let (status, _) = scanner
        .post("/v1/pair/redeem", json!({"client": "Nosy", "requested": ["tasks:read"]}))
        .await;
    assert_eq!(status, 409);
}

/// Approving is minting, so it is bounded the same way: an operator cannot
/// hand out what they do not themselves hold.
#[tokio::test]
async fn an_approval_cannot_exceed_the_approver() {
    let (url, root, _pool) = setup().await;
    Client::new(&url, &root);

    // An operator who may run pairing, but only over tasks.
    let scoped = erisdb::auth::mint(
        SECRET,
        &["meta:pairing:create", "meta:pairing:read", "meta:pairing:approve", "tasks:*"],
        Some(3600),
        None,
    )
    .unwrap();
    let operator = Client::new(&url, &scoped);

    let (status, cut) = operator.post("/v1/pairings", json!({})).await;
    assert_eq!(status, 201, "{cut}");
    let id = cut["id"].as_str().unwrap().to_string();
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());
    let (status, _) = scanner
        .post("/v1/pair/redeem", json!({"client": "Greedy", "requested": ["*"]}))
        .await;
    assert_eq!(status, 200);

    // Approving the request as asked would grant everything.
    let (status, _) = operator.post(&format!("/v1/pairings/{id}/approve"), json!({})).await;
    assert_eq!(status, 403, "a tasks-only operator granted `*`");

    let (status, _) = operator
        .post(&format!("/v1/pairings/{id}/approve"), json!({"granted": ["lists:read"]}))
        .await;
    assert_eq!(status, 403, "granted a facet the approver does not hold");

    // Inside its own scope it works.
    let (status, ok) = operator
        .post(&format!("/v1/pairings/{id}/approve"), json!({"granted": ["tasks:read"]}))
        .await;
    assert_eq!(status, 200, "{ok}");
}

#[tokio::test]
async fn a_pairing_code_names_one_session_and_only_its_own() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (_, first) = operator.post("/v1/pairings", json!({})).await;
    let (_, second) = operator.post("/v1/pairings", json!({})).await;

    // The first code redeems the first session, not the second.
    let a = Client::new(&url, first["secret"].as_str().unwrap());
    let (status, _) = a.post("/v1/pair/redeem", json!({"client": "A", "requested": ["tasks:read"]})).await;
    assert_eq!(status, 200);

    let (_, other) = operator.get(&format!("/v1/pairings/{}", second["id"].as_str().unwrap())).await;
    assert_eq!(other["body"]["status"], "pending", "one code redeemed another's session");

    // A token that is not a pairing code cannot redeem at all.
    let ordinary = Client::new(&url, &root);
    let (status, _) = ordinary
        .post("/v1/pair/redeem", json!({"client": "root", "requested": ["tasks:read"]}))
        .await;
    assert_eq!(status, 401, "a token with no session redeemed something");
}

#[tokio::test]
async fn a_token_can_always_read_its_own_permissions() {
    let (url, _root, _pool) = setup().await;
    let narrow = erisdb::auth::mint(SECRET, &["tasks:read"], Some(3600), Some("alice")).unwrap();
    let (status, me) = Client::new(&url, &narrow).get("/v1/permissions").await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["grants"], json!(["tasks:read"]));
    assert_eq!(me["user"], "alice");
    assert!(me["exp"].is_i64());
}

#[tokio::test]
async fn server_state_needs_its_own_permission() {
    let (url, root, _pool) = setup().await;
    register_tasks_facet(&Client::new(&url, &root)).await;

    let narrow = erisdb::auth::mint(SECRET, &["*:read"], Some(3600), None).unwrap();
    let (status, _) = Client::new(&url, &narrow).get("/v1/server").await;
    assert_eq!(status, 403);

    let (status, state) = Client::new(&url, &root).get("/v1/server").await;
    assert_eq!(status, 200, "{state}");
    assert!(state["feed_head"].as_i64().unwrap() > 0);
    assert!(state["facets"].as_i64().unwrap() >= 2);
    assert_eq!(state["limits"]["streams"], erisdb::api::MAX_STREAMS as i64);
}

/// A token that has been issued must not be recoverable from the audit log.
/// The approve write is permanent, so the token is never in it: what is
/// stored is the shape of the token, and minting happens at collection.
#[tokio::test]
async fn an_issued_token_never_reaches_the_change_feed() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    register_tasks_facet(&operator).await;

    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let id = cut["id"].as_str().unwrap().to_string();
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());
    scanner
        .post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"]}))
        .await;
    operator.post(&format!("/v1/pairings/{id}/approve"), json!({})).await;

    let (_, collected) = scanner.get("/v1/pair/status").await;
    let token = collected["token"].as_str().expect("a token").to_string();
    assert!(erisdb::auth::verify(SECRET, &token).is_ok(), "the collected token does not verify");

    // Every trace of this session, everywhere it is written down.
    let (status, feed) = operator.get("/v1/changes?since=0&limit=5000").await;
    assert_eq!(status, 200, "{feed}");
    let written = serde_json::to_string(&feed).unwrap();
    assert!(!written.contains(&token), "the issued token is in the change feed");
    assert!(!written.contains("bz1."), "a token of some kind is in the change feed");

    let (_, history) = operator.get(&format!("/v1/items/{id}/history")).await;
    assert!(!serde_json::to_string(&history).unwrap().contains(&token), "the token is in history");

    let (_, session) = operator.get(&format!("/v1/pairings/{id}")).await;
    assert!(!serde_json::to_string(&session).unwrap().contains(&token));
}

/// Reading pairing requests is a dashboard's job. It must not also be a way
/// to rewrite one, or to read a session as an unredacted item.
#[tokio::test]
async fn a_pairing_reader_cannot_change_a_session() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let id = cut["id"].as_str().unwrap().to_string();
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());
    scanner
        .post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"]}))
        .await;

    let viewer = Client::new(&url, &erisdb::auth::mint(SECRET, &["meta:pairing:read"], Some(3600), None).unwrap());

    // It may look.
    let (status, seen) = viewer.get("/v1/pairings").await;
    assert_eq!(status, 200, "{seen}");

    // It may not approve, deny, or hand-edit the session as an item.
    let (status, _) = viewer.post(&format!("/v1/pairings/{id}/approve"), json!({})).await;
    assert_eq!(status, 403);
    let (status, _) = viewer.post(&format!("/v1/pairings/{id}/deny"), json!({})).await;
    assert_eq!(status, 403);

    let (_, item) = viewer.get(&format!("/v1/items/{id}")).await;
    let revision = item["revision"].as_i64().unwrap_or(1);
    let (status, _) = viewer
        .put(
            &format!("/v1/items/{id}"),
            json!({"body": {"status": "approved", "granted": ["*"], "expires": 9_999_999_999i64},
                   "revision": revision}),
        )
        .await;
    assert_ne!(status, 200, "a pairing reader rewrote a session into approved");
    assert_ne!(viewer.delete(&format!("/v1/items/{id}")).await, 204);
}

/// Even an approver drives the state machine rather than the rows: a
/// hand-written session could grant what nobody asked for and nobody
/// approved.
#[tokio::test]
async fn the_cores_own_facets_are_not_writable_as_items() {
    let (url, root, _pool) = setup().await;
    let admin = Client::new(&url, &root);

    let (_, cut) = admin.post("/v1/pairings", json!({})).await;
    let id = cut["id"].as_str().unwrap().to_string();
    let (_, item) = admin.get(&format!("/v1/items/{id}")).await;

    let (status, body) = admin
        .put(
            &format!("/v1/items/{id}"),
            json!({"body": {"status": "approved", "granted": ["*"], "expires": 9_999_999_999i64},
                   "revision": item["revision"]}),
        )
        .await;
    assert_eq!(status, 400, "a root token hand-edited a pairing session: {body}");
    assert_eq!(admin.delete(&format!("/v1/items/{id}")).await, 400);

    let (status, _) = admin
        .post("/v1/items", json!({"facet": "pair", "body": {"status": "approved"}}))
        .await;
    assert_eq!(status, 400, "a pairing session was forged through the item route");

    let (status, _) = admin
        .post("/v1/items", json!({"facet": "system", "body": {"anything": true}}))
        .await;
    assert_eq!(status, 400);

    // Reading them as items is still fine.
    let (status, _) = admin.get(&format!("/v1/items/{id}")).await;
    assert_eq!(status, 200);
}

/// Denying after approval is a cancel and it works, because the token does
/// not exist until it is collected. Denying after collection is a lie.
#[tokio::test]
async fn denying_an_approved_pairing_stops_the_token_existing() {
    let (url, root, _pool) = setup().await;
    let operator = Client::new(&url, &root);
    register_tasks_facet(&operator).await;

    let (_, cut) = operator.post("/v1/pairings", json!({})).await;
    let id = cut["id"].as_str().unwrap().to_string();
    let scanner = Client::new(&url, cut["secret"].as_str().unwrap());
    scanner
        .post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"]}))
        .await;
    operator.post(&format!("/v1/pairings/{id}/approve"), json!({})).await;

    // Second thoughts, before the client polls.
    let (status, _) = operator.post(&format!("/v1/pairings/{id}/deny"), json!({})).await;
    assert_eq!(status, 200);

    let (status, after) = scanner.get("/v1/pair/status").await;
    assert_eq!(status, 200);
    assert_eq!(after["status"], "denied");
    assert!(after["token"].is_null(), "a denied pairing still issued a token");

    // But once collected, denial is refused rather than pretending.
    let (_, cut2) = operator.post("/v1/pairings", json!({})).await;
    let id2 = cut2["id"].as_str().unwrap().to_string();
    let s2 = Client::new(&url, cut2["secret"].as_str().unwrap());
    s2.post("/v1/pair/redeem", json!({"client": "App", "requested": ["tasks:read"]})).await;
    operator.post(&format!("/v1/pairings/{id2}/approve"), json!({})).await;
    let (_, got) = s2.get("/v1/pair/status").await;
    assert!(got["token"].as_str().is_some());
    let (status, _) = operator.post(&format!("/v1/pairings/{id2}/deny"), json!({})).await;
    assert_eq!(status, 409, "denied a pairing whose token is already in the wild");
}

#[tokio::test]
async fn iroh_identity_is_derived_from_the_secret() -> Result<()> {
    // Same secret → same endpoint id, across restarts: clients hold one
    // address forever. Different secret → different identity.
    let a = erisdb::net::endpoint(SECRET).await?;
    let id_a = a.id();
    a.close().await;
    let b = erisdb::net::endpoint(SECRET).await?;
    assert_eq!(id_a, b.id(), "endpoint id must survive a restart");
    b.close().await;
    let other = erisdb::net::endpoint(b"a different secret").await?;
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
    let app = erisdb::app(pool, SECRET.to_vec());
    let server_ep = erisdb::net::endpoint(SECRET).await?;
    let server_addr = erisdb::net::advertised_addr(&server_ep).await?;
    tokio::spawn(erisdb::net::serve(server_ep, app));

    // The client is its own endpoint with its own identity.
    let client_ep = erisdb::net::endpoint(b"client-side-secret").await?;
    let conn = client_ep.connect(server_addr, erisdb::net::ALPN).await?;
    let root = erisdb::auth::mint(SECRET, &["*"], Some(3600), None).unwrap();

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
        hyper::Request::get("/v1/health").header("host", "erisdb").body(Full::default())?,
    )
    .await?;
    assert_eq!(status, 200);

    let facet_req = hyper::Request::post("/v1/items")
        .header("host", "erisdb")
        .header("authorization", format!("Bearer {root}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            json!({"facet": "facet", "body": {"name": "tasks", "schema": {"type": "object"}}}).to_string(),
        )))?;
    let (status, body) = request(&conn, facet_req).await?;
    assert_eq!(status, 201, "{body}");

    let item_req = hyper::Request::post("/v1/items")
        .header("host", "erisdb")
        .header("authorization", format!("Bearer {root}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            json!({"facet": "tasks", "body": {"title": "over quic", "done": false}}).to_string(),
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

/// A real terminal process produces the PNG, a real decoder scans it, and
/// operator input selects authority that the resulting client actually uses.
#[tokio::test]
async fn terminal_qr_subset_and_client_management_work_end_to_end() {
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;
    let (base, root, _) = setup().await;
    let png_path = std::env::temp_dir().join(format!("erisdb-pair-{}.png", uuid::Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_erisdb"));
    let mut child = command.args(["pair", "--url", &base, "--client-url", &base,
        "--secret", std::str::from_utf8(SECRET).unwrap(), "--no-iroh", "--qr-output"])
        .arg(&png_path).stdin(Stdio::piped()).stdout(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut printed = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            printed.push_str(&line); printed.push('\n');
            if line.starts_with("saved ") { return; }
        }
        panic!("terminal ended without a PNG");
    }).await.unwrap();
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&png_path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
    let image = reader.next_frame(&mut bytes).unwrap();
    let mut qr = rqrr::PreparedImage::prepare_from_greyscale(image.width as usize, image.height as usize,
        |x,y| bytes[y * image.width as usize + x]);
    let decoded = qr.detect_grids()[0].decode().unwrap().1;
    assert!(printed.contains(&decoded));
    let ticket = erisdb::ticket::Ticket::parse(&decoded).unwrap();
    assert_eq!(ticket.url.as_deref(), Some(base.as_str()));
    let app = Client::new(&base, &ticket.token);
    let (status, request) = app.post("/v1/pair/redeem", json!({"client":"Scanned QR app", "requested":["tasks:read","tasks:create"]})).await;
    assert_eq!(status,200);
    let id = request["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("Compare this code") {
                assert!(line.contains(request["body"]["fingerprint"].as_str().unwrap()));
                return;
            }
        }
        panic!("terminal did not display comparison");
    }).await.unwrap();
    input.write_all(b"s\n1\n").await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10),child.wait()).await.unwrap().unwrap().success());
    let (status, approved) = app.get("/v1/pair/status").await;
    assert_eq!(status,200);
    assert_eq!(approved["granted"],json!(["tasks:read"]));
    let client = Client::new(&base,approved["token"].as_str().unwrap());
    assert_eq!(client.post("/v1/items",json!({"facet":"tasks","body":{"title":"forbidden"}})).await.0,403);
    for action in [vec!["list"],vec!["show",id],vec!["permissions",id,"--grant","tasks:read,tasks:create"],vec!["revoke",id]] {
        let out = Command::new(env!("CARGO_BIN_EXE_erisdb"))
            .args(["clients","--url",&base,"--secret",std::str::from_utf8(SECRET).unwrap()])
            .args(action).output().await.unwrap();
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains(id));
    }
    assert_eq!(client.get("/v1/permissions").await.0,401);
    assert_eq!(Client::new(&base,&root).get("/v1/clients").await.1["clients"].as_array().unwrap().len(),1);
    std::fs::remove_file(png_path).unwrap();
}

/// Upgrade a real pre-registry database, preserving data and recording every
/// forced pairing transition in the same durable feed as ordinary writes.
#[tokio::test]
async fn upgrading_legacy_pairings_preserves_data_and_audits_invalidation() {
    let legacy = sqlx::migrate::Migrator::with_migrations(
        erisdb::MIGRATOR.iter().filter(|m| m.version < 6).cloned().collect());
    let pool = fresh_pool_with(&legacy).await;
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO items (id, facet, body) VALUES ($1, 'pair', $2)")
        .bind(id).bind(json!({"status":"approved","requested":["tasks:read"],"granted":["tasks:read"]}))
        .execute(&pool).await.unwrap();
    let data_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO items (id, facet, body) VALUES ($1, 'tasks', $2)")
        .bind(data_id).bind(json!({"title":"existing data","done":false})).execute(&pool).await.unwrap();
    erisdb::MIGRATOR.run(&pool).await.unwrap();
    let base = spawn_core(pool).await;
    let admin = Client::new(&base, &erisdb::auth::mint(SECRET, &["*"], Some(60), None).unwrap());
    let pairing = admin.get(&format!("/v1/pairings/{id}")).await.1;
    assert_eq!(pairing["body"]["status"],"denied");
    assert_eq!(pairing["revision"],2);
    let history = admin.get(&format!("/v1/items/{id}/history")).await.1;
    assert!(history.to_string().contains("0006_registered_clients"));
    assert!(history.to_string().contains("denied"));
    assert_eq!(admin.get(&format!("/v1/items/{data_id}")).await.1["body"]["title"],"existing data");
    assert_eq!(admin.get("/v1/clients").await.1["clients"],json!([]));
}
