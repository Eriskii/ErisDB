//! The page's Iroh endpoint: the app's key, reaching everyone through relays.
//!
//! A browser reaches nothing but relays, so the endpoint has a home relay and no other way
//! out, as the mesh's `Network::Relay` does: no address lookup, no direct paths. It reaches a
//! participant homed on another relay through that relay. A page binds one endpoint per key,
//! homed on the first relay it needs, so the sign-in and every request after it come from the
//! same endpoint.

use std::rc::Rc;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr};

thread_local! {
    static ENDPOINTS: Rc<tokio::sync::Mutex<Vec<Endpoint>>> = Rc::default();
}

/// The endpoint for `key`, bound with `relay` as its home when there is none yet.
pub async fn endpoint(key: &SecretKey, relay: &RelayUrl) -> Result<Endpoint, String> {
    let endpoints = ENDPOINTS.with(Rc::clone);
    let mut endpoints = endpoints.lock().await;
    endpoints.retain(|endpoint| !endpoint.is_closed());
    if let Some(endpoint) = endpoints.iter().find(|endpoint| endpoint.id() == key.public()) {
        return Ok(endpoint.clone());
    }
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(key.clone())
        .relay_mode(RelayMode::Custom(RelayMap::from(relay.clone())))
        .bind()
        .await
        .map_err(|e| format!("starting this app's endpoint: {e}"))?;
    endpoints.push(endpoint.clone());
    Ok(endpoint)
}

/// Where `id` is, as a browser can reach it: through `relay` alone.
pub fn through(id: EndpointId, relay: &RelayUrl) -> EndpointAddr {
    EndpointAddr::from_parts(id, [TransportAddr::Relay(relay.clone())])
}

/// Close the endpoint `key` runs, and every connection on it.
pub async fn close(key: &SecretKey) {
    let endpoints = ENDPOINTS.with(Rc::clone);
    let mut endpoints = endpoints.lock().await;
    for endpoint in endpoints.iter().filter(|endpoint| endpoint.id() == key.public()) {
        endpoint.close().await;
    }
    endpoints.retain(|endpoint| endpoint.id() != key.public());
}
