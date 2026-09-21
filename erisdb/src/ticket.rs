//! Pairing tickets: where the core is and what to say, as one scannable
//! string.
//!
//! `erisdb://pair/<base64url-nopad(JSON)>`. The payload carries a capability
//! token and at least one address — an iroh endpoint id, a plain HTTP URL,
//! or both, so one ticket serves a browser on the LAN and a phone anywhere.
//!
//! There is no pairing service. A ticket is self-contained, which is what
//! makes "I operate no infrastructure" true, and also what makes a ticket
//! worth exactly the capability inside it. See docs/pairing.md.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The literal prefix. Everything after it is the encoded payload.
pub const SCHEME: &str = "erisdb://pair/";

/// The only ticket version there is. A client that meets a version it does
/// not know refuses the ticket rather than guessing at the fields.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub v: u32,
    /// A label for the human — *paired with my-laptop*. Never trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Iroh endpoint id, 64 lowercase hex characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eid: Option<String>,
    /// Plain HTTP base URL, for clients that cannot speak QUIC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub token: String,
}

impl Ticket {
    pub fn new(token: String, eid: Option<String>, url: Option<String>, name: Option<String>) -> Result<Self> {
        let ticket = Ticket { v: VERSION, name, eid, url, token };
        ticket.check()?;
        Ok(ticket)
    }

    /// Everything a client is required to reject, checked in one place so
    /// building and parsing cannot disagree about what a valid ticket is.
    fn check(&self) -> Result<()> {
        if self.v != VERSION {
            return Err(Error::BadRequest(format!(
                "ticket version {} is not supported; this core speaks v{VERSION}",
                self.v
            )));
        }
        if self.token.is_empty() {
            return Err(Error::BadRequest("ticket carries no token".into()));
        }
        if self.eid.is_none() && self.url.is_none() {
            return Err(Error::BadRequest(
                "ticket names no address: it needs an endpoint id, a url, or both".into(),
            ));
        }
        if let Some(eid) = &self.eid {
            let ok = eid.len() == 64 && eid.chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(Error::BadRequest(
                    "endpoint id must be 64 hex characters".into(),
                ));
            }
        }
        Ok(())
    }

    /// The scannable string.
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(format!("{SCHEME}{}", B64.encode(json)))
    }

    pub fn parse(s: &str) -> Result<Self> {
        let payload = s
            .trim()
            .strip_prefix(SCHEME)
            .ok_or_else(|| Error::BadRequest(format!("a pairing ticket starts with {SCHEME}")))?;
        let json = B64
            .decode(payload)
            .map_err(|_| Error::BadRequest("ticket payload is not base64url".into()))?;
        let ticket: Ticket = serde_json::from_slice(&json)
            .map_err(|e| Error::BadRequest(format!("ticket payload is not a ticket: {e}")))?;
        ticket.check()?;
        Ok(ticket)
    }
}
