//! Signed capability tokens and bounded delegation.
//!
//! A token is `erisdb1.<b64url(payload)>.<b64url(hmac_sha256(secret, payload))>`.
//! The payload carries an upper scope bound. Registered tokens also name an
//! installation; installation.rs intersects that bound with its live authority.
//!
//! Scope is a set of grants, matched against the permission each request
//! requires. See permission.rs and docs/permissions.md.
//!
//! Lifetime runs on two clocks. `exp` is when this token stops working.
//! `max_exp` is the end of its refresh chain: refresh moves `exp` forward,
//! never past `max_exp`. Once `max_exp` passes, the line is dead and a human
//! re-mints. Registered installations have a separate, revocable renewal proof.
//! Only the CLI can mint a token with no `exp`; refreshing one gives it a
//! bounded lifetime.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::error::{Error, Result};

pub const PREFIX: &str = "erisdb1";

/// How long a refresh chain runs when nobody names a length. An app token
/// refreshes itself for this long, then a human mints a new one.
pub const DEFAULT_CHAIN_SECS: i64 = 30 * 86_400;

/// The longest chain the HTTP surface mints, whatever it is asked for.
pub const MAX_CHAIN_SECS: i64 = 365 * 86_400;

/// Caps on the attacker-controlled strings that get baked into a payload and
/// then HMAC'd on every request.
pub const MAX_GRANTS: usize = 64;
pub const MAX_GRANT_LEN: usize = 128;
pub const MAX_USER_LEN: usize = 128;

/// The scope a token grants: which permissions, until when — and
/// optionally who: a signed user identity, stamped into every write's
/// source. Attribution, not privilege; it grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// What this token may do, as permission patterns.
    pub grants: Vec<String>,
    /// Unix seconds; `None` never expires (only the CLI mints those).
    pub exp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unix seconds past which no refresh extends this line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_exp: Option<i64>,
    /// The pairing session this token redeems, when it is a pairing code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    /// Registered installation; revocation and current grants live in Postgres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<uuid::Uuid>,
}

impl Capability {
    /// True when some grant covers `required`.
    pub fn granted(&self, required: &str) -> bool {
        crate::permission::granted(&self.grants, required)
    }

    /// Errors unless some grant covers `required`.
    pub fn require(&self, required: &str) -> Result<()> {
        if self.granted(required) {
            Ok(())
        } else {
            Err(Error::Forbidden { permission: required.to_string() })
        }
    }

    /// The instant past which this capability grants nothing, refresh
    /// included: the end of its chain, or its own expiry when it has no
    /// chain. `None` only for a token minted never to expire.
    pub fn deadline(&self) -> Option<i64> {
        self.exp.map(|e| self.max_exp.unwrap_or(e))
    }

    /// True when `other` grants nothing this capability doesn't — in scope
    /// *and* in time. A short-lived parent cannot mint a longer-lived child,
    /// which is what keeps delegation from laundering a deadline away.
    pub fn encloses(&self, other: &Capability) -> bool {
        crate::permission::encloses(&self.grants, &other.grants) && match self.deadline() {
            None => true,
            Some(mine) => matches!(other.deadline(), Some(theirs) if theirs <= mine),
        }
    }
}

/// Reject the unbounded strings a caller would otherwise get to sign.
pub fn check_grants(grants: &[String], user: Option<&str>) -> Result<()> {
    if grants.is_empty() {
        return Err(Error::BadRequest("a capability needs at least one grant".into()));
    }
    if grants.len() > MAX_GRANTS {
        return Err(Error::BadRequest(format!("at most {MAX_GRANTS} grants")));
    }
    if let Some(g) = grants.iter().find(|g| g.len() > MAX_GRANT_LEN) {
        return Err(Error::BadRequest(format!("grant is longer than {MAX_GRANT_LEN}: {g:?}")));
    }
    for grant in grants {
        crate::permission::check_grant(grant)?;
    }
    if user.is_some_and(|u| u.len() > MAX_USER_LEN) {
        return Err(Error::BadRequest(format!("user is longer than {MAX_USER_LEN}")));
    }
    Ok(())
}

/// `now + secs`, refusing the overflow rather than wrapping into the past —
/// which would turn an absurd lifetime into an expired token, or worse, a
/// live one. Callers that care about sign check it themselves; the HTTP
/// surface and the CLI both require a positive lifetime.
pub fn deadline_from_now(secs: i64) -> Result<i64> {
    chrono::Utc::now()
        .timestamp()
        .checked_add(secs)
        .ok_or_else(|| Error::BadRequest("lifetime overflows".into()))
}

fn signature(secret: &[u8], payload: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(payload);
    mac
}

/// Mint a token. `ttl_secs` counts from now; `None` never expires.
/// An expiring token gets a refresh chain of `DEFAULT_CHAIN_SECS`
/// (or its own lifetime, whichever is longer), so apps renew themselves for a
/// bounded stretch and then a human re-mints.
pub fn mint(secret: &[u8], grants: &[&str], ttl_secs: Option<i64>, user: Option<&str>) -> Result<String> {
    mint_chain(secret, grants, ttl_secs, None, user)
}

/// Mint with an explicit refresh chain. `max_ttl_secs` is how long the line
/// may keep renewing; it is raised to `ttl_secs` if shorter.
pub fn mint_chain(
    secret: &[u8],
    grants: &[&str],
    ttl_secs: Option<i64>,
    max_ttl_secs: Option<i64>,
    user: Option<&str>,
) -> Result<String> {
    let grants: Vec<String> = grants.iter().map(|s| s.to_string()).collect();
    check_grants(&grants, user)?;
    let exp = ttl_secs.map(deadline_from_now).transpose()?;
    let max_exp = ttl_secs.map(|ttl| deadline_from_now(max_ttl_secs.unwrap_or(DEFAULT_CHAIN_SECS).max(ttl))).transpose()?;
    let cap = Capability { grants, exp, user: user.map(str::to_string), max_exp, pair: None, client: None };
    mint_capability(secret, &cap)
}

pub fn mint_capability(secret: &[u8], cap: &Capability) -> Result<String> {
    let payload = serde_json::to_vec(cap).map_err(|e| Error::Internal(e.to_string()))?;
    let sig = signature(secret, &payload).finalize().into_bytes();
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
    signature(secret, &payload).verify_slice(&sig).map_err(|_| Error::Unauthorized)?;
    let cap: Capability = serde_json::from_slice(&payload).map_err(|_| Error::Unauthorized)?;
    let now = chrono::Utc::now().timestamp();
    if cap.exp.is_some_and(|exp| now >= exp) {
        return Err(Error::Unauthorized);
    }
    if cap.deadline().is_some_and(|end| now >= end) {
        return Err(Error::Unauthorized);
    }
    Ok(cap)
}
