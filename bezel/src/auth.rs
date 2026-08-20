//! Stateless capability tokens.
//!
//! A token is `bz1.<b64url(payload)>.<b64url(hmac_sha256(secret, payload))>`.
//! The payload carries its own scope; the core verifies a signature and looks
//! nothing up. Revocation is expiry: keep TTLs short and re-mint.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};

pub const PREFIX: &str = "bz1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Read,
    Write,
    Admin,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Write => "write",
            Verb::Admin => "admin",
        }
    }
}

/// The scope a token grants: which facets, which verbs, until when — and
/// optionally who: a signed user identity, stamped into every write's
/// source. Attribution, not privilege; it grants nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub facets: Vec<String>,
    pub verbs: Vec<String>,
    /// Unix seconds; `None` never expires (mint those deliberately).
    pub exp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Capability {
    pub fn covers_facet(&self, facet: &str) -> bool {
        self.facets.iter().any(|f| f == "*" || f == facet)
    }

    pub fn has_verb(&self, verb: Verb) -> bool {
        self.verbs.iter().any(|v| v == verb.as_str())
    }

    /// Errors unless this capability grants `verb` on `facet`.
    pub fn require(&self, facet: &str, verb: Verb) -> Result<()> {
        if self.covers_facet(facet) && self.has_verb(verb) {
            Ok(())
        } else {
            Err(Error::Forbidden { facet: facet.to_string(), verb: verb.as_str().to_string() })
        }
    }

    /// True when `other` grants nothing this capability doesn't.
    pub fn encloses(&self, other: &Capability) -> bool {
        let facets_ok = other.facets.iter().all(|f| {
            if f == "*" {
                self.facets.iter().any(|s| s == "*")
            } else {
                self.covers_facet(f)
            }
        });
        let verbs_ok = other.verbs.iter().all(|v| self.verbs.iter().any(|s| s == v));
        facets_ok && verbs_ok
    }
}

fn sign(secret: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

/// Mint a token. `ttl_secs` counts from now; `None` never expires.
/// `user` is the signed identity the token writes as.
pub fn mint(
    secret: &[u8],
    facets: &[&str],
    verbs: &[&str],
    ttl_secs: Option<i64>,
    user: Option<&str>,
) -> Result<String> {
    let cap = Capability {
        facets: facets.iter().map(|s| s.to_string()).collect(),
        verbs: verbs.iter().map(|s| s.to_string()).collect(),
        exp: ttl_secs.map(|t| chrono::Utc::now().timestamp() + t),
        user: user.map(str::to_string),
    };
    mint_capability(secret, &cap)
}

pub fn mint_capability(secret: &[u8], cap: &Capability) -> Result<String> {
    let payload = serde_json::to_vec(cap).map_err(|e| Error::Internal(e.to_string()))?;
    let sig = sign(secret, &payload);
    Ok(format!("{PREFIX}.{}.{}", B64.encode(&payload), B64.encode(sig)))
}

/// Verify a token's signature and expiry; return its scope.
pub fn verify(secret: &[u8], token: &str) -> Result<Capability> {
    let mut parts = token.split('.');
    let (prefix, payload_b64, sig_b64) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(p), Some(pl), Some(s), None) => (p, pl, s),
        _ => return Err(Error::Unauthorized),
    };
    if prefix != PREFIX {
        return Err(Error::Unauthorized);
    }
    let payload = B64.decode(payload_b64).map_err(|_| Error::Unauthorized)?;
    let sig = B64.decode(sig_b64).map_err(|_| Error::Unauthorized)?;
    let expected = sign(secret, &payload);
    if expected.ct_eq(&sig).unwrap_u8() != 1 {
        return Err(Error::Unauthorized);
    }
    let cap: Capability = serde_json::from_slice(&payload).map_err(|_| Error::Unauthorized)?;
    if let Some(exp) = cap.exp {
        if chrono::Utc::now().timestamp() >= exp {
            return Err(Error::Unauthorized);
        }
    }
    Ok(cap)
}
