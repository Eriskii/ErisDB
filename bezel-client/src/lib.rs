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
    token: tokio::sync::RwLock<String>,
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
            token: tokio::sync::RwLock::new(token.to_string()),
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
            .header("authorization", format!("Bearer {}", self.token.read().await))
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

    /// Trade the current token for one with the same scope and a fresh
    /// `ttl_secs` expiry, swap it in for every later request on this
    /// client, and hand it back.
    ///
    /// Persistence is the caller's job: the client holds the fresh token
    /// only for its own lifetime, so a device that wants to survive a
    /// restart writes the returned string to its own storage. Refresh
    /// moves time, not privilege — facets, verbs, and the signed user
    /// carry over untouched — and an expired token cannot refresh, so a
    /// long-sleeping device refreshes on wake before its expiry passes.
    pub async fn refresh_capability(&self, ttl_secs: i64) -> Result<String> {
        let (status, body) = self
            .request("POST", "/v1/capabilities/refresh", Some(serde_json::json!({"ttl_secs": ttl_secs})))
            .await?;
        if status != 201 {
            return Err(anyhow!("refresh refused: {status} {body}"));
        }
        let token = body["token"]
            .as_str()
            .ok_or_else(|| anyhow!("refresh returned no token"))?
            .to_string();
        *self.token.write().await = token.clone();
        Ok(token)
    }

    /// Subscribe to the change feed from `since` (exclusive), optionally
    /// narrowed to one facet. The subscription holds one bi-stream open
    /// for its lifetime and yields events as the core emits them.
    ///
    /// The caller owns the cursor. Every event carries its `seq`; when a
    /// subscription ends — dropped connection, sleeping device, process
    /// restart — resubscribe with the last `seq` seen and the feed picks
    /// up exactly there, losing nothing.
    pub async fn subscribe_changes(&self, since: i64, facet: Option<&str>) -> Result<Subscription> {
        let mut path = format!("/v1/changes/stream?since={since}");
        if let Some(f) = facet {
            path.push_str("&facet=");
            path.push_str(&urlencode(f));
        }
        let conn = self.connection(false).await?;
        match self.open_stream(&conn, &path).await {
            Ok(s) => Ok(s),
            Err(_) => {
                let conn = self.connection(true).await?;
                self.open_stream(&conn, &path).await
            }
        }
    }

    async fn open_stream(&self, conn: &Connection, path: &str) -> Result<Subscription> {
        let (send, recv) = conn.open_bi().await?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io).await?;
        let driver = tokio::spawn(driver);

        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(path)
            .header("host", "bezel")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {}", self.token.read().await))
            .header("x-bezel-client", &self.client_name)
            .body(Full::new(Bytes::new()))?;
        let resp = sender.send_request(req).await?;
        let status = resp.status().as_u16();
        if status != 200 {
            let bytes = resp.into_body().collect().await?.to_bytes();
            driver.abort();
            return Err(anyhow!("change stream refused: {status} {}", String::from_utf8_lossy(&bytes)));
        }
        Ok(Subscription {
            body: resp.into_body(),
            driver,
            _sender: sender,
            buf: Vec::new(),
            done: false,
        })
    }
}

/// Percent-encode everything outside the unreserved set, so a facet name
/// like `lists/v1` survives as one query value.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One row of the change feed, as delivered over the live stream.
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// The feed's total order. This is the cursor: hand the highest `seq`
    /// you have processed back to [`Client::subscribe_changes`] to resume.
    pub seq: i64,
    pub facet: String,
    pub op: String,
    /// The item this change touched; absent for feed-level rows like ticks.
    pub item_id: Option<String>,
    /// The body this change produced; absent for deletes and ticks.
    pub body: Option<Value>,
    /// The revision this change produced; absent wherever `body` is.
    pub revision: Option<i64>,
    /// The whole change row as the core serialized it, including `at`
    /// and `source`.
    pub raw: Value,
}

impl ChangeEvent {
    fn parse(data: &str) -> Result<Self> {
        let raw: Value = serde_json::from_str(data).context("parsing change event")?;
        let seq = raw["seq"].as_i64().ok_or_else(|| anyhow!("change event has no seq"))?;
        Ok(Self {
            seq,
            facet: raw["facet"].as_str().unwrap_or_default().to_string(),
            op: raw["op"].as_str().unwrap_or_default().to_string(),
            item_id: raw["item_id"].as_str().map(str::to_string),
            body: match &raw["body"] {
                Value::Null => None,
                v => Some(v.clone()),
            },
            revision: raw["revision"].as_i64(),
            raw,
        })
    }
}

/// A live change feed over one bi-stream. Parses SSE frames incrementally
/// and yields the `change` events; keep-alive comments and any other
/// event name are skipped. Dropping it closes the stream.
pub struct Subscription {
    body: hyper::body::Incoming,
    driver: tokio::task::JoinHandle<std::result::Result<(), hyper::Error>>,
    // Holding the request sender keeps hyper's connection from shutting
    // the stream down under us.
    _sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    buf: Vec<u8>,
    done: bool,
}

impl Subscription {
    /// The next change, or `None` once the stream ends. Cancel-safe only
    /// at the granularity of whole frames: a dropped `next()` future may
    /// lose a partially read frame, so drive it to completion (a timeout
    /// around it costs at most one event).
    pub async fn next(&mut self) -> Result<Option<ChangeEvent>> {
        loop {
            if let Some(event) = self.take_frame()? {
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            match self.body.frame().await {
                Some(frame) => {
                    if let Ok(data) = frame?.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                None => self.done = true,
            }
        }
    }

    /// Pull one complete SSE frame out of the buffer, if there is one.
    /// A frame is lines until a blank line; `event:` names it and each
    /// `data:` line contributes one line of payload.
    fn take_frame(&mut self) -> Result<Option<ChangeEvent>> {
        while let Some(end) = find_frame_end(&self.buf) {
            let frame = self.buf.drain(..end.1).collect::<Vec<u8>>();
            let frame = String::from_utf8_lossy(&frame[..end.0]).to_string();
            let mut name = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let (field, value) = match line.split_once(':') {
                    Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                    None => (line, ""),
                };
                match field {
                    "event" => name = value.to_string(),
                    "data" => {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(value);
                    }
                    _ => {}
                }
            }
            if name == "change" && !data.is_empty() {
                return Ok(Some(ChangeEvent::parse(&data)?));
            }
        }
        Ok(None)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// Where the frame's text ends and where the next frame begins: SSE
/// separates frames with a blank line, in either newline convention.
fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\n\n") {
            return Some((i + 1, i + 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i + 2, i + 4));
        }
    }
    None
}

/// Synchronous facade for FFI callers: one process-wide runtime and
/// client, errors as data (never a panic across the boundary).
pub mod blocking {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use tokio::sync::Mutex as TokioMutex;

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

    /// Refresh the process-wide client's capability and swap it in.
    /// The response is always a JSON string: `{"ok":true,"token":…}` on
    /// success, `{"ok":false,"error":…}` otherwise. Persisting the token
    /// is the caller's job.
    pub fn refresh_capability(ttl_secs: i64) -> String {
        let client = match client_slot().lock().unwrap().clone() {
            Some(c) => c,
            None => return r#"{"ok":false,"error":"not configured"}"#.to_string(),
        };
        match runtime().block_on(client.refresh_capability(ttl_secs)) {
            Ok(token) => serde_json::json!({"ok": true, "token": token}).to_string(),
            Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
        }
    }

    /// A change-feed subscription, addressed by handle. Callbacks across
    /// FFI are painful, so the feed is a pull: the caller parks a thread
    /// in [`next_change`] and gets one event per return.
    pub type SubscriptionHandle = u64;

    fn subscriptions() -> &'static Mutex<HashMap<SubscriptionHandle, Arc<TokioMutex<Subscription>>>>
    {
        static SUBS: OnceLock<Mutex<HashMap<SubscriptionHandle, Arc<TokioMutex<Subscription>>>>> =
            OnceLock::new();
        SUBS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Open a subscription from `since` (exclusive), optionally narrowed
    /// to one facet, and return its handle. The caller owns the cursor:
    /// remember the `seq` of the last event it processed and pass it as
    /// `since` on the next subscribe to resume without loss.
    pub fn subscribe_changes(
        since: i64,
        facet: Option<&str>,
    ) -> std::result::Result<SubscriptionHandle, String> {
        let client = match client_slot().lock().unwrap().clone() {
            Some(c) => c,
            None => return Err("not configured".to_string()),
        };
        let sub = runtime()
            .block_on(client.subscribe_changes(since, facet))
            .map_err(|e| e.to_string())?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let handle = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        subscriptions().lock().unwrap().insert(handle, Arc::new(TokioMutex::new(sub)));
        Ok(handle)
    }

    /// Block for up to `timeout_ms` on the next change. The response is
    /// always a JSON string: `{"ok":true,"change":…}` when one arrives,
    /// `{"ok":true}` when the wait elapsed with the stream still live,
    /// and `{"ok":false,"error":…}` when the stream ended, the handle is
    /// unknown, or a frame failed to parse.
    pub fn next_change(handle: SubscriptionHandle, timeout_ms: u64) -> String {
        let sub = match subscriptions().lock().unwrap().get(&handle) {
            Some(s) => s.clone(),
            None => return r#"{"ok":false,"error":"no such subscription"}"#.to_string(),
        };
        let wait = std::time::Duration::from_millis(timeout_ms);
        runtime().block_on(async {
            let mut sub = sub.lock().await;
            match tokio::time::timeout(wait, sub.next()).await {
                Ok(Ok(Some(event))) => {
                    serde_json::json!({"ok": true, "change": event.raw}).to_string()
                }
                Ok(Ok(None)) => {
                    r#"{"ok":false,"error":"stream ended"}"#.to_string()
                }
                Ok(Err(e)) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
                Err(_) => r#"{"ok":true}"#.to_string(),
            }
        })
    }

    /// Drop a subscription and its stream. Unknown handles are a no-op.
    pub fn close_subscription(handle: SubscriptionHandle) {
        subscriptions().lock().unwrap().remove(&handle);
    }
}
