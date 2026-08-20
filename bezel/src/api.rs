//! The v1 HTTP surface: items CRUD, the change feed, tick, and minting.
//!
//! Every handler is a pure function over (store, request). The process holds
//! no state a restart would lose; replicas are interchangeable.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequestParts, Path, Query, State};
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

use crate::auth::{self, Capability, Verb};
use crate::error::{Error, Result};

/// Facet reserved for core-emitted events (tick).
pub const SYSTEM_FACET: &str = "system";
/// The meta-facet holding facet definitions.
pub const FACET_FACET: &str = "facet";
/// Postgres NOTIFY channel fanned out to change-stream subscribers.
pub const NOTIFY_CHANNEL: &str = "bezel_changes";

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub secret: Arc<Vec<u8>>,
}

pub fn app(pool: PgPool, secret: Vec<u8>) -> Router {
    let state = AppState { pool, secret: Arc::new(secret) };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/items", post(create_item).get(list_items))
        .route("/v1/items/{id}", get(get_item).put(update_item).delete(delete_item))
        .route("/v1/items/{id}/history", get(item_history))
        .route("/v1/items/{id}/revert", post(revert_item))
        .route("/v1/changes", get(list_changes))
        .route("/v1/changes/stream", get(stream_changes))
        .route("/v1/tick", post(tick))
        .route("/v1/capabilities", post(mint_capability))
        // Browser clients are first-class; auth is the token, not the origin.
        .layer(tower_http::cors::CorsLayer::permissive())
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
        auth::verify(&state.secret, token)
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

/// The connection-and-request half of a source; the capability supplies
/// the user.
struct SourceParts {
    addr: Option<String>,
    client: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for SourceParts {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> std::result::Result<Self, Infallible> {
        let addr = parts
            .extensions
            .get::<PeerAddr>()
            .map(|p| p.0.clone())
            .or_else(|| {
                parts
                    .extensions
                    .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                    .map(|ci| ci.0.to_string())
            });
        let client = parts
            .headers
            .get("x-bezel-client")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(SourceParts { addr, client })
    }
}

impl SourceParts {
    /// The stamped source value: connection + token, never the body.
    fn stamp(&self, cap: &Capability) -> Value {
        json!({ "addr": self.addr, "user": cap.user, "client": self.client })
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

fn validate(facet: &str, def: &FacetDef, body: &Value) -> Result<()> {
    if !def.strict {
        return Ok(());
    }
    let validator = jsonschema::validator_for(&def.schema)
        .map_err(|e| Error::Internal(format!("facet {facet} carries an invalid schema: {e}")))?;
    match validator.validate(body) {
        Ok(()) => Ok(()),
        Err(e) => Err(Error::SchemaViolation { facet: facet.to_string(), detail: e.to_string() }),
    }
}

/// Look up the facet an incoming body claims and check the body against it.
async fn check_against_facet(
    tx: &mut Transaction<'_, Postgres>,
    facet: &str,
    body: &Value,
) -> Result<()> {
    let def = load_facet(tx, facet)
        .await?
        .ok_or_else(|| Error::UnknownFacet(facet.to_string()))?;
    validate(facet, &def, body)
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
        Some(f) => cap.require(f, Verb::Read),
        None => {
            if cap.facets.iter().any(|f| f == "*") && cap.has_verb(Verb::Read) {
                Ok(())
            } else {
                Err(Error::Forbidden { facet: "*".into(), verb: "read".into() })
            }
        }
    }
}

// ---------------------------------------------------------------- handlers

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
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
    cap.require(&req.facet, Verb::Write)?;
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    check_against_facet(&mut tx, &req.facet, &req.body).await?;
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
    cap.require(&item.facet, Verb::Read)?;
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
    cap.require(&q.facet, Verb::Read)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let items = sqlx::query_as::<_, Item>(
        "SELECT id, facet, body, revision, created_at, updated_at, source FROM items
         WHERE facet = $1 AND ($2::timestamptz IS NULL OR updated_at > $2)
         ORDER BY updated_at LIMIT $3",
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
    cap.require(&facet, Verb::Write)?;
    check_against_facet(&mut tx, &facet, &req.body).await?;
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

async fn delete_item(
    State(st): State<AppState>,
    cap: Capability,
    src: SourceParts,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode> {
    let source = src.stamp(&cap);
    let mut tx = st.pool.begin().await?;
    let facet: String = sqlx::query_scalar("SELECT facet FROM items WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
    cap.require(&facet, Verb::Write)?;
    sqlx::query("DELETE FROM items WHERE id = $1").bind(id).execute(&mut *tx).await?;
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
        sqlx::query_scalar("SELECT facet FROM changes WHERE item_id = $1 LIMIT 1")
            .bind(id)
            .fetch_optional(&st.pool)
            .await?;
    let facet = facet.ok_or(Error::NotFound)?;
    cap.require(&facet, Verb::Read)?;
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
    cap.require(&facet, Verb::Write)?;
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
    check_against_facet(&mut tx, &facet, &body).await?;
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
    Query(q): Query<ChangesQuery>,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    authorize_feed(&cap, q.facet.as_deref())?;
    let mut listener = PgListener::connect_with(&st.pool).await?;
    listener.listen(NOTIFY_CHANNEL).await?;

    struct Feed {
        pool: PgPool,
        listener: PgListener,
        facet: Option<String>,
        cursor: i64,
        queue: VecDeque<Change>,
    }
    let feed = Feed {
        pool: st.pool.clone(),
        listener,
        facet: q.facet,
        cursor: q.since,
        queue: VecDeque::new(),
    };

    let stream = futures::stream::unfold(feed, |mut s| async move {
        loop {
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
/// that declares a lapse rule. The sweep is idempotent and re-entrant: an
/// item lapses at most once per edit, so overlapping or duplicate pokes
/// find nothing to do. The core knows field *names* from facet data, never
/// facet semantics.
async fn tick(State(st): State<AppState>, cap: Capability, src: SourceParts) -> Result<Json<Value>> {
    cap.require(SYSTEM_FACET, Verb::Write)?;
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
    facets: Vec<String>,
    verbs: Vec<String>,
    ttl_secs: Option<i64>,
    /// Signed user identity for the minted token — attribution, not
    /// privilege, so enclosure ignores it. This is how agents get names.
    user: Option<String>,
}

async fn mint_capability(
    State(st): State<AppState>,
    cap: Capability,
    Json(req): Json<MintRequest>,
) -> Result<impl IntoResponse> {
    if !cap.has_verb(Verb::Admin) {
        return Err(Error::Forbidden { facet: "*".into(), verb: "admin".into() });
    }
    let minted = Capability {
        facets: req.facets,
        verbs: req.verbs,
        exp: req.ttl_secs.map(|t| chrono::Utc::now().timestamp() + t),
        user: req.user,
    };
    if !cap.encloses(&minted) {
        return Err(Error::Forbidden { facet: minted.facets.join(","), verb: minted.verbs.join(",") });
    }
    let token = auth::mint_capability(&st.secret, &minted)?;
    Ok((axum::http::StatusCode::CREATED, Json(json!({ "token": token }))))
}
