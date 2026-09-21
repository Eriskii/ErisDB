//! The v1 HTTP surface: items CRUD, the change feed, tick, and minting.
//!
//! Every handler is a pure function over (store, request). The process holds
//! no state a restart would lose; replicas are interchangeable.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgListener;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::auth::{self, Capability};
use crate::error::{Error, Result};
use crate::permission;
use crate::installation::{self, Identity, Installation};
use crate::plugin::{PluginRegistry, MAX_PLUGIN_REQUEST_BYTES};

/// Facet reserved for core-emitted events (tick).
pub const SYSTEM_FACET: &str = "system";
/// The meta-facet holding facet definitions.
pub const FACET_FACET: &str = "facet";
/// The facet holding pairing sessions.
pub const PAIR_FACET: &str = "pair";
/// Postgres NOTIFY channel fanned out to change-stream subscribers.
pub const NOTIFY_CHANNEL: &str = "bezel_changes";

/// Live change streams allowed at once. Each one holds a Postgres connection
/// of its own, outside the pool, so this is a bound on the store as much as
/// on the process.
pub const MAX_STREAMS: usize = 32;

/// The longest `X-Bezel-Client` the core will stamp. It lands in every change
/// row this caller writes, so an unbounded one is a way to grow the table.
pub const MAX_CLIENT_LEN: usize = 128;

/// Serializes `seq` assignment across writers. `seq` comes from a sequence at
/// INSERT time, but rows only become visible at COMMIT — so without this two
/// writers can commit out of order, and a reader that sees the later `seq`
/// steps over the earlier one and never gets it. Holding one
/// transaction-scoped lock from the append through the commit makes seq order
/// and commit order the same thing, which is what lets a cursor be a single
/// number.
const CHANGES_LOCK: i64 = 0x0062_657a_656c_0001;

/// Facet name to the schema it was compiled from and the validator for it.
type ValidatorCache = Arc<Mutex<HashMap<String, (Value, Arc<jsonschema::Validator>)>>>;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub secret: Arc<Vec<u8>>,
    /// Compiled facet schemas, keyed by facet name and matched on the schema
    /// itself. Compilation is not cheap and the schema only changes when
    /// someone edits the registration.
    validators: ValidatorCache,
    /// Bounds concurrent change streams.
    streams: Arc<tokio::sync::Semaphore>,
    /// Token buckets over the capability endpoints, keyed by caller.
    minting: Arc<Mutex<RateLimiter>>,
    /// Deployment-derived one-shot executable plugins.
    plugins: PluginRegistry,
}

impl AppState {
    pub fn new(pool: PgPool, secret: Vec<u8>) -> Self {
        Self::with_plugins(pool, secret, PluginRegistry::empty())
    }

    pub fn with_plugins(pool: PgPool, secret: Vec<u8>, plugins: PluginRegistry) -> Self {
        Self {
            pool,
            secret: Arc::new(secret),
            validators: Arc::new(Mutex::new(HashMap::new())),
            streams: Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS)),
            // Minting and refreshing are rare for an honest client and
            // attractive to grind on: a handful of bursts, then a trickle.
            minting: Arc::new(Mutex::new(RateLimiter::new(10.0, 0.2))),
            plugins,
        }
    }

    /// The validator for a facet, compiled once per distinct schema.
    fn validator(&self, facet: &str, schema: &Value) -> Result<Arc<jsonschema::Validator>> {
        {
            let cache = self.validators.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached, v)) = cache.get(facet) {
                if cached == schema {
                    return Ok(v.clone());
                }
            }
        }
        let compiled = Arc::new(jsonschema::validator_for(schema).map_err(|e| {
            Error::BadRequest(format!("facet {facet} carries an invalid schema: {e}"))
        })?);
        let mut cache = self.validators.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(facet.to_string(), (schema.clone(), compiled.clone()));
        Ok(compiled)
    }

    fn check_mint_rate(&self, caller: &str) -> Result<()> {
        let mut limiter = self.minting.lock().unwrap_or_else(|e| e.into_inner());
        if limiter.allow(caller) {
            Ok(())
        } else {
            tracing::warn!(caller, "capability endpoint rate limited");
            Err(Error::TooManyRequests)
        }
    }
}

/// A token bucket per caller. Soft state on purpose: a restart forgives
/// everyone, which is fine, because the buckets exist to blunt a flood rather
/// than to keep books.
struct RateLimiter {
    buckets: HashMap<String, (Instant, f64)>,
    burst: f64,
    per_sec: f64,
}

impl RateLimiter {
    fn new(burst: f64, per_sec: f64) -> Self {
        Self { buckets: HashMap::new(), burst, per_sec }
    }

    fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        if self.buckets.len() > 4096 {
            self.buckets.retain(|_, (seen, _)| now.duration_since(*seen) < Duration::from_secs(300));
        }
        let bucket = self.buckets.entry(key.to_string()).or_insert((now, self.burst));
        let elapsed = now.duration_since(bucket.0).as_secs_f64();
        bucket.0 = now;
        bucket.1 = (bucket.1 + elapsed * self.per_sec).min(self.burst);
        if bucket.1 >= 1.0 {
            bucket.1 -= 1.0;
            true
        } else {
            false
        }
    }
}

pub fn app(pool: PgPool, secret: Vec<u8>) -> Router {
    app_with_plugins(pool, secret, PluginRegistry::empty())
}

pub fn app_with_plugins(pool: PgPool, secret: Vec<u8>, plugins: PluginRegistry) -> Router {
    let state = AppState::with_plugins(pool, secret, plugins);
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/plugins", get(list_plugins))
        .route(
            "/v1/call",
            post(call_plugin).layer(DefaultBodyLimit::max(MAX_PLUGIN_REQUEST_BYTES)),
        )
        .route("/v1/items", post(create_item).get(list_items))
        .route("/v1/items/{id}", get(get_item).put(update_item).delete(delete_item))
        .route("/v1/items/{id}/history", get(item_history))
        .route("/v1/items/{id}/revert", post(revert_item))
        .route("/v1/changes", get(list_changes))
        .route("/v1/changes/stream", get(stream_changes))
        .route("/v1/tick", post(tick))
        .route("/v1/capabilities", post(mint_capability))
        .route("/v1/capabilities/refresh", post(refresh_capability))
        .route("/v1/permissions", get(my_permissions))
        .route("/v1/clients", get(list_clients))
        .route("/v1/clients/{id}", get(get_client).put(update_client))
        .route("/v1/clients/{id}/revoke", post(revoke_client))
        .route("/v1/clients/{id}/refresh", post(renew_client))
        .route("/v1/server", get(server_state))
        .route("/v1/pairings", post(create_pairing).get(list_pairings))
        .route("/v1/pairings/{id}", get(get_pairing))
        .route("/v1/pairings/{id}/approve", post(approve_pairing))
        .route("/v1/pairings/{id}/deny", post(deny_pairing))
        .route("/v1/pair/redeem", post(redeem_pairing))
        .route("/v1/pair/status", get(pairing_status))
        // Browser clients are first-class; auth is the token, not the origin.
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(axum::middleware::map_response(|mut response: axum::response::Response| async {
            response.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
            response
        }))
        .with_state(state)
}

// ---------------------------------------------------------------- auth

impl FromRequestParts<AppState> for Capability {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(Error::Unauthorized)?;
        let token = header.strip_prefix("Bearer ").ok_or(Error::Unauthorized)?;
        // A refused token is the one thing an operator most wants a record
        // of, and the only trace of someone probing.
        let mut cap = auth::verify(&state.secret, token).inspect_err(|_| {
            tracing::warn!(
                path = %parts.uri.path(),
                peer = parts.extensions.get::<PeerAddr>().map(|p| p.0.as_str()).unwrap_or("?"),
                "rejected capability token"
            );
        })?;
        let peer = observed_peer(parts);
        installation::authorize(&state.pool, &mut cap, peer.as_deref()).await?;
        Ok(cap)
    }
}

// ---------------------------------------------------------------- source
// Every write is attributed: {addr, user, client} with a trust gradient —
// addr is observed from the connection, user is signed into the capability
// token, client is whatever the caller claims via X-Bezel-Client.

/// The caller's transport identity over Iroh (`iroh:<endpoint id>`),
/// inserted into request extensions by the QUIC acceptor.
#[derive(Clone)]
pub struct PeerAddr(pub String);

fn observed_peer(parts: &Parts) -> Option<String> {
    parts.extensions.get::<PeerAddr>().map(|p| p.0.clone()).or_else(|| {
        parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|address| address.0.to_string())
    })
}

/// The connection-and-request half of a source; the capability supplies
/// the user.
struct SourceParts {
    addr: Option<String>,
    client: Option<String>,
    proof: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for SourceParts {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self> {
        let addr = observed_peer(parts);
        // The claimed rung of the trust gradient, and the only one a caller
        // writes. It is copied into every change row, so it is bounded and
        // printable or it is refused.
        let client = match parts.headers.get("x-bezel-client") {
            None => None,
            Some(v) => {
                let name = v.to_str().map_err(|_| {
                    Error::BadRequest("X-Bezel-Client must be printable ASCII".into())
                })?;
                if name.len() > MAX_CLIENT_LEN {
                    return Err(Error::BadRequest(format!(
                        "X-Bezel-Client is longer than {MAX_CLIENT_LEN}"
                    )));
                }
                if name.chars().any(|c| c.is_control()) {
                    return Err(Error::BadRequest("X-Bezel-Client must be printable".into()));
                }
                Some(name.to_string())
            }
        };
        let proof = parts.headers.get(installation::PROOF_HEADER)
            .map(|v| v.to_str().map(str::to_string).map_err(|_| Error::Unauthorized))
            .transpose()?;
        Ok(SourceParts { addr, client, proof })
    }
}

impl SourceParts {
    /// The stamped source value: connection + token, never the body.
    fn stamp(&self, cap: &Capability) -> Value {
        json!({ "addr": self.addr, "user": cap.user, "client": self.client, "installation": cap.client })
    }

    /// The key a rate limit counts against: the observed address when there
    /// is one, since that is the rung a caller cannot forge.
    fn rate_key(&self) -> String {
        let addr = self.addr.as_deref().unwrap_or("unknown");
        addr.parse::<std::net::SocketAddr>().map(|p| p.ip().to_string()).unwrap_or_else(|_| addr.to_string())
    }
}

// ---------------------------------------------------------------- rows

#[derive(serde::Serialize, sqlx::FromRow)]
struct Item {
    id: Uuid,
    facet: String,
    body: Value,
    revision: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    /// The last writer's source; null only for rows minted by migrations.
    source: Option<Value>,
}

#[derive(serde::Serialize, sqlx::FromRow)]
struct Change {
    seq: i64,
    item_id: Option<Uuid>,
    facet: String,
    op: String,
    at: DateTime<Utc>,
    /// The body this change produced; null for deletes and migration rows.
    body: Option<Value>,
    /// The revision this change produced; null wherever body is.
    revision: Option<i64>,
    /// Who produced it; null for migration rows.
    source: Option<Value>,
}

// ---------------------------------------------------------------- facets

struct FacetDef {
    strict: bool,
    schema: Value,
}

async fn load_facet(tx: &mut Transaction<'_, Postgres>, name: &str) -> Result<Option<FacetDef>> {
    let body: Option<Value> = sqlx::query_scalar(
        "SELECT body FROM items WHERE facet = $1 AND body ->> 'name' = $2",
    )
    .bind(FACET_FACET)
    .bind(name)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(body.map(|b| FacetDef {
        strict: b.get("strict").and_then(Value::as_bool).unwrap_or(true),
        schema: b.get("schema").cloned().unwrap_or_else(|| json!({})),
    }))
}

/// The core's own stateful facets are driven by their own endpoints, so the
/// item routes refuse to write them. Without this, `meta:pairing:approve`
/// would also be a way to hand-edit a session into a state the machine
/// would never have produced — an approved one with grants nobody asked
/// for, say.
fn guard_core_facet(facet: &str, action: &str) -> Result<()> {
    if action != "read" && permission::core_managed(facet) {
        return Err(Error::BadRequest(format!(
            "the {facet} facet is managed by the core; use its own endpoints"
        )));
    }
    Ok(())
}

/// A facet schema is caller-supplied data that the validator later walks, so
/// a `$ref` in it is a request for the core to go and resolve something. Only
/// refs into the document itself are allowed; anything else is refused here,
/// at registration, where there is a human to tell.
fn guard_schema(schema: &Value) -> Result<()> {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "$ref" | "$recursiveRef" | "$dynamicRef") {
                    let target = value.as_str().ok_or_else(|| {
                        Error::BadRequest(format!("schema {key} must be a string"))
                    })?;
                    if !target.starts_with('#') {
                        return Err(Error::BadRequest(format!(
                            "schema {key} {target:?} points outside the document; \
                             only local refs such as \"#/$defs/name\" resolve"
                        )));
                    }
                }
                guard_schema(value)?;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(guard_schema),
        _ => Ok(()),
    }
}

/// Look up the facet an incoming body claims and check the body against it.
async fn check_against_facet(
    st: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    facet: &str,
    body: &Value,
) -> Result<()> {
    // Registering a facet means storing a schema the core will later run,
    // and naming a permission namespace. Both are checked at the door.
    if facet == FACET_FACET {
        if let Some(schema) = body.get("schema") {
            guard_schema(schema)?;
        }
    }
    let def = load_facet(tx, facet)
        .await?
        .ok_or_else(|| Error::UnknownFacet(facet.to_string()))?;
    if def.strict {
        let validator = st.validator(facet, &def.schema)?;
        if let Err(e) = validator.validate(body) {
            return Err(Error::SchemaViolation { facet: facet.to_string(), detail: e.to_string() });
        }
    }
    // After the meta-facet's own schema has spoken, so a registration with
    // no name at all is a schema violation rather than a naming complaint.
    if facet == FACET_FACET {
        if let Some(name) = body.get("name").and_then(Value::as_str) {
            permission::check_facet_name(name)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- changes

/// Append a change row (the bus and the audit log) inside the mutation's
/// own transaction and notify live subscribers on commit. `snapshot` is
/// the item's (body, revision) after the change (`None` for deletes and
/// ticks); `source` is who caused it. History is append-only: these rows
/// are never edited.
async fn record_change(
    tx: &mut Transaction<'_, Postgres>,
    item_id: Option<Uuid>,
    facet: &str,
    op: &str,
    snapshot: Option<(&Value, i64)>,
    source: &Value,
) -> Result<i64> {
    lock_changes(tx).await?;
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO changes (item_id, facet, op, body, revision, source)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING seq",
    )
    .bind(item_id)
    .bind(facet)
    .bind(op)
    .bind(snapshot.map(|(b, _)| b))
    .bind(snapshot.map(|(_, r)| r))
    .bind(source)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(NOTIFY_CHANNEL)
        .bind(seq.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(seq)
}

/// Take the append lock for the rest of this transaction. Every writer takes
/// it after it has whatever item lock it needs, so the order is always item
/// then feed and there is no cycle to deadlock on.
async fn lock_changes(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(CHANGES_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn fetch_changes(
    pool: &PgPool,
    since: i64,
    facet: Option<&str>,
    limit: i64,
) -> Result<Vec<Change>> {
    let rows = match facet {
        Some(f) => {
            sqlx::query_as::<_, Change>(
                "SELECT seq, item_id, facet, op, at, body, revision, source FROM changes
                 WHERE seq > $1 AND facet = $2 ORDER BY seq LIMIT $3",
            )
            .bind(since)
            .bind(f)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, Change>(
                "SELECT seq, item_id, facet, op, at, body, revision, source FROM changes
                 WHERE seq > $1 ORDER BY seq LIMIT $2",
            )
            .bind(since)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

/// Reading changes for one facet needs read on that facet; reading the
/// global feed needs a wildcard capability.
fn authorize_feed(cap: &Capability, facet: Option<&str>) -> Result<()> {
    match facet {
        Some(f) => cap.require(&permission::for_facet(f, "read")),
        // The unfiltered feed crosses every facet, so it is its own
        // permission rather than the sum of the ones it would reveal.
        None => cap.require("meta:feed:read"),
    }
}

// ---------------------------------------------------------------- handlers

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

// One-shot plugin calls are deliberately outside the item/change machinery:
// they authorize and validate, then connect this HTTP request to one child
// process. No request or response is written to Postgres.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginCall {
    plugin: String,
    operation: String,
    input: Value,
}

async fn list_plugins(State(st): State<AppState>, cap: Capability) -> Json<Value> {
    Json(json!({ "plugins": st.plugins.visible_to(&cap) }))
}

async fn call_plugin(
    State(st): State<AppState>,
    cap: Capability,
    Json(call): Json<PluginCall>,
) -> Result<axum::response::Response> {
    st.plugins.invoke(&cap, &call.plugin, &call.operation, call.input).await
}

#[derive(Deserialize)]
struct CreateItem {
    facet: String,
    body: Value,
}

async fn create_item(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Json(req): Json<CreateItem>,
) -> Result<impl IntoResponse> {
    cap.require(&permission::for_facet(&req.facet, "create"))?;
    guard_core_facet(&req.facet, "create")?;
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    check_against_facet(&st, &mut tx, &req.facet, &req.body).await?;
    let item = sqlx::query_as::<_, Item>(
        "INSERT INTO items (id, facet, body, source) VALUES ($1, $2, $3, $4)
         RETURNING id, facet, body, revision, created_at, updated_at, source",
    )
    .bind(Uuid::new_v4())
    .bind(&req.facet)
    .bind(&req.body)
    .bind(&source)
    .fetch_one(&mut *tx)
    .await?;
    record_change(&mut tx, Some(item.id), &item.facet, "created", Some((&item.body, item.revision)), &source)
        .await?;
    tx.commit().await?;
    Ok((axum::http::StatusCode::CREATED, Json(item)))
}

async fn get_item(
    State(st): State<AppState>,
    cap: Capability,
    Path(id): Path<Uuid>,
) -> Result<Json<Item>> {
    let item = sqlx::query_as::<_, Item>(
        "SELECT id, facet, body, revision, created_at, updated_at, source FROM items WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(Error::NotFound)?;
    cap.require(&permission::for_facet(&item.facet, "read"))?;
    Ok(Json(item))
}

#[derive(Deserialize)]
struct ListItems {
    facet: String,
    updated_since: Option<DateTime<Utc>>,
    limit: Option<i64>,
}

async fn list_items(
    State(st): State<AppState>,
    cap: Capability,
    Query(q): Query<ListItems>,
) -> Result<Json<Value>> {
    cap.require(&permission::for_facet(&q.facet, "read"))?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let items = sqlx::query_as::<_, Item>(
        "SELECT id, facet, body, revision, created_at, updated_at, source FROM items
         WHERE facet = $1 AND ($2::timestamptz IS NULL OR updated_at > $2)
         ORDER BY updated_at, id LIMIT $3",
    )
    .bind(&q.facet)
    .bind(q.updated_since)
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
struct UpdateItem {
    body: Value,
    revision: i64,
}

async fn update_item(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateItem>,
) -> Result<Json<Item>> {
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    let facet: String = sqlx::query_scalar("SELECT facet FROM items WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
    cap.require(&permission::for_facet(&facet, "update"))?;
    guard_core_facet(&facet, "update")?;
    check_against_facet(&st, &mut tx, &facet, &req.body).await?;
    let item = write_revision(&mut tx, id, &facet, &req.body, req.revision, &source).await?;
    tx.commit().await?;
    Ok(Json(item))
}

/// The one way an item's body ever changes: a revision-checked UPDATE that
/// stamps the writer's source and appends the audit row atomically.
async fn write_revision(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    facet: &str,
    body: &Value,
    revision: i64,
    source: &Value,
) -> Result<Item> {
    let item = sqlx::query_as::<_, Item>(
        "UPDATE items SET body = $1, revision = revision + 1, updated_at = now(), source = $2
         WHERE id = $3 AND revision = $4
         RETURNING id, facet, body, revision, created_at, updated_at, source",
    )
    .bind(body)
    .bind(source)
    .bind(id)
    .bind(revision)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::RevisionConflict)?;
    record_change(tx, Some(id), facet, "updated", Some((&item.body, item.revision)), source).await?;
    Ok(item)
}

#[derive(Deserialize)]
struct DeleteQuery {
    /// The revision the caller believes is current. Optimistic concurrency,
    /// exactly like an update: pass it and a delete racing someone else's
    /// edit is a conflict instead of a silent win.
    revision: Option<i64>,
}

async fn delete_item(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
    Query(q): Query<DeleteQuery>,
) -> Result<axum::http::StatusCode> {
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    let facet: String = sqlx::query_scalar("SELECT facet FROM items WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
    cap.require(&permission::for_facet(&facet, "delete"))?;
    guard_core_facet(&facet, "delete")?;
    match q.revision {
        Some(revision) => {
            let hit = sqlx::query("DELETE FROM items WHERE id = $1 AND revision = $2")
                .bind(id)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
            if hit.rows_affected() == 0 {
                return Err(Error::RevisionConflict);
            }
        }
        None => {
            sqlx::query("DELETE FROM items WHERE id = $1").bind(id).execute(&mut *tx).await?;
        }
    }
    // Body is null: the state after a delete is absence. The prior snapshot
    // lives one row up in the history.
    record_change(&mut tx, Some(id), &facet, "deleted", None, &source).await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(serde::Serialize, sqlx::FromRow)]
struct HistoryRow {
    seq: i64,
    op: String,
    at: DateTime<Utc>,
    body: Option<Value>,
    revision: Option<i64>,
    source: Option<Value>,
}

/// Every state an item has been in, oldest first. Works for deleted items
/// too: history is append-only and outlives its item.
async fn item_history(
    State(st): State<AppState>,
    cap: Capability,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let facet: Option<String> =
        // Ordered: the authorization decision must not rest on whichever row
        // the planner happened to hand back.
        sqlx::query_scalar("SELECT facet FROM changes WHERE item_id = $1 ORDER BY seq LIMIT 1")
            .bind(id)
            .fetch_optional(&st.pool)
            .await?;
    let facet = facet.ok_or(Error::NotFound)?;
    cap.require(&permission::for_facet(&facet, "read"))?;
    let rows = sqlx::query_as::<_, HistoryRow>(
        "SELECT seq, op, at, body, revision, source FROM changes WHERE item_id = $1 ORDER BY seq",
    )
    .bind(id)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(json!({ "history": rows })))
}

#[derive(Deserialize)]
struct RevertRequest {
    /// The change whose snapshot to restore.
    seq: i64,
    /// The revision the caller believes is current — optimistic concurrency,
    /// exactly like an update.
    revision: i64,
}

/// Git-revert, not time travel: the snapshot at `seq` is written as a NEW
/// revision, landing on the feed as an ordinary update with its own source.
/// History never rewinds.
async fn revert_item(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
    Json(req): Json<RevertRequest>,
) -> Result<Json<Item>> {
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    let facet: String = sqlx::query_scalar("SELECT facet FROM items WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
    cap.require(&permission::for_facet(&facet, "update"))?;
    guard_core_facet(&facet, "update")?;
    let snapshot: Option<Option<Value>> =
        sqlx::query_scalar("SELECT body FROM changes WHERE seq = $1 AND item_id = $2")
            .bind(req.seq)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let body = snapshot
        .flatten()
        .ok_or_else(|| Error::BadRequest(format!("no snapshot at seq {} for this item", req.seq)))?;
    // The facet's schema may have tightened since the snapshot was live.
    check_against_facet(&st, &mut tx, &facet, &body).await?;
    let item = write_revision(&mut tx, id, &facet, &body, req.revision, &source).await?;
    tx.commit().await?;
    Ok(Json(item))
}

#[derive(Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    since: i64,
    facet: Option<String>,
    limit: Option<i64>,
}

async fn list_changes(
    State(st): State<AppState>,
    cap: Capability,
    Query(q): Query<ChangesQuery>,
) -> Result<Json<Value>> {
    authorize_feed(&cap, q.facet.as_deref())?;
    let limit = q.limit.unwrap_or(500).clamp(1, 5000);
    let changes = fetch_changes(&st.pool, q.since, q.facet.as_deref(), limit).await?;
    let next = changes.last().map(|c| c.seq).unwrap_or(q.since);
    Ok(Json(json!({ "changes": changes, "next": next })))
}

async fn stream_changes(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Query(q): Query<ChangesQuery>,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    authorize_feed(&cap, q.facet.as_deref())?;
    // A stream costs a Postgres connection of its own for as long as it is
    // open, so the count is bounded before one is taken.
    let permit = st
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Unavailable)?;
    let mut listener = PgListener::connect_with(&st.pool).await?;
    listener.listen(NOTIFY_CHANNEL).await?;

    struct Feed {
        pool: PgPool,
        listener: PgListener,
        facet: Option<String>,
        cursor: i64,
        queue: VecDeque<Change>,
        /// Signature verification happens at subscribe; expiry and live
        /// registration authority are checked before every emitted event.
        expires_at: Option<i64>,
        capability: Capability,
        peer: Option<String>,
        _permit: tokio::sync::OwnedSemaphorePermit,
    }
    let feed = Feed {
        pool: st.pool.clone(),
        listener,
        facet: q.facet,
        cursor: q.since,
        queue: VecDeque::new(),
        expires_at: cap.exp,
        capability: cap,
        peer: src.addr,
        _permit: permit,
    };

    let stream = futures::stream::unfold(feed, |mut s| async move {
        loop {
            if s.expires_at.is_some_and(|exp| chrono::Utc::now().timestamp() >= exp) {
                return None;
            }
            if installation::authorize(&s.pool, &mut s.capability, s.peer.as_deref()).await.is_err()
                || authorize_feed(&s.capability, s.facet.as_deref()).is_err()
            {
                return None;
            }
            if let Some(change) = s.queue.pop_front() {
                let data = serde_json::to_string(&change).expect("change serializes");
                return Some((Ok(Event::default().event("change").data(data)), s));
            }
            match fetch_changes(&s.pool, s.cursor, s.facet.as_deref(), 500).await {
                Ok(rows) if rows.is_empty() => {
                    // Idle: wake on NOTIFY, or after a beat as a self-heal.
                    let _ = tokio::time::timeout(Duration::from_secs(1), s.listener.recv()).await;
                }
                Ok(rows) => {
                    s.cursor = rows.last().map(|r| r.seq).unwrap_or(s.cursor);
                    s.queue.extend(rows);
                }
                Err(_) => return None,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// The poker's endpoint. Emits a tick on the bus, then sweeps every facet
/// that declares a lapse rule. An item lapses at most once per edit: the
/// tick's own change row takes the append lock first, so overlapping pokes
/// run one at a time and the second one sees the first one's rows and finds
/// nothing to do. The core knows field *names* from facet data, never facet
/// semantics.
async fn tick(State(st): State<AppState>, cap: Capability, src: SourceParts) -> Result<Json<Value>> {
    cap.require("meta:system:tick")?;
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    let seq = record_change(&mut tx, None, SYSTEM_FACET, "tick", None, &source).await?;

    let rules: Vec<Value> =
        sqlx::query_scalar("SELECT body FROM items WHERE facet = $1 AND jsonb_exists(body, 'lapse')")
            .bind(FACET_FACET)
            .fetch_all(&mut *tx)
            .await?;
    let mut lapsed = 0i64;
    for rule in &rules {
        let (Some(facet), Some(due_field)) = (rule["name"].as_str(), rule["lapse"]["due"].as_str())
        else {
            continue;
        };
        let done_field = rule["lapse"]["done"].as_str().unwrap_or("");
        let fired: Vec<i64> = sqlx::query_scalar(
            "INSERT INTO changes (item_id, facet, op, body, revision, source)
             SELECT i.id, i.facet, 'lapsed', i.body, i.revision, $4 FROM items i
             WHERE i.facet = $1
               AND safe_ts(i.body ->> $2) <= now()
               AND NOT coalesce((i.body ->> $3) = 'true', false)
               AND NOT EXISTS (
                   SELECT 1 FROM changes c
                   WHERE c.item_id = i.id AND c.op = 'lapsed' AND c.at >= i.updated_at
               )
             RETURNING seq",
        )
        .bind(facet)
        .bind(due_field)
        .bind(done_field)
        .bind(&source)
        .fetch_all(&mut *tx)
        .await?;
        lapsed += fired.len() as i64;
        if let Some(max) = fired.iter().max() {
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(NOTIFY_CHANNEL)
                .bind(max.to_string())
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(Json(json!({ "seq": seq, "lapsed": lapsed })))
}

#[derive(Deserialize)]
struct MintRequest {
    grants: Vec<String>,
    /// How long the minted token lives. Required: the HTTP surface has no
    /// spelling for a token that never expires, because a stateless core
    /// cannot take one back. `erisdb mint --no-expiry` is the deliberate,
    /// secret-holding way to cut one.
    ttl_secs: i64,
    /// How long the minted token may keep refreshing itself. Defaults to
    /// `DEFAULT_CHAIN_SECS`, and is clamped to whatever is left of the
    /// minting token's own chain.
    max_ttl_secs: Option<i64>,
    /// Signed user identity for the minted token — attribution, not
    /// privilege, so enclosure ignores it. This is how agents get names.
    user: Option<String>,
}

async fn mint_capability(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Json(req): Json<MintRequest>,
) -> Result<impl IntoResponse> {
    st.check_mint_rate(&src.rate_key())?;
    cap.require("meta:capabilities:mint")?;
    auth::check_grants(&req.grants, req.user.as_deref())?;
    if req.ttl_secs <= 0 {
        return Err(Error::BadRequest("ttl_secs must be positive".into()));
    }
    let chain = req.max_ttl_secs.unwrap_or(auth::DEFAULT_CHAIN_SECS).max(req.ttl_secs);
    if chain > auth::MAX_CHAIN_SECS {
        return Err(Error::BadRequest(format!(
            "max_ttl_secs exceeds the {} second ceiling",
            auth::MAX_CHAIN_SECS
        )));
    }
    let exp = auth::deadline_from_now(req.ttl_secs)?;
    let mut chain_end = auth::deadline_from_now(chain)?;
    // An unasked-for chain never costs the caller a refusal: it takes
    // whatever is left of the parent's. An explicitly asked-for one is left
    // alone, so asking for more than the parent has is a refusal and not a
    // surprise.
    if req.max_ttl_secs.is_none() {
        if let Some(parent_end) = cap.deadline() {
            chain_end = chain_end.min(parent_end).max(exp);
        }
    }
    let minted = Capability {
        grants: req.grants,
        exp: Some(exp),
        user: req.user,
        max_exp: Some(chain_end),
        pair: None,
        client: cap.client,
    };
    // Enclosure covers time as well as scope, so a token that dies in a
    // minute cannot hand out one that outlives it.
    if !cap.encloses(&minted) {
        return Err(Error::Forbidden { permission: minted.grants.join(",") });
    }
    let token = auth::mint_capability(&st.secret, &minted)?;
    Ok((axum::http::StatusCode::CREATED, Json(json!({ "token": token }))))
}

#[derive(Deserialize)]
struct RefreshRequest {
    ttl_secs: i64,
}

/// Trade a still-valid token for one with the same scope and a fresh
/// expiry. Refresh moves time, not privilege: facets, verbs, and the
/// signed user carry over untouched, and no admin verb is needed — this
/// is how app tokens outlive their TTL without a human re-minting.
///
/// The line still ends. A refreshed token never reaches past `max_exp`,
/// so a leaked token buys the holder the rest of the chain and no more;
/// after that a human mints a new one. A token with no expiry has nothing
/// to move and is refused.
async fn refresh_capability(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Json(req): Json<RefreshRequest>,
) -> Result<impl IntoResponse> {
    st.check_mint_rate(&src.rate_key())?;
    if req.ttl_secs <= 0 {
        return Err(Error::BadRequest("ttl_secs must be positive".into()));
    }
    let wanted = auth::deadline_from_now(req.ttl_secs)?;
    // A non-expiring token has no chain to run out; refreshing it trades it
    // for a bounded one, which only ever narrows.
    let chain_end = cap.deadline().unwrap_or(wanted);
    let exp = wanted.min(chain_end);
    if exp <= chrono::Utc::now().timestamp() {
        return Err(Error::Unauthorized);
    }
    let fresh = Capability { exp: Some(exp), max_exp: Some(chain_end), ..cap };
    let token = auth::mint_capability(&st.secret, &fresh)?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "token": token, "exp": exp, "chain_ends": chain_end })),
    ))
}

// ---------------------------------------------------------------- pairing
//
// Pairing is a conversation between a client that wants permissions and a
// human who decides. The core holds the middle of it as an ordinary item in
// the `pair` facet, so nothing about it is special: it survives a restart,
// replicates, and lands on the change feed where a dashboard sees a request
// arrive live.
//
//     pending  ── redeem ──▶  requested  ── approve ──▶  approved
//                                        ── deny ─────▶  denied
//
// The code in the QR is a token holding exactly `meta:pairing:redeem` and
// naming its own session. It is not a capability over any data, so reading
// it off a screen buys nothing without a human answering the prompt.

/// How long a pairing code stays open. Long enough to walk to the other
/// device, short enough that a photographed screen goes stale.
pub const PAIR_TTL_SECS: i64 = 600;
/// The default life of the token an approval issues.
pub const PAIRED_TTL_SECS: i64 = 604_800;

async fn load_pairing(pool: &PgPool, id: Uuid) -> Result<Item> {
    sqlx::query_as::<_, Item>(
        "SELECT id, facet, body, revision, created_at, updated_at, source FROM items
         WHERE id = $1 AND facet = $2",
    )
    .bind(id)
    .bind(PAIR_FACET)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::NotFound)
}

/// Rewrite a session, revision-checked like any other item, so two
/// approvals racing each other cannot both win.
async fn write_pairing(st: &AppState, item: &Item, body: Value, source: &Value) -> Result<Item> {
    let mut tx = st.pool.begin().await?;
    check_against_facet(st, &mut tx, PAIR_FACET, &body).await?;
    let updated = write_revision(&mut tx, item.id, PAIR_FACET, &body, item.revision, source).await?;
    tx.commit().await?;
    Ok(updated)
}

fn pairing_is_live(body: &Value) -> Result<()> {
    let expires = body.get("expires").and_then(Value::as_i64).unwrap_or(0);
    if chrono::Utc::now().timestamp() >= expires {
        return Err(Error::BadRequest("this pairing code has expired".into()));
    }
    Ok(())
}

/// A session as an onlooker may see it: never the token, which belongs to
/// the client that redeemed the code and to nobody else.
fn public_pairing(item: &Item) -> Value {
    let mut body = item.body.clone();
    if let Some(map) = body.as_object_mut() {
        map.remove("token");
    }
    json!({ "id": item.id, "revision": item.revision, "created_at": item.created_at, "body": body })
}

#[derive(Deserialize)]
struct NewPairing {
    ttl_secs: Option<i64>,
}

async fn create_pairing(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    body: Option<Json<NewPairing>>,
) -> Result<impl IntoResponse> {
    cap.require("meta:pairing:create")?;
    let ttl = body.and_then(|Json(b)| b.ttl_secs).unwrap_or(PAIR_TTL_SECS).clamp(60, 3600);
    let expires = auth::deadline_from_now(ttl)?;
    let id = Uuid::new_v4();
    let source = src.stamp(&cap);
    let session = json!({ "status": "pending", "expires": expires });

    let mut tx = st.pool.begin().await?;
    check_against_facet(&st, &mut tx, PAIR_FACET, &session).await?;
    let item = sqlx::query_as::<_, Item>(
        "INSERT INTO items (id, facet, body, source) VALUES ($1, $2, $3, $4)
         RETURNING id, facet, body, revision, created_at, updated_at, source",
    )
    .bind(id)
    .bind(PAIR_FACET)
    .bind(&session)
    .bind(&source)
    .fetch_one(&mut *tx)
    .await?;
    record_change(&mut tx, Some(id), PAIR_FACET, "created", Some((&item.body, item.revision)), &source)
        .await?;
    tx.commit().await?;

    // The code itself: redeem, once, for this session, until it expires.
    let code = auth::mint_capability(
        &st.secret,
        &Capability {
            grants: vec!["meta:pairing:redeem".into()],
            exp: Some(expires),
            user: None,
            max_exp: Some(expires),
            pair: Some(id.to_string()),
            client: None,
        },
    )?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "id": id, "secret": code, "expires": expires })),
    ))
}

/// The session a pairing code names, or an error. A code that names nothing
/// is a forged one.
fn code_session(cap: &Capability) -> Result<Uuid> {
    cap.require("meta:pairing:redeem")?;
    cap.pair
        .as_deref()
        .and_then(|s| s.parse().ok())
        .ok_or(Error::Unauthorized)
}

#[derive(Deserialize)]
struct RedeemRequest {
    /// Who is asking. Shown to the human, trusted for nothing.
    client: String,
    /// The permissions the client would like. Asking is free; the answer
    /// is a person.
    requested: Vec<String>,
    /// Browser S256 commitment; native identity comes from the QUIC connection.
    challenge: Option<String>,
}

async fn redeem_pairing(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Json(req): Json<RedeemRequest>,
) -> Result<impl IntoResponse> {
    let id = code_session(&cap)?;
    if req.client.trim().is_empty() || req.client.len() > MAX_CLIENT_LEN || req.client.chars().any(char::is_control) {
        return Err(Error::BadRequest(format!("client must be 1..={MAX_CLIENT_LEN} characters")));
    }
    auth::check_grants(&req.requested, None)?;
    let identity = Identity::enrollment(src.addr.as_deref(), req.challenge)?;

    let item = load_pairing(&st.pool, id).await?;
    pairing_is_live(&item.body)?;
    match item.body.get("status").and_then(Value::as_str) {
        Some("pending") => {}
        Some(other) => {
            return Err(Error::Conflict(format!("this pairing code is already {other}")));
        }
        None => return Err(Error::Internal("pairing session has no status".into())),
    }

    let mut body = item.body.clone();
    body["status"] = json!("requested");
    body["client"] = json!(req.client);
    body["requested"] = json!(req.requested);
    body["fingerprint"] = json!(identity.fingerprint(id, &req.requested));
    body["identity"] = json!(identity);
    let updated = write_pairing(&st, &item, body, &src.stamp(&cap)).await?;
    tracing::info!(client = %req.client, pairing = %id, "pairing requested");
    Ok(Json(public_pairing(&updated)))
}

/// Collection is bound to the redeemer and safely repeatable after a lost
/// response. Only that installation can recover the issued access token.
async fn pairing_status(State(st): State<AppState>, cap: Capability, src: SourceParts) -> Result<Json<Value>> {
    let id = code_session(&cap)?;
    let mut tx = st.pool.begin().await?;
    let item = sqlx::query_as::<_, Item>(
        "SELECT id, facet, body, revision, created_at, updated_at, source FROM items
         WHERE id = $1 AND facet = $2 FOR UPDATE",
    ).bind(id).bind(PAIR_FACET).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    pairing_is_live(&item.body)?;
    let status = item.body["status"].as_str().ok_or(Error::Unauthorized)?;
    if status == "pending" {
        return Ok(Json(json!({ "status": status })));
    }
    let identity: Identity = serde_json::from_value(item.body["identity"].clone())
        .map_err(|_| Error::Unauthorized)?;
    identity.prove(src.addr.as_deref(), src.proof.as_deref())?;
    if !matches!(status, "approved" | "collected") {
        return Ok(Json(json!({ "status": status, "fingerprint": item.body["fingerprint"] })));
    }
    let source = src.stamp(&cap);
    let mut client_id = item.body["client_id"].as_str().and_then(|id| id.parse::<Uuid>().ok()).unwrap_or(id);
    if status == "approved" {
        // Serialize enrollment of one proof, including the initially empty case.
        // Existing row locks also serialize collection against permission/revoke writes.
        let identity_json = sqlx::types::Json(&identity);
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7))")
            .bind(serde_json::to_string(&identity).expect("identity serializes"))
            .execute(&mut *tx).await?;
        let current = sqlx::query_as::<_, Installation>(
            "SELECT * FROM clients WHERE identity = $1
             ORDER BY (revoked_at IS NULL) DESC, created_at DESC, id DESC LIMIT 1 FOR UPDATE",
        ).bind(identity_json).fetch_optional(&mut *tx).await?;
        let expected: Option<installation::Version> = serde_json::from_value(item.body["approval_anchor"].clone())
            .map_err(|_| Error::Unauthorized)?;
        if expected != current.as_ref().map(Installation::version) {
            return Err(Error::RevisionConflict);
        }
        let requested: Vec<String> = serde_json::from_value(item.body["requested"].clone())
            .map_err(|_| Error::Unauthorized)?;
        let grants: Vec<String> = serde_json::from_value(item.body["granted"].clone())
            .map_err(|_| Error::Unauthorized)?;
        let client = sqlx::query_as::<_, Installation>(
            "INSERT INTO clients (id, name, identity, requested, grants, user_name, access_ttl_secs, expires)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (identity) WHERE revoked_at IS NULL DO UPDATE SET
                 name = EXCLUDED.name, requested = EXCLUDED.requested, grants = EXCLUDED.grants,
                 user_name = EXCLUDED.user_name, access_ttl_secs = EXCLUDED.access_ttl_secs,
                 expires = EXCLUDED.expires, revision = clients.revision + 1
             RETURNING *",
        ).bind(id).bind(item.body["client"].as_str().ok_or(Error::Unauthorized)?)
            .bind(sqlx::types::Json(&identity)).bind(&requested).bind(&grants)
            .bind(item.body["user"].as_str())
            .bind(item.body["access_ttl_secs"].as_i64().ok_or(Error::Unauthorized)?)
            .bind(item.body["max_exp"].as_i64())
            .fetch_one(&mut *tx).await?;
        client_id = client.id;
        let mut body = item.body.clone();
        body["status"] = json!("collected");
        body["client_id"] = json!(client_id);
        write_revision(&mut tx, item.id, PAIR_FACET, &body, item.revision, &source).await?;
        audit_client(&mut tx, &client, if client.id == id { "created" } else { "updated" }, &source).await?;
    }
    let client = sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1")
        .bind(client_id).fetch_one(&mut *tx).await?;
    let issued = client.capability(None)?;
    let token = auth::mint_capability(&st.secret, &issued)?;
    tx.commit().await?;
    Ok(Json(json!({
        "status": "approved", "granted": issued.grants, "token": token,
        "client_id": client.id, "exp": issued.exp, "fingerprint": item.body["fingerprint"],
    })))
}

async fn list_pairings(State(st): State<AppState>, cap: Capability) -> Result<Json<Value>> {
    cap.require("meta:pairing:read")?;
    let rows = sqlx::query_as::<_, Item>(
        "SELECT id, facet, body, revision, created_at, updated_at, source FROM items
         WHERE facet = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(PAIR_FACET)
    .fetch_all(&st.pool)
    .await?;
    let pairings: Vec<Value> = rows.iter().map(public_pairing).collect();
    Ok(Json(json!({ "pairings": pairings })))
}

async fn get_pairing(State(st): State<AppState>, cap: Capability, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    cap.require("meta:pairing:read")?;
    Ok(Json(public_pairing(&load_pairing(&st.pool, id).await?)))
}

#[derive(Deserialize)]
struct ApproveRequest {
    /// What to actually grant. Omit to grant exactly what was asked for;
    /// pass a narrower set to approve part of a request.
    granted: Option<Vec<String>>,
    ttl_secs: Option<i64>,
    max_ttl_secs: Option<i64>,
    /// The signed identity the paired client writes as.
    user: Option<String>,
}

async fn approve_pairing(
    State(st): State<AppState>, cap: Capability, src: SourceParts,
    Path(id): Path<Uuid>, body: Option<Json<ApproveRequest>>,
) -> Result<Json<Value>> {
    cap.require("meta:pairing:approve")?;
    let req = body.map(|Json(b)| b).unwrap_or(ApproveRequest {
        granted: None, ttl_secs: None, max_ttl_secs: None, user: None,
    });
    let item = load_pairing(&st.pool, id).await?;
    pairing_is_live(&item.body)?;
    if item.body["status"] != "requested" {
        return Err(Error::Conflict("only a redeemed pairing code can be approved".into()));
    }
    let asked: Vec<String> = serde_json::from_value(item.body["requested"].clone())
        .map_err(|_| Error::Unauthorized)?;
    let granted = req.granted.unwrap_or_else(|| asked.clone());
    auth::check_grants(&granted, req.user.as_deref())?;
    if !permission::encloses(&asked, &granted) || !permission::encloses(&cap.grants, &granted) {
        return Err(Error::Forbidden { permission: granted.join(",") });
    }
    let ttl = req.ttl_secs.unwrap_or(PAIRED_TTL_SECS);
    if !(1..=installation::MAX_ACCESS_TTL).contains(&ttl) {
        return Err(Error::BadRequest(format!("ttl_secs must be 1..={}", installation::MAX_ACCESS_TTL)));
    }
    let identity: Identity = serde_json::from_value(item.body["identity"].clone()).map_err(|_| Error::Unauthorized)?;
    let previous = sqlx::query_as::<_, Installation>(
        "SELECT * FROM clients WHERE identity = $1
         ORDER BY (revoked_at IS NULL) DESC, created_at DESC, id DESC LIMIT 1",
    ).bind(sqlx::types::Json(identity)).fetch_optional(&st.pool).await?;
    let expires = req.max_ttl_secs.map(|secs| {
        if !(1..=auth::MAX_CHAIN_SECS).contains(&secs) {
            Err(Error::BadRequest("max_ttl_secs is outside the supported lifetime".into()))
        } else {
            auth::deadline_from_now(secs)
        }
    }).transpose()?;
    // Enrollment creates durable, revocable authority. Its optional explicit
    // deadline is independent of the token the administrator logged in with.
    let mut body = item.body.clone();
    body["status"] = json!("approved");
    body["granted"] = json!(granted);
    body["access_ttl_secs"] = json!(ttl);
    body["approval_anchor"] = json!(previous.as_ref().map(Installation::version));
    body["exp"] = json!(auth::deadline_from_now(ttl)?);
    if let Some(expires) = expires { body["max_exp"] = json!(expires); }
    if let Some(user) = req.user { body["user"] = json!(user); }
    let updated = write_pairing(&st, &item, body, &src.stamp(&cap)).await?;
    tracing::info!(pairing = %id, "pairing approved");
    Ok(Json(public_pairing(&updated)))
}

async fn deny_pairing(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    cap.require("meta:pairing:approve")?;
    let item = load_pairing(&st.pool, id).await?;
    // Denying an approved-but-uncollected session is a cancel, and it works:
    // the token does not exist until collection, so this stops it existing.
    // Once collected there is a live credential and denying is a lie.
    if item.body.get("status").and_then(Value::as_str) == Some("collected") {
        return Err(Error::Conflict(
            "this pairing was already collected; revoke its client registration instead".into(),
        ));
    }
    let mut body = item.body.clone();
    body["status"] = json!("denied");
    if let Some(map) = body.as_object_mut() {
        map.remove("granted");
        map.remove("exp");
        map.remove("max_exp");
    }
    let updated = write_pairing(&st, &item, body, &src.stamp(&cap)).await?;
    tracing::info!(pairing = %id, "pairing denied");
    Ok(Json(public_pairing(&updated)))
}

// ---------------------------------------------------------------- installations

async fn audit_client(
    tx: &mut Transaction<'_, Postgres>, client: &Installation, op: &str, source: &Value,
) -> Result<()> {
    let body = json!({ "event": "client", "client": client });
    record_change(tx, None, SYSTEM_FACET, op, Some((&body, client.revision)), source).await?;
    Ok(())
}

async fn list_clients(State(st): State<AppState>, cap: Capability) -> Result<Json<Value>> {
    cap.require("meta:clients:read")?;
    let clients = sqlx::query_as::<_, Installation>("SELECT * FROM clients ORDER BY created_at, id")
        .fetch_all(&st.pool).await?;
    Ok(Json(json!({ "clients": clients })))
}

async fn get_client(State(st): State<AppState>, cap: Capability, Path(id): Path<Uuid>) -> Result<Json<Installation>> {
    cap.require("meta:clients:read")?;
    let client = sqlx::query_as("SELECT * FROM clients WHERE id = $1")
        .bind(id).fetch_optional(&st.pool).await?.ok_or(Error::NotFound)?;
    Ok(Json(client))
}

#[derive(Deserialize)]
struct ClientPermissions { grants: Vec<String>, revision: i64 }

async fn update_client(
    State(st): State<AppState>, cap: Capability, src: SourceParts,
    Path(id): Path<Uuid>, Json(req): Json<ClientPermissions>,
) -> Result<Json<Installation>> {
    cap.require("meta:clients:write")?;
    auth::check_grants(&req.grants, None)?;
    if !permission::encloses(&cap.grants, &req.grants) {
        return Err(Error::Forbidden { permission: req.grants.join(",") });
    }
    let mut tx = st.pool.begin().await?;
    let current = sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1 FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    current.active()?;
    if !permission::encloses(&current.requested, &req.grants) {
        return Err(Error::Forbidden { permission: req.grants.join(",") });
    }
    let client = sqlx::query_as::<_, Installation>(
        "UPDATE clients SET grants = $2, revision = revision + 1 WHERE id = $1 AND revision = $3 RETURNING *",
    ).bind(id).bind(&req.grants).bind(req.revision)
        .fetch_optional(&mut *tx).await?.ok_or(Error::RevisionConflict)?;
    audit_client(&mut tx, &client, "updated", &src.stamp(&cap)).await?;
    tx.commit().await?;
    Ok(Json(client))
}

async fn revoke_client(
    State(st): State<AppState>, cap: Capability, src: SourceParts, Path(id): Path<Uuid>,
) -> Result<Json<Installation>> {
    cap.require("meta:clients:revoke")?;
    let mut tx = st.pool.begin().await?;
    let current = sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1 FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    if current.revoked_at.is_some() { return Ok(Json(current)); }
    let client = sqlx::query_as::<_, Installation>(
        "UPDATE clients SET revoked_at = now(), revision = revision + 1 WHERE id = $1 RETURNING *",
    ).bind(id).fetch_one(&mut *tx).await?;
    audit_client(&mut tx, &client, "updated", &src.stamp(&cap)).await?;
    tx.commit().await?;
    Ok(Json(client))
}

#[derive(Deserialize)]
struct ClientRenewal { ttl_secs: Option<i64> }

async fn renew_client(
    State(st): State<AppState>, src: SourceParts, Path(id): Path<Uuid>,
    Json(req): Json<ClientRenewal>,
) -> Result<Json<Value>> {
    st.check_mint_rate(&src.rate_key())?;
    let client = installation::authenticate(&st.pool, id, src.addr.as_deref(), src.proof.as_deref()).await?;
    let cap = client.capability(req.ttl_secs)?;
    let token = auth::mint_capability(&st.secret, &cap)?;
    Ok(Json(json!({ "token": token, "exp": cap.exp, "grants": cap.grants, "client_id": id })))
}

// ---------------------------------------------------------------- dashboard

/// What this token is. Needs no permission: a caller may always ask what it
/// already holds, and the answer tells it nothing it could not learn by
/// decoding its own token.
async fn my_permissions(cap: Capability) -> Json<Value> {
    Json(json!({
        "grants": cap.grants,
        "exp": cap.exp,
        "max_exp": cap.max_exp,
        "user": cap.user,
        "client_id": cap.client,
    }))
}

async fn server_state(State(st): State<AppState>, cap: Capability) -> Result<Json<Value>> {
    cap.require("meta:server:read")?;
    let head: Option<i64> = sqlx::query_scalar("SELECT max(seq) FROM changes").fetch_one(&st.pool).await?;
    let facets: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE facet = $1")
        .bind(FACET_FACET)
        .fetch_one(&st.pool)
        .await?;
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM items").fetch_one(&st.pool).await?;
    let changes: i64 = sqlx::query_scalar("SELECT count(*) FROM changes").fetch_one(&st.pool).await?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "feed_head": head.unwrap_or(0),
        "facets": facets,
        "items": items,
        "changes": changes,
        "limits": {
            "streams": MAX_STREAMS,
            "streams_free": st.streams.available_permits(),
            "iroh_connections": crate::net::MAX_CONNECTIONS,
            "client_name": MAX_CLIENT_LEN,
        },
    })))
}
