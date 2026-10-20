//! An installation is an app a device let in: its Iroh key, the device that
//! approved it, and the note it approved it with. The app's key is its
//! credential — over Iroh the peer is the app, so it calls with no token at
//! all. Its authority lasts while the registration is active, its device is
//! a member of the mesh and its note has not been revoked; the operator can
//! narrow its grants or revoke it outright, and the mesh revokes it when the
//! device leaves or the note is revoked.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use erisoverlaynetwork::Note;

use crate::auth::Capability;
use crate::error::{Error, Result};
use crate::mesh::Mesh;
use crate::permission;
use crate::store::Write;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Installation {
    pub id: Uuid,
    /// The app's name for itself, as the device approved it. Trusted for nothing.
    pub name: String,
    /// The app's Iroh endpoint id, hex.
    pub app: String,
    /// The device that approved it, hex.
    pub device: String,
    /// The id of the note it was approved with.
    pub note: String,
    /// What the note approved: the most the installation can ever hold.
    pub approved: Vec<String>,
    /// What it holds now.
    pub grants: Vec<String>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

impl Installation {
    /// Active, and still counting in the mesh: its device a member and its
    /// note not revoked. Checked live against the core's copy of the device
    /// list, so a removal stops an app the moment the list carries it.
    pub fn check(&self, mesh: Option<&Mesh>) -> Result<()> {
        if self.revoked_at.is_some() {
            return Err(Error::Unauthorized);
        }
        match mesh {
            Some(mesh) if mesh.counts(&self.device, &self.note) => Ok(()),
            _ => Err(Error::Unauthorized),
        }
    }

    /// The capability an installation calls with over Iroh: its grants, no
    /// expiry of its own, and its id for every write's source.
    pub fn capability(&self) -> Capability {
        Capability {
            grants: self.grants.clone(),
            exp: None,
            user: None,
            max_exp: None,
            client: Some(self.id),
        }
    }
}

/// The active installation for an app key.
pub async fn by_app(pool: &PgPool, app: &iroh::EndpointId) -> Result<Installation> {
    sqlx::query_as("SELECT * FROM clients WHERE app = $1 AND revoked_at IS NULL")
        .bind(app.to_string())
        .fetch_optional(pool)
        .await?
        .ok_or(Error::Unauthorized)
}

pub async fn load(pool: &PgPool, id: Uuid) -> Result<Installation> {
    sqlx::query_as("SELECT * FROM clients WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(Error::Unauthorized)
}

/// A token delegated by an installation holds no more than the installation
/// does now, and nothing once it stops counting.
pub async fn authorize(pool: &PgPool, mesh: Option<&Mesh>, cap: &mut Capability) -> Result<()> {
    if let Some(id) = cap.client {
        let client = load(pool, id).await?;
        client.check(mesh)?;
        cap.grants = permission::intersection(&cap.grants, &client.grants);
    }
    Ok(())
}

/// Enroll the app `note` approves: its active installation takes the note's
/// device, id and grants, or a new one is made with them. A revoked
/// installation stays revoked; enrolling again makes another.
pub async fn enroll(pool: &PgPool, note: &Note, source: serde_json::Value) -> Result<Installation> {
    let app = note.app().to_string();
    let mut tx = Write::begin(pool, source).await?;
    // Serialize enrollments of one app, its first included.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7))")
        .bind(&app)
        .execute(tx.db())
        .await?;
    let current: Option<Installation> =
        sqlx::query_as("SELECT * FROM clients WHERE app = $1 AND revoked_at IS NULL FOR UPDATE")
            .bind(&app)
            .fetch_optional(tx.db())
            .await?;
    let (client, op): (Installation, &str) = match current {
        Some(current) => (
            sqlx::query_as(
                "UPDATE clients SET name = $2, device = $3, note = $4, approved = $5, grants = $5,
                     revision = revision + 1
                 WHERE id = $1 RETURNING *",
            )
            .bind(current.id)
            .bind(&note.body.name)
            .bind(note.device().to_string())
            .bind(note.id.to_string())
            .bind(&note.body.grants)
            .fetch_one(tx.db())
            .await?,
            "updated",
        ),
        None => (
            sqlx::query_as(
                "INSERT INTO clients (id, name, app, device, note, approved, grants)
                 VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING *",
            )
            .bind(Uuid::new_v4())
            .bind(&note.body.name)
            .bind(&app)
            .bind(note.device().to_string())
            .bind(note.id.to_string())
            .bind(&note.body.grants)
            .fetch_one(tx.db())
            .await?,
            "created",
        ),
    };
    tx.audit_client(&client, op).await?;
    tx.commit().await?;
    Ok(client)
}
