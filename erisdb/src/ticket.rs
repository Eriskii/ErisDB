//! Pairing tickets: where the core is and what to say, as one scannable
//! string.
//!
//! `bezel://pair/<base64url-nopad(JSON)>`. The payload carries a capability
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
pub const SCHEME: &str = "bezel://pair/";

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

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> String {
        "bz1.eyJmYWNldHMiOlsiKiJdfQ.c2ln".to_string()
    }

    fn eid() -> String {
        "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06".to_string()
    }

    #[test]
    fn a_ticket_round_trips() {
        let t = Ticket::new(token(), Some(eid()), Some("http://10.0.0.2:7700".into()), Some("my-laptop".into()))
            .unwrap();
        let back = Ticket::parse(&t.encode().unwrap()).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn either_address_alone_is_enough() {
        assert!(Ticket::new(token(), Some(eid()), None, None).is_ok());
        assert!(Ticket::new(token(), None, Some("http://x:7700".into()), None).is_ok());
    }

    #[test]
    fn a_ticket_with_no_address_is_refused() {
        assert!(Ticket::new(token(), None, None, None).is_err());
    }

    #[test]
    fn a_ticket_with_no_token_is_refused() {
        assert!(Ticket::new(String::new(), Some(eid()), None, None).is_err());
    }

    #[test]
    fn a_malformed_endpoint_id_is_refused() {
        for bad in [
            "too-short",
            &"f".repeat(63),
            &"f".repeat(65),
            &"g".repeat(64), // not hex
        ] {
            assert!(
                Ticket::new(token(), Some(bad.to_string()), None, None).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn the_prefix_is_required() {
        let payload = Ticket::new(token(), Some(eid()), None, None).unwrap().encode().unwrap();
        let bare = payload.strip_prefix(SCHEME).unwrap();
        assert!(Ticket::parse(bare).is_err());
        assert!(Ticket::parse(&format!("https://pair/{bare}")).is_err());
    }

    #[test]
    fn a_corrupt_payload_is_refused() {
        assert!(Ticket::parse(&format!("{SCHEME}!!!not-base64!!!")).is_err());
        assert!(Ticket::parse(&format!("{SCHEME}{}", B64.encode("not json"))).is_err());
        assert!(Ticket::parse(SCHEME).is_err());
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed_at() {
        let payload = serde_json::json!({"v": 2, "token": token(), "eid": eid()});
        let s = format!("{SCHEME}{}", B64.encode(serde_json::to_vec(&payload).unwrap()));
        let err = Ticket::parse(&s).unwrap_err().to_string();
        assert!(err.contains('2'), "the refusal should name the version: {err}");
    }

    #[test]
    fn surrounding_whitespace_is_forgiven() {
        // Pasting a ticket picks up a newline more often than not.
        let t = Ticket::new(token(), Some(eid()), None, None).unwrap();
        let s = t.encode().unwrap();
        assert_eq!(Ticket::parse(&format!("  {s}\n")).unwrap(), t);
    }

    #[test]
    fn the_encoding_carries_no_padding() {
        let s = Ticket::new(token(), Some(eid()), Some("http://x:7700".into()), Some("n".into()))
            .unwrap()
            .encode()
            .unwrap();
        // The scheme has slashes of its own; only the payload is encoded.
        let payload = s.strip_prefix(SCHEME).unwrap();
        assert!(!payload.contains('='), "padding breaks a URL: {payload}");
        assert!(!payload.contains('+') && !payload.contains('/'), "not url-safe: {payload}");
    }
}
