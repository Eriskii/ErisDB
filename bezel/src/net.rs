//! HTTP over Iroh: the same axum router, served over QUIC bi-streams.
//!
//! Each accepted bi-stream carries one HTTP/1.1 connection. Clients open a
//! stream per request (or keep one open and pipeline); either works.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr};
use tower::ServiceExt;

/// The bezel wire protocol: HTTP/1.1 inside QUIC bi-streams.
pub const ALPN: &[u8] = b"bezel/0";

/// Domain-separation tag for deriving the iroh key from the deployment
/// secret. Changing this string changes every deployment's iroh identity.
const IROH_KEY_TAG: &[u8] = b"bezel/iroh-endpoint-key/0";

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

/// Bind an Iroh endpoint speaking the bezel ALPN, with an identity
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

/// Accept connections forever, serving `app` on every bi-stream.
pub async fn serve(ep: Endpoint, app: Router) -> Result<()> {
    while let Some(incoming) = ep.accept().await {
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            serve_connection(conn, app).await;
        });
    }
    Ok(())
}

async fn serve_connection(conn: Connection, app: Router) {
    // The remote endpoint id is a cryptographic identity, verified by the
    // QUIC handshake; it becomes source.addr for every write on this
    // connection.
    let peer = crate::api::PeerAddr(format!("iroh:{}", conn.remote_id()));
    // Streams stop arriving when the connection closes.
    while let Ok((send, recv)) = conn.accept_bi().await {
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
        });
    }
}
