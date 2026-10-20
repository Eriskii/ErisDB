//! ErisDB's place in its owner's mesh.
//!
//! `erisdb join` asks the device's ErisAuth to approve this core's Iroh key
//! as the app serving `erisdb`: the mesh's ErisDB. A mesh has one; approving
//! another revokes this one's note, and apps find the newest approved. The
//! note and the core's copy of the device list live in Postgres, and `erisdb
//! serve` runs a [`Node`] under that note, which keeps the list current:
//! devices push every addition, removal and revocation to it the moment they
//! make one.
//!
//! The list is what ErisDB trusts. A device in it may enroll apps and change
//! the passphrase, while this core's own note counts; an installation counts
//! while its device is in it and its note is not revoked in it. [`watch`]
//! revokes the installations a removal or a revocation ends, as soon as the
//! list carries it, and says so when this core's own note stops counting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use erisoverlaynetwork::devices::DeviceId;
use erisoverlaynetwork::mesh::{Local, Node, Saved, Store};
use erisoverlaynetwork::{Note, NoteId, Snapshot};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::error::{Error, Result};
use crate::installation::Installation;
use crate::store::Write;

/// The node's store: the one `mesh` row, beside the note it runs under.
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        PgStore { pool }
    }
}

fn stored(e: impl std::fmt::Display) -> erisoverlaynetwork::Error {
    erisoverlaynetwork::Error::Internal(format!("the mesh row: {e}"))
}

impl Store for PgStore {
    fn load(&self) -> BoxFuture<'static, erisoverlaynetwork::Result<Saved>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let row: Option<(Value, Value)> = sqlx::query_as("SELECT entries, directory FROM mesh")
                .fetch_optional(&pool)
                .await
                .map_err(stored)?;
            let Some((entries, directory)) = row else {
                return Ok(Saved::default());
            };
            Ok(Saved {
                entries: serde_json::from_value(entries).map_err(stored)?,
                directory: serde_json::from_value(directory).map_err(stored)?,
            })
        })
    }

    fn save(&self, saved: Saved) -> BoxFuture<'static, erisoverlaynetwork::Result<()>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            sqlx::query("UPDATE mesh SET entries = $1, directory = $2")
                .bind(serde_json::to_value(&saved.entries).map_err(stored)?)
                .bind(serde_json::to_value(&saved.directory).map_err(stored)?)
                .execute(&pool)
                .await
                .map_err(stored)?;
            Ok(())
        })
    }
}

/// The note this core joined its mesh under, once it has.
pub async fn joined(pool: &PgPool) -> Result<Option<Note>> {
    let note: Option<Value> = sqlx::query_scalar("SELECT note FROM mesh")
        .fetch_optional(pool)
        .await?;
    note.map(|note| {
        serde_json::from_value(note).map_err(|e| Error::Internal(format!("the stored note: {e}")))
    })
    .transpose()
}

/// Keep the note a device approved this core with, and the mesh as that
/// device showed it, in place of any it had before.
pub async fn join(pool: &PgPool, note: &Note, snapshot: &Snapshot) -> Result<()> {
    fn json(value: &impl serde::Serialize) -> Result<Value> {
        serde_json::to_value(value).map_err(|e| Error::Internal(e.to_string()))
    }
    sqlx::query(
        "INSERT INTO mesh (id, note, entries, directory) VALUES (true, $1, $2, $3)
         ON CONFLICT (id) DO UPDATE SET note = EXCLUDED.note, entries = EXCLUDED.entries,
             directory = EXCLUDED.directory, joined_at = now()",
    )
    .bind(json(note)?)
    .bind(json(&snapshot.entries)?)
    .bind(json(&snapshot.directory)?)
    .execute(pool)
    .await?;
    Ok(())
}

/// The running core's node, as the API consults it.
#[derive(Clone)]
pub struct Mesh {
    node: Node,
    /// Whether this core's own note counted at the last change to the list.
    counted: Arc<AtomicBool>,
}

impl Mesh {
    pub fn new(node: Node) -> Self {
        Mesh {
            node,
            counted: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn node(&self) -> &Node {
        &self.node
    }

    /// Whether this core is the mesh's ErisDB: its own note counts, so apps
    /// find it. A newer ErisDB's approval revokes it, as does its device
    /// leaving the mesh.
    pub fn is_erisdb(&self) -> bool {
        match self.node.local() {
            Local::App(note) => self.node.counts(note),
            Local::Device(_) => false,
        }
    }

    /// Whether `device` is in the mesh now.
    pub fn is_member(&self, device: &DeviceId) -> bool {
        self.node
            .with_list(|list| list.is_some_and(|list| list.is_member(device)))
    }

    /// Whether an installation approved by `device` with `note` still
    /// counts: the device a member, and the note not revoked.
    pub fn counts(&self, device: &str, note: &str) -> bool {
        let (Ok(device), Ok(note)) = (device.parse::<DeviceId>(), note.parse::<NoteId>()) else {
            return false;
        };
        self.node.with_list(|list| {
            list.is_some_and(|list| list.is_member(&device) && !list.is_revoked(&note))
        })
    }

    /// Whether `note` counts in the list as it stands, and why not.
    pub fn check(&self, note: &Note) -> erisoverlaynetwork::Result<()> {
        self.node.with_list(|list| match list {
            Some(list) => note.check(list),
            None => Err(erisoverlaynetwork::Error::Conflict(
                "this ErisDB is in no mesh".into(),
            )),
        })
    }
}

/// Revoke the installations the list has ended, now and on every change to
/// it, until the node stops.
pub async fn watch(mesh: Mesh, pool: PgPool) {
    let mut changes = mesh.node.subscribe();
    loop {
        if let Err(e) = sweep(&mesh, &pool).await {
            tracing::error!(error = %e, "revoking installations the mesh ended");
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// One pass: every active installation whose device the list removed, or
/// whose note it revoked, is revoked here too, each with its audit event.
async fn sweep(mesh: &Mesh, pool: &PgPool) -> Result<()> {
    let counts = mesh.is_erisdb();
    if mesh.counted.swap(counts, Ordering::SeqCst) != counts {
        if counts {
            tracing::info!(
                "this ErisDB's approval counts again: it is the mesh's ErisDB and lets apps in"
            );
        } else {
            tracing::error!(
                "this ErisDB's approval no longer counts: the mesh approved another ErisDB, or the device that \
                 approved this one left. Apps find the other one, and this one lets no more apps in; \
                 `erisdb join` makes it the mesh's ErisDB again"
            );
        }
    }
    let Some((removed, revoked)) = mesh.node.with_list(|list| {
        list.map(|list| {
            let state = list.state();
            let removed: Vec<String> = state.removed.iter().map(ToString::to_string).collect();
            let revoked: Vec<String> = state.revoked.iter().map(ToString::to_string).collect();
            (removed, revoked)
        })
    }) else {
        return Ok(());
    };
    for (why, column, ended) in [
        ("device removed", "device", removed),
        ("note revoked", "note", revoked),
    ] {
        if ended.is_empty() {
            continue;
        }
        let mut tx = Write::begin(pool, json!({ "mesh": why })).await?;
        // `column` is one of two literals above.
        let cut: Vec<Installation> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE clients SET revoked_at = now(), revision = revision + 1
             WHERE revoked_at IS NULL AND {column} = ANY($1) RETURNING *"
        )))
        .bind(&ended)
        .fetch_all(tx.db())
        .await?;
        for client in &cut {
            tx.audit_client(client, "updated").await?;
        }
        tx.commit().await?;
        for client in &cut {
            tracing::info!(installation = %client.id, app = %client.name, why, "revoked by the mesh");
        }
    }
    Ok(())
}
