//! The core's Iroh endpoint: the API over QUIC, and the mesh beside it.
//!
//! One endpoint speaks three protocols. `eris/database/1`, the ALPN of the
//! capability ErisDB serves, carries the same axum router the TCP listener
//! serves, one HTTP/1.1 connection per accepted bi-stream. `eris-mesh/0` is
//! the core's [node](erisoverlaynetwork::mesh), keeping its copy of the
//! device list current. `eris-enroll/0` is where devices bring apps' notes
//! and the passphrase ([`crate::enroll`]). The accept loop tells them apart
//! by ALPN.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use erisoverlaynetwork::enroll::{self, EnrollService};
use erisoverlaynetwork::mesh::{self, MeshProtocol};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use iroh::endpoint::Connection;
use iroh::Endpoint;
use tokio::sync::Semaphore;
use tower::ServiceExt;

/// The ALPN the API answers on, the database's: `eris/database/1`. The wire
/// is HTTP/1.1 inside QUIC bi-streams.
pub fn alpn() -> Vec<u8> {
    crate::serves().capability.alpn()
}

/// Domain-separation tag for deriving the iroh key from the deployment
/// secret. Changing this string changes every deployment's iroh identity.
const IROH_KEY_TAG: &[u8] = b"erisdb/iroh-endpoint-key/0";

/// The endpoint's ed25519 key, derived deterministically from the
/// deployment secret. Same secret → same endpoint id across restarts: the
/// key a device approved stays this core's key, and the core keeps no key
/// file to lose.
pub fn key(secret: &[u8]) -> iroh::SecretKey {
    erisoverlaynetwork::key::derive(secret, IROH_KEY_TAG)
}

/// The endpoint id a core with this secret serves under. Pure derivation,
/// no socket: answerable any time.
pub fn endpoint_id(secret: &[u8]) -> iroh::EndpointId {
    key(secret).public()
}

/// Connections served at once. The endpoint id is public and stable by
/// design, so anyone who knows it can dial: the accept loop is bounded
/// before authentication, because an authorization check costs more than a
/// refusal.
pub const MAX_CONNECTIONS: usize = 64;

/// Concurrent request streams per connection.
pub const MAX_STREAMS_PER_CONNECTION: usize = 32;

/// The caller's Iroh key, verified by the QUIC handshake, inserted into
/// every request that arrives over Iroh.
#[derive(Clone, Copy, Debug)]
pub struct IrohPeer(pub iroh::EndpointId);

/// Accept connections until the endpoint closes: the API on
/// `eris/database/1`, the mesh on `eris-mesh/0`, enrollment on
/// `eris-enroll/0`.
pub async fn serve(
    ep: Endpoint,
    app: Router,
    mesh: MeshProtocol,
    enrolling: EnrollService,
) -> Result<()> {
    let api = alpn();
    ep.set_alpns(vec![
        api.clone(),
        mesh::ALPN.to_vec(),
        enroll::ALPN.to_vec(),
    ]);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    while let Some(incoming) = ep.accept().await {
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!("refusing an iroh connection: {MAX_CONNECTIONS} already open");
            continue;
        };
        let (app, mesh, enrolling, api) =
            (app.clone(), mesh.clone(), enrolling.clone(), api.clone());
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::debug!(error = %e, "iroh handshake failed");
                    return;
                }
            };
            match conn.alpn() {
                alpn if alpn == api.as_slice() => serve_connection(conn, app).await,
                mesh::ALPN => mesh.serve(conn).await,
                enroll::ALPN => enrolling.serve(conn).await,
                other => {
                    tracing::debug!(alpn = %String::from_utf8_lossy(other), "unknown protocol")
                }
            }
            drop(slot);
        });
    }
    Ok(())
}

async fn serve_connection(conn: Connection, app: Router) {
    // The remote endpoint id is a cryptographic identity, verified by the
    // QUIC handshake: it is the credential of an installation calling with
    // no token, and source.addr for every write on this connection.
    let id = conn.remote_id();
    let peer = crate::api::PeerAddr(format!("iroh:{id}"));
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
                req.extensions_mut().insert(IrohPeer(id));
                req.map(Body::new)
            },
        ));
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
            drop(slot);
        });
    }
}
