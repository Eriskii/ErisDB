//! An installation is durable authority; a pairing is only its enrollment.
//! Native installations prove possession of their Iroh key through QUIC.
//! Browsers use a random 256-bit renewal secret over HTTPS, stored here only
//! as its S256 commitment. Neither display names nor caller-supplied IDs
//! authenticate an installation.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{types::Json, PgPool};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{auth::Capability, error::{Error, Result}, permission};

pub const MAX_ACCESS_TTL: i64 = 7 * 86_400;
pub const PROOF_HEADER: &str = "x-erisdb-client-proof";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "key", rename_all = "snake_case")]
pub enum Identity {
    Iroh(String),
    Browser(String),
}

impl Identity {
    pub fn enrollment(peer: Option<&str>, challenge: Option<String>) -> Result<Self> {
        if let Some(key) = peer.and_then(|p| p.strip_prefix("iroh:")) {
            return Ok(Self::Iroh(key.to_string()));
        }
        private_hop(peer)?;
        let challenge = challenge.ok_or_else(|| Error::BadRequest(
            "browser pairing requires an S256 challenge from a random installation secret".into(),
        ))?;
        if !B64.decode(&challenge).is_ok_and(|bytes| bytes.len() == 32 && B64.encode(bytes) == challenge) {
            return Err(Error::BadRequest("challenge must encode a SHA-256 digest as unpadded base64url".into()));
        }
        Ok(Self::Browser(challenge))
    }

    pub fn prove(&self, peer: Option<&str>, verifier: Option<&str>) -> Result<()> {
        let valid = match self {
            Self::Iroh(key) => peer.and_then(|p| p.strip_prefix("iroh:")) == Some(key),
            Self::Browser(expected) => private_hop(peer).is_ok() && verifier
                .filter(|v| (43..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)))
                .map(|v| B64.encode(Sha256::digest(v.as_bytes())))
                .is_some_and(|actual| actual.as_bytes().ct_eq(expected.as_bytes()).into()),
        };
        if valid { Ok(()) } else { Err(Error::Unauthorized) }
    }

    pub fn bind_access(&self, peer: Option<&str>) -> Result<()> {
        match self {
            Self::Iroh(_) => self.prove(peer, None),
            Self::Browser(_) => private_hop(peer),
        }
    }

    pub fn fingerprint(&self, session: Uuid, requested: &[String]) -> String {
        let transcript = serde_json::to_vec(&("erisdb-pair-v1", session, self, requested)).expect("serializable transcript");
        let digest = Sha256::digest(transcript);
        format!("{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}", digest[0], digest[1], digest[2], digest[3], digest[4], digest[5])
    }
}

/// TLS terminates at a local reverse proxy. Trust the actual TCP peer,
/// never forwarded headers that a direct caller can forge.
fn private_hop(peer: Option<&str>) -> Result<()> {
    if peer.and_then(|p| p.parse::<std::net::SocketAddr>().ok()).is_some_and(|p| p.ip().is_loopback()) {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
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
