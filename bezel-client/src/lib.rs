//! Dials a bezel over Iroh: HTTP/1.1 per QUIC bi-stream, ALPN `bezel/0`.
//!
//! The async [`Client`] is the real thing; [`blocking`] wraps it in an
//! owned runtime for FFI callers (JNI has no executor). The Android
//! bindings live in [`android`] and compile only for that target.

use anyhow::{anyhow, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use serde_json::Value;

#[cfg(target_os = "android")]
mod android;

/// The bezel wire protocol; must match the core's `net::ALPN`.
pub const ALPN: &[u8] = b"bezel/0";

pub struct Client {
    endpoint: Endpoint,
    server: EndpointAddr,
    token: String,
    client_name: String,
    conn: tokio::sync::Mutex<Option<Connection>>,
}

impl Client {
    /// Dial by whatever the user pasted: a bare endpoint id (hex, with or
    /// without an `iroh:` prefix) resolved through discovery, or a full
    /// JSON `EndpointAddr` for direct dialing.
    pub async fn dial(
        server: &str,
        token: &str,
        client_name: &str,
        identity: Option<[u8; 32]>,
    ) -> Result<Self> {
        let server = server.trim();
        let addr: EndpointAddr = if server.starts_with('{') {
            serde_json::from_str(server).context("parsing endpoint addr JSON")?
        } else {
            let id: EndpointId = server
                .strip_prefix("iroh:")
                .unwrap_or(server)
                .parse()
                .map_err(|e| anyhow!("bad endpoint id: {e}"))?;
            id.into()
        };
        Self::dial_addr(addr, token, client_name, identity).await
    }

    /// Dial a known address. `identity` pins the caller's own endpoint
    /// key so `source.addr` names the same device forever; `None` is a
    /// fresh identity per process.
    pub async fn dial_addr(
        server: EndpointAddr,
        token: &str,
        client_name: &str,
        identity: Option<[u8; 32]>,
    ) -> Result<Self> {
        let mut builder = Endpoint::builder(presets::N0);
        if let Some(bytes) = identity {
            builder = builder.secret_key(SecretKey::from_bytes(&bytes));
        }
        let endpoint = builder.bind().await?;
        Ok(Self {
            endpoint,
            server,
            token: token.to_string(),
            client_name: client_name.to_string(),
            conn: tokio::sync::Mutex::new(None),
        })
    }

    /// One API call: open a bi-stream on the (cached) connection, speak
    /// one HTTP/1.1 exchange. A dead connection gets one redial.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let conn = self.connection(false).await?;
        match self.exchange(&conn, method, path, &body).await {
            Ok(r) => Ok(r),
            Err(_) => {
                let conn = self.connection(true).await?;
                self.exchange(&conn, method, path, &body).await
            }
        }
    }

    async fn connection(&self, force_redial: bool) -> Result<Connection> {
        let mut slot = self.conn.lock().await;
        if force_redial {
            *slot = None;
        }
        if let Some(conn) = &*slot {
            return Ok(conn.clone());
        }
        let conn = self.endpoint.connect(self.server.clone(), ALPN).await?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    async fn exchange(
        &self,
        conn: &Connection,
        method: &str,
        path: &str,
        body: &Option<Value>,
    ) -> Result<(u16, Value)> {
        let (send, recv) = conn.open_bi().await?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(driver);

        let mut req = hyper::Request::builder()
            .method(hyper::Method::from_bytes(method.as_bytes())?)
            .uri(path)
            .header("host", "bezel")
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-bezel-client", &self.client_name);
        let payload = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Bytes::from(serde_json::to_vec(v)?)
            }
            None => Bytes::new(),
        };
        let resp = sender.send_request(req.body(Full::new(payload))?).await?;
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await?.to_bytes();
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }
}

/// Synchronous facade for FFI callers: one process-wide runtime and
/// client, errors as data (never a panic across the boundary).
pub mod blocking {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn runtime() -> &'static tokio::runtime::Runtime {
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime builds")
        })
    }

    fn client_slot() -> &'static Mutex<Option<std::sync::Arc<Client>>> {
        static CLIENT: OnceLock<Mutex<Option<std::sync::Arc<Client>>>> = OnceLock::new();
        CLIENT.get_or_init(|| Mutex::new(None))
    }

    /// (Re)connect the process-wide client.
    pub fn configure(
        server: &str,
        token: &str,
        client_name: &str,
        identity: &[u8],
    ) -> std::result::Result<(), String> {
        let identity: [u8; 32] =
            identity.try_into().map_err(|_| "identity must be 32 bytes".to_string())?;
        let client = runtime()
            .block_on(Client::dial(server, token, client_name, Some(identity)))
            .map_err(|e| e.to_string())?;
        *client_slot().lock().unwrap() = Some(std::sync::Arc::new(client));
        Ok(())
    }

    /// One API call; the response is always a JSON string:
    /// `{"status": n, "body": …}` on an exchange, `{"status": 0, "error": …}`
    /// when the transport failed or nothing is configured.
    pub fn request(method: &str, path: &str, body_json: Option<&str>) -> String {
        let client = match client_slot().lock().unwrap().clone() {
            Some(c) => c,
            None => return r#"{"status":0,"error":"not configured"}"#.to_string(),
        };
        let body = match body_json {
            Some(s) => match serde_json::from_str(s) {
                Ok(v) => Some(v),
                Err(e) => {
                    return serde_json::json!({"status": 0, "error": format!("bad body: {e}")})
                        .to_string()
                }
            },
            None => None,
        };
        match runtime().block_on(client.request(method, path, body)) {
            Ok((status, body)) => {
                serde_json::json!({"status": status, "body": body}).to_string()
            }
            Err(e) => serde_json::json!({"status": 0, "error": e.to_string()}).to_string(),
        }
    }
}
