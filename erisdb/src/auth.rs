//! Stateless capability tokens.
//!
//! A token is `bz1.<b64url(payload)>.<b64url(hmac_sha256(secret, payload))>`.
//! The payload carries its own scope; the core verifies a signature and looks
//! nothing up.
//!
//! Scope is a set of grants, matched against the permission each request
//! requires. See permission.rs and docs/permissions.md.
//!
//! Lifetime runs on two clocks. `exp` is when this token stops working.
//! `max_exp` is the end of its refresh chain: refresh moves `exp` forward,
//! never past `max_exp`. Once `max_exp` passes, the line is dead and a human
//! re-mints — so a leaked token is bounded even though the core keeps no
//! revocation list. A token with no `exp` never expires and cannot be
//! refreshed; only the CLI, which holds the secret, mints one.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};

pub const PREFIX: &str = "bz1";

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
        let scope_ok = crate::permission::encloses(&self.grants, &other.grants);
        let time_ok = match self.deadline() {
            None => true,
            Some(mine) => matches!(other.deadline(), Some(theirs) if theirs <= mine),
        };
        scope_ok && time_ok
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

fn sign(secret: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

/// Mint a token. `ttl_secs` counts from now; `None` never expires and cannot
/// be refreshed. An expiring token gets a refresh chain of `DEFAULT_CHAIN_SECS`
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
    let max_exp = match ttl_secs {
        None => None,
        Some(ttl) => {
            let chain = max_ttl_secs.unwrap_or(DEFAULT_CHAIN_SECS).max(ttl);
            Some(deadline_from_now(chain)?)
        }
    };
    let cap = Capability { grants, exp, user: user.map(str::to_string), max_exp, pair: None };
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
    let now = chrono::Utc::now().timestamp();
    if cap.exp.is_some_and(|exp| now >= exp) {
        return Err(Error::Unauthorized);
    }
    if cap.deadline().is_some_and(|end| now >= end) {
        return Err(Error::Unauthorized);
    }
    Ok(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const S: &[u8] = b"unit-test-secret";
    const OTHER: &[u8] = b"a-different-secret";

    fn cap(grants: &[&str], exp: Option<i64>, max_exp: Option<i64>) -> Capability {
        Capability {
            grants: grants.iter().map(|s| s.to_string()).collect(),
            exp,
            user: None,
            max_exp,
            pair: None,
        }
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    // ------------------------------------------------------------ signature

    #[test]
    fn a_minted_token_verifies_and_carries_its_scope() {
        let t = mint(S, &["tasks:read"], Some(3600), Some("alice")).unwrap();
        let c = verify(S, &t).unwrap();
        assert_eq!(c.grants, ["tasks:read"]);
        assert_eq!(c.user.as_deref(), Some("alice"));
    }

    #[test]
    fn a_token_signed_with_another_secret_is_rejected() {
        let t = mint(OTHER, &["*"], Some(3600), None).unwrap();
        assert!(verify(S, &t).is_err());
    }

    #[test]
    fn a_mutated_payload_is_rejected() {
        // Escalate the scope in the payload and keep the original signature.
        let t = mint(S, &["tasks:read"], Some(3600), None).unwrap();
        let mut parts = t.split('.');
        let (prefix, payload_b64, sig) =
            (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        let mut payload: Value = serde_json::from_slice(&B64.decode(payload_b64).unwrap()).unwrap();
        payload["grants"] = serde_json::json!(["*"]);
        let forged = B64.encode(serde_json::to_vec(&payload).unwrap());
        assert!(verify(S, &format!("{prefix}.{forged}.{sig}")).is_err());
    }

    #[test]
    fn a_signature_lifted_from_another_token_is_rejected() {
        let narrow = mint(S, &["tasks:read"], Some(3600), None).unwrap();
        let wide = mint(S, &["*"], Some(3600), None).unwrap();
        let wide_payload = wide.split('.').nth(1).unwrap();
        let narrow_sig = narrow.split('.').nth(2).unwrap();
        assert!(verify(S, &format!("bz1.{wide_payload}.{narrow_sig}")).is_err());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let good = mint(S, &["*"], Some(3600), None).unwrap();
        let payload = good.split('.').nth(1).unwrap();
        let sig = good.split('.').nth(2).unwrap();
        for bad in [
            String::new(),
            "bz1".into(),
            format!("bz1.{payload}"),
            format!("bz1.{payload}.{sig}.extra"),
            format!("bz1.{payload}."),
            format!("bz2.{payload}.{sig}"),
            format!("bz1.!!not-base64!!.{sig}"),
            format!("bz1.{payload}.!!not-base64!!"),
            format!("{payload}.{sig}"),
        ] {
            assert!(verify(S, &bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn an_empty_signature_never_passes() {
        let payload = B64.encode(serde_json::to_vec(&cap(&["*"], None, None)).unwrap());
        assert!(verify(S, &format!("bz1.{payload}.")).is_err());
    }

    // ------------------------------------------------------------ expiry

    #[test]
    fn an_expired_token_is_rejected() {
        let t = mint_capability(S, &cap(&["*"], Some(now() - 1), None)).unwrap();
        assert!(verify(S, &t).is_err());
    }

    #[test]
    fn a_zero_second_token_is_already_dead() {
        let t = mint(S, &["*"], Some(0), None).unwrap();
        assert!(verify(S, &t).is_err());
    }

    #[test]
    fn a_token_past_its_chain_is_rejected_even_with_a_live_exp() {
        let stale = cap(&["*"], Some(now() + 3600), Some(now() - 1));
        assert!(verify(S, &mint_capability(S, &stale).unwrap()).is_err());
    }

    #[test]
    fn minting_gives_an_expiring_token_a_bounded_chain() {
        let c = verify(S, &mint(S, &["*"], Some(3600), None).unwrap()).unwrap();
        let end = c.deadline().expect("an expiring token has a deadline");
        assert!(end <= now() + DEFAULT_CHAIN_SECS + 5);
        assert!(end >= c.exp.unwrap());
    }

    #[test]
    fn a_non_expiring_token_has_no_deadline() {
        let c = verify(S, &mint(S, &["*"], None, None).unwrap()).unwrap();
        assert_eq!(c.deadline(), None);
    }

    #[test]
    fn a_long_lifetime_raises_the_chain_to_match() {
        let c = verify(S, &mint(S, &["*"], Some(DEFAULT_CHAIN_SECS * 2), None).unwrap()).unwrap();
        assert!(c.deadline().unwrap() >= c.exp.unwrap());
    }

    #[test]
    fn an_overflowing_lifetime_is_refused_rather_than_wrapped() {
        assert!(mint(S, &["*"], Some(i64::MAX), None).is_err());
    }

    #[test]
    fn a_negative_lifetime_mints_a_token_that_is_already_dead() {
        assert!(verify(S, &mint(S, &["*"], Some(-10), None).unwrap()).is_err());
    }

    // ------------------------------------------------------------ enclosure

    /// Scope enclosure lives in permission.rs; what auth adds is time.
    #[test]
    fn a_short_lived_parent_cannot_mint_a_longer_lived_child() {
        let parent = cap(&["*"], Some(now() + 60), Some(now() + 60));
        assert!(!parent.encloses(&cap(&["tasks:read"], None, None)), "minted an immortal child");
        assert!(
            !parent.encloses(&cap(&["tasks:read"], Some(now() + 86_400), None)),
            "minted a child that outlives it"
        );
        assert!(
            !parent.encloses(&cap(&["tasks:read"], Some(now() + 30), Some(now() + 86_400))),
            "minted a child whose chain outlives it"
        );
        assert!(parent.encloses(&cap(&["tasks:read"], Some(now() + 30), Some(now() + 30))));
    }

    #[test]
    fn enclosure_covers_scope_as_well_as_time() {
        let parent = cap(&["tasks:*"], None, None);
        assert!(parent.encloses(&cap(&["tasks:read"], None, None)));
        assert!(!parent.encloses(&cap(&["lists:read"], None, None)));
        assert!(!parent.encloses(&cap(&["*"], None, None)));
    }

    #[test]
    fn an_immortal_parent_may_mint_anything_it_covers() {
        let root = cap(&["*"], None, None);
        assert!(root.encloses(&cap(&["*"], None, None)));
        assert!(root.encloses(&cap(&["tasks:read"], Some(now() + 60), None)));
    }

    #[test]
    fn the_chain_is_what_bounds_enclosure_not_the_current_expiry() {
        let parent = cap(&["*"], Some(now() + 60), Some(now() + 86_400));
        assert!(parent.encloses(&cap(&["tasks:read"], Some(now() + 3600), Some(now() + 3600))));
        assert!(!parent.encloses(&cap(&["tasks:read"], Some(now() + 90_000), Some(now() + 90_000))));
    }

    // ------------------------------------------------------------ scope input

    #[test]
    fn malformed_grants_are_refused() {
        assert!(mint(S, &["Tasks:read"], Some(60), None).is_err());
        assert!(mint(S, &["tasks::read"], Some(60), None).is_err());
        assert!(mint(S, &[], Some(60), None).is_err());
    }

    #[test]
    fn oversized_scopes_are_refused() {
        let many: Vec<String> = (0..MAX_GRANTS + 1).map(|i| format!("f{i}:read")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(mint(S, &refs, Some(60), None).is_err());

        let long = format!("{}:read", "f".repeat(MAX_GRANT_LEN));
        assert!(mint(S, &[&long], Some(60), None).is_err());

        let user = "u".repeat(MAX_USER_LEN + 1);
        assert!(mint(S, &["*"], Some(60), Some(&user)).is_err());
    }

    #[test]
    fn require_names_the_permission_it_wanted() {
        let c = cap(&["tasks:read"], None, None);
        assert!(c.require("tasks:read").is_ok());
        let err = c.require("tasks:delete").unwrap_err().to_string();
        assert!(err.contains("tasks:delete"), "{err}");
    }
}
