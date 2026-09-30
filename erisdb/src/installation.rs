//! An installation is durable authority; a pairing is only its enrollment.
//! Native installations prove possession of their Iroh key through QUIC.
//! Browsers use a random 256-bit renewal secret over HTTPS, stored here only
//! as its S256 commitment. Neither display names nor caller-supplied IDs
//! authenticate an installation. [`Identity`] is [`erislogin`]'s.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{types::Json, PgPool};
use uuid::Uuid;

use crate::{auth::Capability, error::{Error, Result}, permission};

pub const MAX_ACCESS_TTL: i64 = 7 * 86_400;
pub const PROOF_HEADER: &str = "x-erisdb-client-proof";

pub use erislogin::identity::Identity;

/// The tag in every ErisDB pairing fingerprint, so that no other app's
/// fingerprints match one of ours.
pub const PAIR_DOMAIN: &str = "erisdb-pair-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub id: Uuid,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Installation {
    pub id: Uuid,
    pub name: String,
    pub identity: Json<Identity>,
    pub requested: Vec<String>,
    pub grants: Vec<String>,
    pub user_name: Option<String>,
    pub access_ttl_secs: i64,
    pub expires: Option<i64>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

impl Installation {
    pub fn version(&self) -> Version { Version { id: self.id, revision: self.revision } }

    pub fn active(&self) -> Result<()> {
        if self.revoked_at.is_some() || self.expires.is_some_and(|end| Utc::now().timestamp() >= end) {
            Err(Error::Unauthorized)
        } else {
            Ok(())
        }
    }

    pub fn capability(&self, ttl: Option<i64>) -> Result<Capability> {
        self.active()?;
        let ttl = ttl.unwrap_or(self.access_ttl_secs);
        if !(1..=self.access_ttl_secs).contains(&ttl) {
            return Err(Error::BadRequest(format!("ttl_secs must be 1..={}", self.access_ttl_secs)));
        }
        let exp = crate::auth::deadline_from_now(ttl)?;
        Ok(Capability {
            grants: self.grants.clone(),
            exp: Some(self.expires.map_or(exp, |end| end.min(exp))),
            user: self.user_name.clone(),
            max_exp: self.expires,
            pair: None,
            client: Some(self.id),
        })
    }
}

pub async fn load(pool: &PgPool, id: Uuid) -> Result<Installation> {
    sqlx::query_as("SELECT * FROM clients WHERE id = $1")
        .bind(id).fetch_optional(pool).await?.ok_or(Error::Unauthorized)
}

/// Refresh never trusts the old token: it proves the installation identity.
pub async fn authenticate(pool: &PgPool, id: Uuid, peer: Option<&str>, proof: Option<&str>) -> Result<Installation> {
    let client = load(pool, id).await?;
    client.active()?;
    client.identity.prove(peer, proof)?;
    Ok(client)
}

pub async fn authorize(pool: &PgPool, cap: &mut Capability, peer: Option<&str>) -> Result<()> {
    if let Some(id) = cap.client {
        let client = load(pool, id).await?;
        client.active()?;
        client.identity.bind_access(peer)?;
        cap.grants = permission::intersection(&cap.grants, &client.grants);
    }
    Ok(())
}
