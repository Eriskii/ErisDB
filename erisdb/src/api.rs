//! The v1 HTTP surface: items CRUD, the change feed, tick, minting,
//! installations, and enrollment.
//!
//! Every handler is a pure function over (store, mesh, request). The process
//! holds no state a restart would lose; replicas are interchangeable.

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

use erisoverlaynetwork::devices::DeviceId;
use erisoverlaynetwork::Note;

use crate::auth::{self, Capability};
use crate::error::{Error, Result};
use crate::installation::{self, Installation};
use crate::mesh::Mesh;
use crate::net::IrohPeer;
use crate::passphrase;
use crate::permission;
use crate::plugin::{PluginRegistry, MAX_PLUGIN_REQUEST_BYTES};
use crate::store::{Change, Item, Write};

pub use crate::store::{FACET_FACET, NOTIFY_CHANNEL, SYSTEM_FACET};

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
    /// The core's node in its owner's mesh: who the devices are, which
    /// notes are revoked. A core without one serves bearer tokens only.
    mesh: Option<Mesh>,
    /// The first wait after a device's third wrong passphrase.
    lockout: Duration,
}

impl AppState {
    pub fn new(pool: PgPool, secret: Vec<u8>) -> Self {
        let listeners = PgPoolOptions::new()
            .max_connections(MAX_STREAMS as u32)
            .idle_timeout(None)
            .max_lifetime(None)
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
            plugins: PluginRegistry::empty(),
            mesh: None,
            lockout: passphrase::LOCKOUT,
        }
    }

    pub fn with_plugins(self, plugins: PluginRegistry) -> Self {
        Self { plugins, ..self }
    }

    pub fn with_mesh(self, mesh: Mesh) -> Self {
        Self {
            mesh: Some(mesh),
            ..self
        }
    }

    /// Shorten or lengthen the first lockout; the schedule after it is fixed.
    pub fn with_lockout(self, first: Duration) -> Self {
        Self {
            lockout: first,
            ..self
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/v1/health", get(health))
            .route("/v1/plugins", get(list_plugins))
            .route(
                "/v1/call",
                post(call_plugin).layer(DefaultBodyLimit::max(MAX_PLUGIN_REQUEST_BYTES)),
            )
            .route("/v1/items", post(create_item).get(list_items))
            .route(
                "/v1/items/{id}",
                get(get_item).put(update_item).delete(delete_item),
            )
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
            .route("/v1/enroll", post(enroll))
            .route("/v1/passphrase", post(change_passphrase))
            .route("/v1/server", get(server_state))
            // Browser clients are first-class; auth is the token, not the origin.
            .layer(tower_http::cors::CorsLayer::permissive())
            .layer(tower_http::trace::TraceLayer::new_for_http())
            .layer(axum::middleware::map_response(
                |mut response: axum::response::Response| async {
                    response.headers_mut().insert(
                        axum::http::header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static("no-store"),
                    );
                    response
                },
            ))
            .with_state(self)
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
        Self {
            buckets: HashMap::new(),
            burst,
            per_sec,
        }
    }

    fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        if self.buckets.len() > 4096 {
            self.buckets
                .retain(|_, (seen, _)| now.duration_since(*seen) < Duration::from_secs(300));
        }
        let bucket = self
            .buckets
            .entry(key.to_string())
            .or_insert((now, self.burst));
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

/// A core over TCP with no mesh and no plugins: bearer tokens only.
pub fn app(pool: PgPool, secret: Vec<u8>) -> Router {
    AppState::new(pool, secret).router()
}

pub fn app_with_plugins(pool: PgPool, secret: Vec<u8>, plugins: PluginRegistry) -> Router {
    AppState::new(pool, secret).with_plugins(plugins).router()
}

// ---------------------------------------------------------------- auth

/// A request carries a bearer token, or arrives over Iroh from an enrolled
/// app's key. The token wins when both are there: an app may act with a
/// narrower token it was handed.
impl FromRequestParts<AppState> for Capability {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // A refused credential is the one thing an operator most wants a
        // record of, and the only trace of someone probing.
        let refused = |what: &str| {
            tracing::warn!(
                path = %parts.uri.path(),
                peer = parts.extensions.get::<PeerAddr>().map(|p| p.0.as_str()).unwrap_or("?"),
                "rejected {what}"
            );
        };
        let Some(header) = parts.headers.get(axum::http::header::AUTHORIZATION) else {
            let peer = parts
                .extensions
                .get::<IrohPeer>()
                .ok_or(Error::Unauthorized)?;
            let client = installation::by_app(&state.pool, &peer.0)
                .await
                .inspect_err(|_| refused("an unenrolled key"))?;
            client
                .check(state.mesh.as_ref())
                .inspect_err(|_| refused("an app the mesh no longer counts"))?;
            return Ok(client.capability());
        };
        let token = header
            .to_str()
            .ok()
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or(Error::Unauthorized)?;
        let mut cap =
            auth::verify(&state.secret, token).inspect_err(|_| refused("capability token"))?;
        installation::authorize(&state.pool, state.mesh.as_ref(), &mut cap).await?;
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
    parts
        .extensions
        .get::<PeerAddr>()
        .map(|p| p.0.clone())
        .or_else(|| {
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|address| address.0.to_string())
        })
}

/// The connection-and-request half of a source; the capability supplies
/// the user.
struct SourceParts {
    addr: Option<String>,
    client: Option<String>,
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
                    return Err(Error::BadRequest(
                        "X-ErisDB-Client must be printable".into(),
                    ));
                }
                Some(name.to_string())
            }
        };
        Ok(SourceParts { addr, client })
    }
}

impl SourceParts {
    /// The stamped source value: connection + token, never the body.
    fn stamp(&self, cap: &Capability) -> Value {
        json!({ "addr": self.addr, "user": cap.user, "client": self.client, "installation": cap.client })
    }

    /// The source of what a device does in its own name.
    fn stamp_device(&self) -> Value {
        json!({ "addr": self.addr, "user": null, "client": self.client, "installation": null })
    }

    /// The key a rate limit counts against: the observed address when there
    /// is one, since that is the rung a caller cannot forge.
    fn rate_key(&self) -> String {
        let addr = self.addr.as_deref().unwrap_or("unknown");
        addr.parse::<std::net::SocketAddr>()
            .map(|p| p.ip().to_string())
            .unwrap_or_else(|_| addr.to_string())
    }
}

// ---------------------------------------------------------------- facets

/// The core writes its own event facet itself, so the item routes refuse
/// to: no grant is a way to forge a tick or an installation's audit event.
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
    let def: Value =
        sqlx::query_scalar("SELECT body FROM items WHERE facet = $1 AND body ->> 'name' = $2")
            .bind(FACET_FACET)
            .bind(facet)
            .fetch_optional(db)
            .await?
            .ok_or_else(|| Error::UnknownFacet(facet.to_string()))?;
    if def.get("strict").and_then(Value::as_bool).unwrap_or(true) {
        let schema = def.get("schema").cloned().unwrap_or_else(|| json!({}));
        let validator = st.validator(facet, &schema)?;
        if let Err(e) = validator.validate(body) {
            return Err(Error::SchemaViolation {
                facet: facet.to_string(),
                detail: e.to_string(),
            });
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
    st.plugins
        .invoke(&st.secret, &cap, &call.plugin, &call.operation, call.input)
        .await
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
        let name = req
            .body
            .get("name")
            .and_then(Value::as_str)
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
    let body = snapshot.flatten().ok_or_else(|| {
        Error::BadRequest(format!("no snapshot at seq {} for this item", req.seq))
    })?;
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
            if !feed_is_live(&st, &mut capability, q.facet.as_deref()).await { break; }
            if rows.is_empty() {
                let _ = tokio::time::timeout(Duration::from_secs(1), listener.recv()).await;
            }
            for change in rows {
                if !feed_is_live(&st, &mut capability, q.facet.as_deref()).await { return; }
                cursor = change.seq;
                let data = serde_json::to_string(&change).expect("change serializes");
                yield Ok::<_, Infallible>(Event::default().event("change").data(data));
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Whether a stream may carry on: its capability unexpired, and its
/// installation, when it has one, still counting with its current grants —
/// so an operator's revocation or a device's removal closes it.
async fn feed_is_live(st: &AppState, cap: &mut Capability, facet: Option<&str>) -> bool {
    cap.exp.is_none_or(|exp| Utc::now().timestamp() < exp)
        && installation::authorize(&st.pool, st.mesh.as_ref(), cap)
            .await
            .is_ok()
        && authorize_feed(cap, facet).is_ok()
}

/// Emit a tick and all newly lapsed item revisions in one transaction.
async fn tick(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
) -> Result<Json<Value>> {
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
    let chain = req
        .max_ttl_secs
        .unwrap_or(auth::DEFAULT_CHAIN_SECS)
        .max(req.ttl_secs);
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
        client: cap.client,
    };
    // Enclosure covers time as well as scope, so a token that dies in a
    // minute cannot hand out one that outlives it.
    if !cap.encloses(&minted) {
        return Err(Error::Forbidden {
            permission: minted.grants.join(","),
        });
    }
    let token = auth::mint_capability(&st.secret, &minted)?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "token": token })),
    ))
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
///
/// Refresh trades a token for a token. An app calling by its key holds
/// none, and does not get one this way: handing out tokens is minting.
async fn refresh_capability(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    headers: axum::http::HeaderMap,
    Json(req): Json<RefreshRequest>,
) -> Result<impl IntoResponse> {
    st.check_mint_rate(&src.rate_key())?;
    if !headers.contains_key(axum::http::header::AUTHORIZATION) {
        return Err(Error::BadRequest(
            "refresh trades a bearer token, and this request carries none".into(),
        ));
    }
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
    let fresh = Capability {
        exp: Some(exp),
        max_exp: Some(chain_end),
        ..cap
    };
    let token = auth::mint_capability(&st.secret, &fresh)?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "token": token, "exp": exp, "chain_ends": chain_end })),
    ))
}

// ---------------------------------------------------------------- installations

async fn list_clients(State(st): State<AppState>, cap: Capability) -> Result<Json<Value>> {
    cap.require("meta:clients:read")?;
    let clients =
        sqlx::query_as::<_, Installation>("SELECT * FROM clients ORDER BY created_at, id")
            .fetch_all(&st.pool)
            .await?;
    Ok(Json(json!({ "clients": clients })))
}

async fn get_client(
    State(st): State<AppState>,
    cap: Capability,
    Path(id): Path<Uuid>,
) -> Result<Json<Installation>> {
    cap.require("meta:clients:read")?;
    let client = sqlx::query_as("SELECT * FROM clients WHERE id = $1")
        .bind(id)
        .fetch_optional(&st.pool)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(client))
}

#[derive(Deserialize)]
struct ClientPermissions {
    grants: Vec<String>,
    revision: i64,
}

/// Narrow, or widen again, what an installation holds — never past what
/// its device's note approved, nor past the operator's own grants.
async fn update_client(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
    Json(req): Json<ClientPermissions>,
) -> Result<Json<Installation>> {
    cap.require("meta:clients:write")?;
    auth::check_grants(&req.grants, None)?;
    if !permission::encloses(&cap.grants, &req.grants) {
        return Err(Error::Forbidden {
            permission: req.grants.join(","),
        });
    }
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current =
        sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(tx.db())
            .await?
            .ok_or(Error::NotFound)?;
    if current.revoked_at.is_some() {
        return Err(Error::Conflict(
            "this installation is revoked; its app enrolls again through a device".into(),
        ));
    }
    if !permission::encloses(&current.approved, &req.grants) {
        return Err(Error::Forbidden {
            permission: req.grants.join(","),
        });
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
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
) -> Result<Json<Installation>> {
    cap.require("meta:clients:revoke")?;
    let mut tx = Write::begin(&st.pool, src.stamp(&cap)).await?;
    let current =
        sqlx::query_as::<_, Installation>("SELECT * FROM clients WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(tx.db())
            .await?
            .ok_or(Error::NotFound)?;
    if current.revoked_at.is_some() {
        return Ok(Json(current));
    }
    let client = sqlx::query_as::<_, Installation>(
        "UPDATE clients SET revoked_at = now(), revision = revision + 1 WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(tx.db())
    .await?;
    tx.audit_client(&client, "updated").await?;
    tx.commit().await?;
    Ok(Json(client))
}

// ---------------------------------------------------------------- enrollment
//
// Apps get into ErisDB through a device. The device approves the app with a
// note, the person types the passphrase on the device, and the device brings
// both here, calling as itself over Iroh. From then on the app calls with
// its own key and nothing else.

/// The device calling: a member of this core's mesh, calling over Iroh as
/// itself, while this core is the mesh's ErisDB. Once a newer ErisDB is
/// approved, apps find that one, and this one lets none in.
fn calling_device(st: &AppState, peer: Option<&IrohPeer>) -> Result<DeviceId> {
    let mesh = st.mesh.as_ref().ok_or(Error::NotJoined)?;
    if !mesh.is_erisdb() {
        return Err(Error::NotTheErisDB);
    }
    let peer =
        peer.ok_or_else(|| Error::NotADevice("a device calls as itself, over Iroh".into()))?;
    if !mesh.is_member(&peer.0) {
        return Err(Error::NotADevice(
            "this key is not a device in ErisDB's mesh".into(),
        ));
    }
    Ok(peer.0)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollRequest {
    note: Value,
    passphrase: String,
}

/// Let in the app a device's note approves, when the person typed the
/// passphrase: create its installation, or give the one it has the note's
/// grants.
async fn enroll(
    State(st): State<AppState>,
    peer: Option<axum::Extension<IrohPeer>>,
    src: SourceParts,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<Value>> {
    let device = calling_device(&st, peer.as_deref())?;
    let note: Note = serde_json::from_value(req.note).map_err(|e| Error::BadNote(e.to_string()))?;
    if note.device() != device {
        return Err(Error::NotADevice(
            "a device enrolls apps with its own notes".into(),
        ));
    }
    let mesh = st.mesh.as_ref().ok_or(Error::NotJoined)?;
    mesh.check(&note)
        .map_err(|e| Error::BadNote(e.to_string()))?;
    if note.body.service != crate::SERVICE {
        return Err(Error::BadNote(format!(
            "it approves {:?}, not {:?}",
            note.body.service,
            crate::SERVICE
        )));
    }
    if note.body.serves {
        return Err(Error::BadNote(
            "it approves an app to serve erisdb; ErisDB enrolls apps that call it".into(),
        ));
    }
    passphrase::check(&st.pool, &device, &req.passphrase, st.lockout).await?;
    let client = installation::enroll(&st.pool, &note, src.stamp_device()).await?;
    tracing::info!(installation = %client.id, app = %client.name, device = %device, "enrolled");
    Ok(Json(
        json!({ "client": client.id, "grants": client.grants }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PassphraseChange {
    current: String,
    new: String,
}

/// A member device changes the passphrase by knowing it. A wrong `current`
/// counts against the device like any other wrong guess.
async fn change_passphrase(
    State(st): State<AppState>,
    peer: Option<axum::Extension<IrohPeer>>,
    Json(req): Json<PassphraseChange>,
) -> Result<axum::http::StatusCode> {
    let device = calling_device(&st, peer.as_deref())?;
    passphrase::validate(&req.new)?;
    passphrase::check(&st.pool, &device, &req.current, st.lockout).await?;
    passphrase::set(&st.pool, &req.new).await?;
    tracing::info!(device = %device, "passphrase changed");
    Ok(axum::http::StatusCode::NO_CONTENT)
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
    )
    .bind(FACET_FACET)
    .fetch_one(&st.pool)
    .await?;
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
