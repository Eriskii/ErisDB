//! Items, immutable changes, and the transaction that writes them together.
//! HTTP authorization and schema validation stay at the API boundary.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgExecutor, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{error::{Error, Result}, installation::Installation};

/// Facets reserved for definitions, pairing sessions, and core events.
pub const FACET_FACET: &str = "facet";
pub const PAIR_FACET: &str = "pair";
pub const SYSTEM_FACET: &str = "system";
/// Postgres notifications wake subscribers after changes commit.
pub const NOTIFY_CHANNEL: &str = "erisdb_changes";

// A sequence allocates before commit. Serialize appends until commit so a
// cursor can never skip a lower sequence committed by a slower writer.
const CHANGES_LOCK: i64 = 0x0065_7269_7364_6201;

#[derive(Serialize, sqlx::FromRow)]
pub(crate) struct Item {
    pub id: Uuid,
    pub facet: String,
    pub body: Value,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub source: Option<Value>,
}

impl Item {
    pub async fn load<'e>(db: impl PgExecutor<'e>, id: Uuid) -> Result<Self> {
        sqlx::query_as("SELECT * FROM items WHERE id = $1")
            .bind(id).fetch_optional(db).await?.ok_or(Error::NotFound)
    }

    pub fn in_facet(self, facet: &str) -> Result<Self> {
        if self.facet == facet { Ok(self) } else { Err(Error::NotFound) }
    }
}

#[derive(Serialize, sqlx::FromRow)]
pub(crate) struct Change {
    pub seq: i64,
    pub item_id: Option<Uuid>,
    pub facet: String,
    pub op: String,
    pub at: DateTime<Utc>,
    pub body: Option<Value>,
    pub revision: Option<i64>,
    pub source: Option<Value>,
}

impl Change {
    pub async fn since(pool: &PgPool, cursor: i64, facet: Option<&str>, limit: i64) -> Result<Vec<Self>> {
        Ok(sqlx::query_as(
            "SELECT * FROM changes WHERE seq > $1 AND ($2::text IS NULL OR facet = $2)
             ORDER BY seq LIMIT $3",
        ).bind(cursor).bind(facet).bind(limit).fetch_all(pool).await?)
    }
}

/// A mutation carries its source once. Every item write appends its snapshot
/// before commit; pairing uses exactly the same revision and history rules.
pub(crate) struct Write {
    tx: Transaction<'static, Postgres>,
    source: Value,
}

impl Write {
    pub async fn begin(pool: &PgPool, source: Value) -> Result<Self> {
        Ok(Self { tx: pool.begin().await?, source })
    }

    pub fn db(&mut self) -> &mut PgConnection { &mut self.tx }

    pub async fn commit(self) -> Result<()> { Ok(self.tx.commit().await?) }

    pub async fn lock_item(&mut self, id: Uuid) -> Result<Item> {
        sqlx::query_as("SELECT * FROM items WHERE id = $1 FOR UPDATE")
            .bind(id).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    pub async fn create(&mut self, facet: &str, body: &Value) -> Result<Item> {
        let item: Item = sqlx::query_as(
            "INSERT INTO items (id, facet, body, source) VALUES ($1, $2, $3, $4) RETURNING *",
        ).bind(Uuid::new_v4()).bind(facet).bind(body).bind(&self.source)
            .fetch_one(&mut *self.tx).await?;
        self.snapshot(&item, "created").await?;
        Ok(item)
    }

    pub async fn update(&mut self, id: Uuid, body: &Value, revision: i64) -> Result<Item> {
        let item: Item = sqlx::query_as(
            "UPDATE items SET body = $1, revision = revision + 1, updated_at = now(), source = $2
             WHERE id = $3 AND revision = $4 RETURNING *",
        ).bind(body).bind(&self.source).bind(id).bind(revision)
            .fetch_optional(&mut *self.tx).await?.ok_or(Error::RevisionConflict)?;
        self.snapshot(&item, "updated").await?;
        Ok(item)
    }

    pub async fn delete(&mut self, item: &Item, revision: Option<i64>) -> Result<()> {
        let deleted = sqlx::query("DELETE FROM items WHERE id = $1 AND ($2::bigint IS NULL OR revision = $2)")
            .bind(item.id).bind(revision).execute(self.db()).await?;
        if deleted.rows_affected() == 0 { return Err(Error::RevisionConflict); }
        self.record(Some(item.id), &item.facet, "deleted", None).await?;
        Ok(())
    }

    async fn snapshot(&mut self, item: &Item, op: &str) -> Result<()> {
        self.record(Some(item.id), &item.facet, op, Some((&item.body, item.revision))).await?;
        Ok(())
    }

    pub async fn audit_client(&mut self, client: &Installation, op: &str) -> Result<()> {
        let body = json!({ "event": "client", "client": client });
        self.record(None, SYSTEM_FACET, op, Some((&body, client.revision))).await?;
        Ok(())
    }

    async fn record(&mut self, id: Option<Uuid>, facet: &str, op: &str, snapshot: Option<(&Value, i64)>) -> Result<i64> {
        // Always take item/client locks before this append lock.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CHANGES_LOCK).execute(self.db()).await?;
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO changes (item_id, facet, op, body, revision, source)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING seq",
        ).bind(id).bind(facet).bind(op).bind(snapshot.map(|(b, _)| b))
            .bind(snapshot.map(|(_, r)| r)).bind(&self.source).fetch_one(&mut *self.tx).await?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(NOTIFY_CHANNEL).bind(seq.to_string()).execute(self.db()).await?;
        Ok(seq)
    }

    pub async fn tick(&mut self) -> Result<(i64, u64)> {
        let seq = self.record(None, SYSTEM_FACET, "tick", None).await?;
        // The tick's append lock serializes sweeps; its notification wakes
        // subscribers only after every lapse has committed. Revisions, unlike
        // transaction timestamps, identify edits even when writers overlap.
        let lapsed = sqlx::query(
            "INSERT INTO changes (item_id, facet, op, body, revision, source)
             SELECT i.id, i.facet, 'lapsed', i.body, i.revision, $1
             FROM items i JOIN items f ON f.facet = $2 AND f.body ->> 'name' = i.facet
             WHERE safe_ts(i.body ->> (f.body #>> '{lapse,due}')) <= now()
               AND NOT coalesce((i.body ->> coalesce(f.body #>> '{lapse,done}', '')) = 'true', false)
               AND NOT EXISTS (
                   SELECT 1 FROM changes c
                   WHERE c.item_id = i.id AND c.op = 'lapsed' AND c.revision = i.revision
               )",
        ).bind(&self.source).bind(FACET_FACET).execute(&mut *self.tx).await?.rows_affected();
        Ok((seq, lapsed))
    }
}
