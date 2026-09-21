//! HTTP over Iroh: the same axum router, served over QUIC bi-streams.
//!
//! Each accepted bi-stream carries one HTTP/1.1 connection. Clients open a
//! stream per request (or keep one open and pipeline); either works.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use tokio::sync::Semaphore;
use axum::Router;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr};
use tower::ServiceExt;

/// The erisdb wire protocol: HTTP/1.1 inside QUIC bi-streams.
pub const ALPN: &[u8] = b"erisdb/0";

/// Domain-separation tag for deriving the iroh key from the deployment
/// secret. Changing this string changes every deployment's iroh identity.
const IROH_KEY_TAG: &[u8] = b"erisdb/iroh-endpoint-key/0";

/// The endpoint's ed25519 key, derived deterministically from the
/// deployment secret (HMAC-SHA256 as a KDF, domain-separated). Same
/// secret → same endpoint id across restarts: clients hold one address
/// forever, and the core stays stateless — no key file to lose.
fn derive_key(secret: &[u8]) -> iroh::SecretKey {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(IROH_KEY_TAG);
    let bytes: [u8; 32] = mac.finalize().into_bytes().into();
    iroh::SecretKey::from_bytes(&bytes)
}

/// The endpoint id a core with this secret serves under — the address
/// clients dial. Pure derivation, no socket: answerable any time.
pub fn endpoint_id(secret: &[u8]) -> iroh::EndpointId {
    derive_key(secret).public()
}

/// Bind an Iroh endpoint speaking the erisdb ALPN, with an identity
/// derived from `secret`.
pub async fn endpoint(secret: &[u8]) -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(derive_key(secret))
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    Ok(ep)
}

/// The address other endpoints dial to reach this one.
pub async fn advertised_addr(ep: &Endpoint) -> Result<EndpointAddr> {
    Ok(ep.addr())
}

/// Connections served at once. The endpoint id is public and stable by
/// design, so anyone who knows it can dial: the accept loop is bounded
/// before authentication, because a capability check costs more than a
/// refusal.
pub const MAX_CONNECTIONS: usize = 64;

/// Concurrent request streams per connection.
pub const MAX_STREAMS_PER_CONNECTION: usize = 32;

/// Accept connections forever, serving `app` on every bi-stream.
pub async fn serve(ep: Endpoint, app: Router) -> Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    while let Some(incoming) = ep.accept().await {
        let app = app.clone();
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!("refusing an iroh connection: {MAX_CONNECTIONS} already open");
            continue;
        };
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::debug!(error = %e, "iroh handshake failed");
                    return;
                }
            };
            serve_connection(conn, app).await;
            drop(slot);
        });
    }
    Ok(())
}

async fn serve_connection(conn: Connection, app: Router) {
    // The remote endpoint id is a cryptographic identity, verified by the
    // QUIC handshake; it becomes source.addr for every write on this
    // connection.
    let peer = crate::api::PeerAddr(format!("iroh:{}", conn.remote_id()));
    let slots = Arc::new(Semaphore::new(MAX_STREAMS_PER_CONNECTION));
    // Streams stop arriving when the connection closes.
    while let Ok((send, recv)) = conn.accept_bi().await {
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!(peer = %peer.0, "refusing a stream: connection is at its limit");
            continue;
        };
        let io = TokioIo::new(tokio::io::join(recv, send));
        let peer = peer.clone();
        let svc = TowerToHyperService::new(app.clone().map_request(
            move |mut req: axum::http::Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(peer.clone());
                req.map(Body::new)
            },
        ));
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
            drop(slot);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One secret, one identity — printable without binding a socket, so
    /// the CLI can answer "what's my endpoint id" while a core runs.
    #[test]
    fn endpoint_id_is_a_pure_function_of_the_secret() {
        let a = endpoint_id(b"secret-a");
        assert_eq!(a, endpoint_id(b"secret-a"));
        assert_ne!(a, endpoint_id(b"secret-b"));
    }

    /// The derived id names the endpoint a core actually binds.
    #[tokio::test]
    async fn endpoint_id_matches_a_bound_endpoint() {
        let ep = endpoint(b"secret-a").await.expect("bind");
        assert_eq!(endpoint_id(b"secret-a"), ep.id());
    }
}
