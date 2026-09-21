//! The v1 HTTP surface: items CRUD, the change feed, tick, and minting.
//!
//! Every handler is a pure function over (store, request). The process holds
//! no state a restart would lose; replicas are interchangeable.

use std::collections::HashMap;
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
use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::{self, Capability};
use crate::error::{Error, Result};
use crate::permission;
use crate::store::{Change, Item, Write};
use crate::installation::{self, Identity, Installation};
use crate::plugin::{PluginRegistry, MAX_PLUGIN_REQUEST_BYTES};

pub use crate::store::{FACET_FACET, NOTIFY_CHANNEL, PAIR_FACET, SYSTEM_FACET};

/// Live change streams allowed at once. Each one holds a Postgres connection
/// of its own, outside the pool, so this is a bound on the store as much as
/// on the process.
pub const MAX_STREAMS: usize = 32;

/// The longest `X-ErisDB-Client` the core will stamp. It lands in every change
/// row this caller writes, so an unbounded one is a way to grow the table.
pub const MAX_CLIENT_LEN: usize = 128;

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
    /// LISTEN holds a connection for the stream's lifetime, outside the request pool.
    listeners: PgPool,
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
        let listeners = PgPoolOptions::new().max_connections(MAX_STREAMS as u32)
            .idle_timeout(None).max_lifetime(None)
            .connect_lazy_with((*pool.connect_options()).clone());
        Self {
            pool,
            secret: Arc::new(secret),
            validators: Arc::new(Mutex::new(HashMap::new())),
            streams: Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS)),
            listeners,
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
// token, client is whatever the caller claims via X-ErisDB-Client.

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
        let client = match parts.headers.get("x-erisdb-client") {
            None => None,
            Some(v) => {
                let name = v.to_str().map_err(|_| {
                    Error::BadRequest("X-ErisDB-Client must be printable ASCII".into())
                })?;
                if name.len() > MAX_CLIENT_LEN {
                    return Err(Error::BadRequest(format!(
                        "X-ErisDB-Client is longer than {MAX_CLIENT_LEN}"
                    )));
                }
                if name.chars().any(|c| c.is_control()) {
                    return Err(Error::BadRequest("X-ErisDB-Client must be printable".into()));
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

// ---------------------------------------------------------------- facets

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

/// Look up the facet an incoming body claims and check the body against it.
async fn check_against_facet(
    st: &AppState,
    db: &mut PgConnection,
    facet: &str,
    body: &Value,
) -> Result<()> {
    // Registering a facet means storing a schema the core will later run,
    // and naming a permission namespace. Both are checked at the door.
    if facet == FACET_FACET {
        if let Some(schema) = body.get("schema") {
            crate::schema::compile(schema)?;
        }
    }
    let def: Value = sqlx::query_scalar(
        "SELECT body FROM items WHERE facet = $1 AND body ->> 'name' = $2",
    ).bind(FACET_FACET).bind(facet).fetch_optional(db).await?
        .ok_or_else(|| Error::UnknownFacet(facet.to_string()))?;
    if def.get("strict").and_then(Value::as_bool).unwrap_or(true) {
        let schema = def.get("schema").cloned().unwrap_or_else(|| json!({}));
        let validator = st.validator(facet, &schema)?;
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
    if req.facet == FACET_FACET && !cap.granted("meta:facets:write") {
        // Creating data in a namespace includes initializing its schema.
        // Validate the name before deriving authority; the unique index keeps
        // this insert-only. Changing or deleting a definition still needs admin.
        let name = req.body.get("name").and_then(Value::as_str)
            .ok_or_else(|| Error::BadRequest("facet name must be a string".into()))?;
        permission::check_facet_name(name)?;
        cap.require(&permission::for_facet(name, "create"))?;
    } else {
        cap.require(&permission::for_facet(&req.facet, "create"))?;
    }
    guard_core_facet(&req.facet, "create")?;
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    check_against_facet(&st, tx.db(), &req.facet, &req.body).await?;
    let item = tx.create(&req.facet, &req.body).await?;
    tx.commit().await?;
    Ok((axum::http::StatusCode::CREATED, Json(item)))
}

async fn get_item(
    State(st): State<AppState>,
    cap: Capability,
    Path(id): Path<Uuid>,
) -> Result<Json<Item>> {
    let item = Item::load(&st.pool, id).await?;
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
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current = tx.lock_item(id).await?;
    cap.require(&permission::for_facet(&current.facet, "update"))?;
    guard_core_facet(&current.facet, "update")?;
    check_against_facet(&st, tx.db(), &current.facet, &req.body).await?;
    let item = tx.update(id, &req.body, req.revision).await?;
    tx.commit().await?;
    Ok(Json(item))
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
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current = tx.lock_item(id).await?;
    cap.require(&permission::for_facet(&current.facet, "delete"))?;
    guard_core_facet(&current.facet, "delete")?;
    tx.delete(&current, q.revision).await?;
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
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current = tx.lock_item(id).await?;
    cap.require(&permission::for_facet(&current.facet, "update"))?;
    guard_core_facet(&current.facet, "update")?;
    let snapshot: Option<Option<Value>> =
        sqlx::query_scalar("SELECT body FROM changes WHERE seq = $1 AND item_id = $2")
            .bind(req.seq)
            .bind(id)
            .fetch_optional(tx.db())
            .await?;
    let body = snapshot
        .flatten()
        .ok_or_else(|| Error::BadRequest(format!("no snapshot at seq {} for this item", req.seq)))?;
    // The facet's schema may have tightened since the snapshot was live.
    check_against_facet(&st, tx.db(), &current.facet, &body).await?;
    let item = tx.update(id, &body, req.revision).await?;
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
    let changes = Change::since(&st.pool, q.since, q.facet.as_deref(), limit).await?;
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
    let mut listener = PgListener::connect_with(&st.listeners).await?;
    listener.listen(NOTIFY_CHANNEL).await?;

    let stream = async_stream::stream! {
        let _permit = permit;
        let mut capability = cap;
        let mut cursor = q.since;
        loop {
            let rows = match Change::since(&st.pool, cursor, q.facet.as_deref(), 500).await {
                Ok(rows) => rows,
                Err(_) => break,
            };
            // An idle stream must also close promptly after expiry or revocation.
            if !feed_is_live(&st.pool, &mut capability, &src, q.facet.as_deref()).await { break; }
            if rows.is_empty() {
                let _ = tokio::time::timeout(Duration::from_secs(1), listener.recv()).await;
            }
            for change in rows {
                if !feed_is_live(&st.pool, &mut capability, &src, q.facet.as_deref()).await { return; }
                cursor = change.seq;
                let data = serde_json::to_string(&change).expect("change serializes");
                yield Ok::<_, Infallible>(Event::default().event("change").data(data));
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn feed_is_live(pool: &PgPool, cap: &mut Capability, src: &SourceParts, facet: Option<&str>) -> bool {
    cap.exp.is_none_or(|exp| Utc::now().timestamp() < exp)
        && installation::authorize(pool, cap, src.addr.as_deref()).await.is_ok()
        && authorize_feed(cap, facet).is_ok()
}

/// Emit a tick and all newly lapsed item revisions in one transaction.
async fn tick(State(st): State<AppState>, cap: Capability, src: SourceParts) -> Result<Json<Value>> {
    cap.require("meta:system:tick")?;
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let (seq, lapsed) = tx.tick().await?;
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
/// after that a human mints a new one. Refreshing a token with no expiry
/// gives it the requested bounded lifetime.
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
    Item::load(pool, id).await?.in_facet(PAIR_FACET)
}

/// Rewrite a session, revision-checked like any other item, so two
/// approvals racing each other cannot both win.
async fn write_pairing(st: &AppState, item: &Item, body: Value, source: &Value) -> Result<Item> {
    let mut tx = Write::begin(&st.pool, source.clone()).await?;
    check_against_facet(st, tx.db(), PAIR_FACET, &body).await?;
    let updated = tx.update(item.id, &body, item.revision).await?;
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
    let session = json!({ "status": "pending", "expires": expires });
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    check_against_facet(&st, tx.db(), PAIR_FACET, &session).await?;
    let item = tx.create(PAIR_FACET, &session).await?;
    let id = item.id;
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
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let item = tx.lock_item(id).await?.in_facet(PAIR_FACET)?;
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
    let mut client_id = item.body["client_id"].as_str().and_then(|id| id.parse::<Uuid>().ok()).unwrap_or(id);
    if status == "approved" {
        // Serialize enrollment of one proof, including the initially empty case.
        // Existing row locks also serialize collection against permission/revoke writes.
        let identity_json = sqlx::types::Json(&identity);
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7))")
            .bind(serde_json::to_string(&identity).expect("identity serializes"))
            .execute(tx.db()).await?;
        let current = sqlx::query_as::<_, Installation>(
            "SELECT * FROM clients WHERE identity = $1
             ORDER BY (revoked_at IS NULL) DESC, created_at DESC, id DESC LIMIT 1 FOR UPDATE",
        ).bind(identity_json).fetch_optional(tx.db()).await?;
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
            .fetch_one(tx.db()).await?;
        client_id = client.id;
        let mut body = item.body.clone();
        body["status"] = json!("collected");
        body["client_id"] = json!(client_id);
        tx.update(item.id, &body, item.revision).await?;
        tx.audit_client(&client, if client.id == id { "created" } else { "updated" }).await?;
    }
    let client = sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1")
        .bind(client_id).fetch_one(tx.db()).await?;
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
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current = sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1 FOR UPDATE")
        .bind(id).fetch_optional(tx.db()).await?.ok_or(Error::NotFound)?;
    current.active()?;
    if !permission::encloses(&current.requested, &req.grants) {
        return Err(Error::Forbidden { permission: req.grants.join(",") });
    }
    let client = sqlx::query_as::<_, Installation>(
        "UPDATE clients SET grants = $2, revision = revision + 1 WHERE id = $1 AND revision = $3 RETURNING *",
    ).bind(id).bind(&req.grants).bind(req.revision)
        .fetch_optional(tx.db()).await?.ok_or(Error::RevisionConflict)?;
    tx.audit_client(&client, "updated").await?;
    tx.commit().await?;
    Ok(Json(client))
}

async fn revoke_client(
    State(st): State<AppState>, cap: Capability, src: SourceParts, Path(id): Path<Uuid>,
) -> Result<Json<Installation>> {
    cap.require("meta:clients:revoke")?;
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current = sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1 FOR UPDATE")
        .bind(id).fetch_optional(tx.db()).await?.ok_or(Error::NotFound)?;
    if current.revoked_at.is_some() { return Ok(Json(current)); }
    let client = sqlx::query_as::<_, Installation>(
        "UPDATE clients SET revoked_at = now(), revision = revision + 1 WHERE id = $1 RETURNING *",
    ).bind(id).fetch_one(tx.db()).await?;
    tx.audit_client(&client, "updated").await?;
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
    let (items, facets, changes, head): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM items),
                (SELECT count(*) FROM items WHERE facet = $1),
                count(*), coalesce(max(seq), 0) FROM changes",
    ).bind(FACET_FACET).fetch_one(&st.pool).await?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "feed_head": head,
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
