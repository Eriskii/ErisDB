//! Dials an ErisDB over Iroh: HTTP/1.1 per QUIC bi-stream, ALPN `erisdb/0`.
//!
//! The async [`Client`] is the real thing; [`blocking`] wraps it in an
//! owned runtime for FFI callers (JNI has no executor). The Android
//! bindings live in `android` and compile only for that target.
//!
//! Two rules run through all of it. A failed call is repeated only when
//! repeating it is provably harmless, because the protocol has no
//! idempotency key and a retried `POST` is a second item. And nothing
//! leaves the FFI surface as a panic: refusals carry a status, transport
//! failures carry a message, and an unwind is caught before it reaches a
//! frame that would abort the process.
//!
//! Getting a token is [pairing](Client::pair): a scanned ticket carries a
//! code that grants only `meta:pairing:redeem`, the client redeems it with
//! the permissions it wants, and a human decides. Every wait for that
//! human is bounded and cancellable — one may never answer.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use serde_json::{json, Value};

#[cfg(target_os = "android")]
mod android;

/// The erisdb wire protocol; must match the core's `net::ALPN`.
pub const ALPN: &[u8] = b"erisdb/0";

/// A request the core answered with a refusal. The status is a field, not
/// prose, so callers — Kotlin across the FFI boundary especially — branch
/// on a number instead of substring-matching a message.
#[derive(Debug)]
pub struct Refused {
    /// The HTTP status the core returned.
    pub status: u16,
    /// The body it returned with it, parsed as JSON where it is JSON and
    /// carried as a string where it is not.
    pub body: Value,
    /// What was refused, for the message.
    what: &'static str,
}

/// A refusal as an error, downcastable back to [`Refused`].
fn refused(what: &'static str, status: u16, body: Value) -> anyhow::Error {
    anyhow!(Refused { status, body, what })
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} refused: {} {}", self.what, self.status, self.body)
    }
}

impl std::error::Error for Refused {}

/// How far an exchange got before it failed, which is what decides
/// whether repeating it is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// No request byte left this process. The server has not seen the
    /// call, so sending it again cannot repeat an effect.
    BeforeSend,
    /// The request went out. The server may have applied it and lost only
    /// the answer, so sending it again may repeat an effect.
    AfterSend,
}

/// A failed exchange, tagged with the stage it failed at.
struct Failed {
    stage: Stage,
    error: anyhow::Error,
}

impl Failed {
    fn before_send(e: impl Into<anyhow::Error>) -> Self {
        Self { stage: Stage::BeforeSend, error: e.into() }
    }

    fn after_send(e: impl Into<anyhow::Error>) -> Self {
        Self { stage: Stage::AfterSend, error: e.into() }
    }
}

/// Whether asking twice is the same as asking once. Only methods that
/// change nothing qualify: `PUT` and `DELETE` are idempotent in HTTP's
/// sense but a repeat under erisdb's revision checks answers 409 or 404
/// instead of the truth, which is worse than reporting the transport
/// failure.
fn nullipotent(method: &str) -> bool {
    method.eq_ignore_ascii_case("GET")
        || method.eq_ignore_ascii_case("HEAD")
        || method.eq_ignore_ascii_case("OPTIONS")
}

/// A response body as a value: JSON where it is JSON, the raw text where
/// it is not, and null where there is none. An error page from something
/// other than the core still reaches the caller instead of vanishing
/// behind a bare status.
fn parse_body(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

/// This unverified claim selects a renewal endpoint, never an authority.
/// The server authenticates the installation using the actual QUIC peer.
fn installation_id(token: &str) -> Option<String> {
    let payload = B64.decode(token.split('.').nth(1)?).ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    let id = claims["client"].as_str()?;
    (id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then(|| id.to_string())
}

pub struct Client {
    endpoint: Endpoint,
    server: EndpointAddr,
    token: tokio::sync::RwLock<String>,
    client_name: String,
    conn: tokio::sync::Mutex<Option<Connection>>,
    renewal: tokio::sync::Mutex<()>,
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
            renewal: tokio::sync::Mutex::new(()),
        })
    }

    /// One API call: open a bi-stream on the (cached) connection, speak
    /// one HTTP/1.1 exchange.
    ///
    /// A failure is retried on a fresh connection only when repeating it
    /// is provably harmless. The protocol carries no idempotency key, so
    /// once a request's bytes have left this process the server may have
    /// applied it and lost only the answer — repeating a `POST` there
    /// would create a second item. Two cases are safe: a failure before
    /// anything was sent (see [`Stage`]), and a nullipotent method, where
    /// asking twice cannot change the store.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let before = self.token.read().await.clone();
        let response = self.request_unrenewed(method, path, &body).await?;
        if response.0 != 401 || installation_id(&before).is_none() || path.ends_with("/refresh") {
            return Ok(response);
        }
        // A 401 is an explicit refusal before effects. Renewing and then
        // retrying is safe even for a write; a transport failure is not.
        let _renewal = self.renewal.lock().await;
        if *self.token.read().await == before {
            if let Err(error) = self.renew_access(None).await {
                return match error.downcast_ref::<Refused>() {
                    Some(_) => Ok(response),
                    None => Err(error),
                };
            }
        }
        self.request_unrenewed(method, path, &body).await
    }

    async fn request_unrenewed(&self, method: &str, path: &str, body: &Option<Value>) -> Result<(u16, Value)> {
        let conn = self.connection(false).await?;
        match self.exchange(&conn, method, path, body).await {
            Ok(r) => Ok(r),
            Err(failed) if failed.stage == Stage::BeforeSend || nullipotent(method) => {
                let conn = self.connection(true).await?;
                self.exchange(&conn, method, path, body).await.map_err(|f| f.error)
            }
            Err(failed) => Err(failed.error),
        }
    }

    /// Invoke one plugin operation and buffer its response. The core starts
    /// one fresh executable for this call and stores nothing. Any status is
    /// returned to the caller because a non-2xx response may be the external
    /// API's ordinary error format rather than a core refusal.
    pub async fn call_plugin(
        &self,
        plugin: &str,
        operation: &str,
        input: Value,
    ) -> Result<(u16, Value)> {
        self.request(
            "POST",
            "/v1/call",
            Some(json!({ "plugin": plugin, "operation": operation, "input": input })),
        )
        .await
    }

    /// Invoke one plugin operation without buffering its response. This is
    /// the path for SSE and any other streamed plugin body. A `POST` is never
    /// retried after a transport failure: the external system may already
    /// have received it.
    pub async fn stream_plugin(
        &self,
        plugin: &str,
        operation: &str,
        input: Value,
    ) -> Result<PluginStream> {
        let conn = self.connection(false).await?;
        self.open_plugin_stream(&conn, plugin, operation, input).await
    }

    async fn connection(&self, force_redial: bool) -> Result<Connection> {
        let mut slot = self.conn.lock().await;
        if force_redial {
            *slot = None;
        }
        if let Some(conn) = &*slot {
            // A connection the transport has already given up on carries
            // nothing. Noticing that here, rather than mid-request, is
            // what lets a write on a slept device redial safely.
            if conn.close_reason().is_none() {
                return Ok(conn.clone());
            }
            *slot = None;
        }
        // Discovery or a dead network must not hold a blocking/JNI caller
        // forever. No application bytes have been sent at this boundary.
        let conn = tokio::time::timeout(Duration::from_secs(30),
            self.endpoint.connect(self.server.clone(), ALPN))
            .await.context("connecting to the core timed out")??;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    async fn exchange(
        &self,
        conn: &Connection,
        method: &str,
        path: &str,
        body: &Option<Value>,
    ) -> std::result::Result<(u16, Value), Failed> {
        // Everything up to `send_request` is local: opening a QUIC stream
        // reserves an id without a round trip, and hyper's HTTP/1
        // handshake writes nothing. A failure in here never reached the
        // server.
        let (send, recv) = conn.open_bi().await.map_err(Failed::before_send)?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) =
            hyper::client::conn::http1::handshake(io).await.map_err(Failed::before_send)?;
        tokio::spawn(driver);

        let mut req = hyper::Request::builder()
            .method(hyper::Method::from_bytes(method.as_bytes()).map_err(Failed::before_send)?)
            .uri(path)
            .header("host", "erisdb")
            .header("authorization", format!("Bearer {}", self.token.read().await))
            .header("x-erisdb-client", &self.client_name);
        let payload = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Bytes::from(serde_json::to_vec(v).map_err(Failed::before_send)?)
            }
            None => Bytes::new(),
        };
        let req = req.body(Full::new(payload)).map_err(Failed::before_send)?;

        // From here on the request is on the wire.
        let resp = sender.send_request(req).await.map_err(Failed::after_send)?;
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.map_err(Failed::after_send)?.to_bytes();
        Ok((status, parse_body(&bytes)))
    }

    /// Trade the current token for one with the same scope and a fresh
    /// `ttl_secs` expiry, swap it in for every later request on this
    /// client, and hand it back.
    ///
    /// Persistence is the caller's job: the client holds the fresh token
    /// only for its own lifetime, so a device that wants to survive a
    /// restart writes the returned string to its own storage. Refresh
    /// uses the registration's current grants for paired installations,
    /// even after access expiry. Manual tokens retain their bounded chain.
    pub async fn refresh_capability(&self, ttl_secs: i64) -> Result<String> {
        let _renewal = self.renewal.lock().await;
        self.renew_access(Some(ttl_secs)).await
    }

    async fn renew_access(&self, ttl_secs: Option<i64>) -> Result<String> {
        let client = installation_id(&self.token.read().await);
        let path = client.as_ref().map(|id| format!("/v1/clients/{id}/refresh"))
            .unwrap_or_else(|| "/v1/capabilities/refresh".to_string());
        let (status, body) = self
            .request_unrenewed("POST", &path, &Some(serde_json::json!({"ttl_secs": ttl_secs})))
            .await?;
        if status != if client.is_some() { 200 } else { 201 } {
            return Err(refused("refresh", status, body));
        }
        let token = body["token"]
            .as_str()
            .ok_or_else(|| anyhow!("refresh returned no token"))?
            .to_string();
        *self.token.write().await = token.clone();
        Ok(token)
    }

    /// What this client's token holds: its grants, when it expires, when
    /// its refresh chain ends, and the signed user it writes as.
    ///
    /// Needs no permission — the answer is already inside the token the
    /// caller is carrying — so it is the one call that always works, and
    /// the right way for an app to find out what it may offer to do.
    pub async fn permissions(&self) -> Result<Permissions> {
        let (status, body) = self.request("GET", "/v1/permissions", None).await?;
        if status != 200 {
            return Err(refused("permissions", status, body));
        }
        Ok(Permissions {
            grants: body["grants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect(),
            exp: body["exp"].as_i64(),
            max_exp: body["max_exp"].as_i64(),
            user: body["user"].as_str().map(str::to_string),
            raw: body,
        })
    }

    /// Redeem a pairing code: say who this client is and which
    /// permissions it wants. Asking is free — the answer is a person.
    ///
    /// The client's token must be the code from a ticket, which grants
    /// exactly `meta:pairing:redeem` on one session. Redeeming moves that
    /// session from pending to requested, which is a write: a lost answer
    /// is reported rather than resent, because a second redeem against
    /// the same session is a 409.
    pub async fn redeem_pairing(&self, client: &str, requested: &[&str]) -> Result<Value> {
        let body = json!({ "client": client, "requested": requested });
        let (status, body) = self.request("POST", "/v1/pair/redeem", Some(body)).await?;
        if status != 200 {
            return Err(refused("redeem", status, body));
        }
        Ok(body)
    }

    /// Collect the result through the same authenticated installation key.
    /// Repeating collection recovers safely after a lost response.
    pub async fn pairing_status(&self) -> Result<Pairing> {
        let (status, body) = self.request("GET", "/v1/pair/status", None).await?;
        if status != 200 {
            return Err(refused("pairing status", status, body));
        }
        fn already_collected() -> anyhow::Error {
            anyhow!(
                "this pairing was already collected: the token is handed over once, \
                 so pair again to get another"
            )
        }
        let granted = || -> Vec<String> {
            body["granted"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect()
        };
        match body["status"].as_str() {
            Some("pending") | Some("requested") => Ok(Pairing::Waiting),
            Some("denied") => Ok(Pairing::Denied),
            Some("approved") => match body["token"].as_str() {
                Some(token) => Ok(Pairing::Approved { token: token.to_string(), granted: granted() }),
                None => Err(already_collected()),
            },
            // The core mints the token at the moment it is collected and
            // spends the session doing it, so there is nothing left to
            // collect and nothing to be done but pair again. Say that,
            // rather than returning an approval with no token in it.
            Some("collected") => Err(already_collected()),
            other => Err(anyhow!("pairing session is in an unknown state: {other:?}")),
        }
    }

    /// Poll until a human answers, `within` elapses, or `cancel` fires.
    ///
    /// A human may never answer — the phone is in a pocket, the laptop is
    /// shut — so this never waits forever and never has to be killed to
    /// stop. A cancel is noticed between polls, so at worst it waits out
    /// the request in flight.
    pub async fn await_pairing(&self, within: Duration, cancel: &Cancel) -> Result<Pairing> {
        let deadline = Instant::now() + within;
        loop {
            if cancel.is_cancelled() {
                return Ok(Pairing::Cancelled);
            }
            match self.pairing_status().await? {
                Pairing::Waiting => {}
                settled => return Ok(settled),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Pairing::TimedOut);
            }
            if cancel.sleep(PAIR_POLL_INTERVAL.min(left)).await {
                return Ok(Pairing::Cancelled);
            }
        }
    }

    /// The whole of the client's side of pairing: redeem the code this
    /// client dialed with, then wait for the answer.
    pub async fn pair(
        &self,
        client: &str,
        requested: &[&str],
        within: Duration,
        cancel: &Cancel,
    ) -> Result<Pairing> {
        self.redeem_pairing(client, requested).await?;
        self.await_pairing(within, cancel).await
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
        // Subscribing is a read from a cursor the caller owns: opening it
        // twice costs nothing and changes nothing, so any failure redials.
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
            .header("host", "erisdb")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {}", self.token.read().await))
            .header("x-erisdb-client", &self.client_name)
            .body(Full::new(Bytes::new()))?;
        let resp = sender.send_request(req).await?;
        let status = resp.status().as_u16();
        if status != 200 {
            let bytes = resp.into_body().collect().await?.to_bytes();
            driver.abort();
            return Err(refused("change stream", status, parse_body(&bytes)));
        }
        Ok(Subscription {
            body: resp.into_body(),
            driver,
            _sender: sender,
            buf: Vec::new(),
            done: false,
        })
    }

    async fn open_plugin_stream(
        &self,
        conn: &Connection,
        plugin: &str,
        operation: &str,
        input: Value,
    ) -> Result<PluginStream> {
        let (send, recv) = conn.open_bi().await?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io).await?;
        let driver = tokio::spawn(driver);
        let payload = serde_json::to_vec(
            &json!({ "plugin": plugin, "operation": operation, "input": input }),
        )?;
        let req = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri("/v1/call")
            .header("host", "erisdb")
            .header("authorization", format!("Bearer {}", self.token.read().await))
            .header("x-erisdb-client", &self.client_name)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(payload)))?;
        let resp = sender.send_request(req).await?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let request_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        Ok(PluginStream {
            status,
            content_type,
            request_id,
            body: resp.into_body(),
            driver,
            _sender: sender,
        })
    }
}

/// A raw streaming response from one plugin invocation. The status belongs
/// to the plugin/upstream and is exposed even when it is not 2xx. Dropping
/// this value closes the response; the core then kills the one-shot process.
pub struct PluginStream {
    pub status: u16,
    pub content_type: Option<String>,
    pub request_id: Option<String>,
    body: hyper::body::Incoming,
    driver: tokio::task::JoinHandle<std::result::Result<(), hyper::Error>>,
    _sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
}

impl PluginStream {
    /// The next body chunk exactly as the plugin emitted it, or `None` after
    /// EOF. SSE parsing, JSON decoding, and other media semantics belong to
    /// the caller because plugin content types are deliberately open.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.body.frame().await {
                Some(frame) => match frame?.into_data() {
                    Ok(bytes) => return Ok(Some(bytes.to_vec())),
                    Err(_) => continue,
                },
                None => return Ok(None),
            }
        }
    }
}

impl Drop for PluginStream {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

// ---------------------------------------------------------------- pairing

/// The literal prefix of a pairing ticket. Everything after it is the
/// encoded payload.
pub const TICKET_SCHEME: &str = "erisdb://pair/";

/// The only ticket version this client understands. A ticket carrying
/// another is refused rather than guessed at.
pub const TICKET_VERSION: u64 = 1;

/// How often a waiting client asks whether the human has answered.
pub const PAIR_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A scanned pairing ticket: where the core is, and the code to start the
/// conversation with.
///
/// `token` is **not** a capability over any data. It grants exactly
/// `meta:pairing:redeem` on one session and expires in minutes, so a
/// photographed screen gets an attacker as far as raising a prompt on
/// somebody else's device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub v: u64,
    /// A label for the human — *paired with my-laptop*. Never trusted.
    pub name: Option<String>,
    /// Iroh endpoint id, 64 lowercase hex characters.
    pub eid: Option<String>,
    /// Plain HTTP base URL, for clients that cannot speak QUIC.
    pub url: Option<String>,
    /// The pairing code.
    pub token: String,
}

impl Ticket {
    /// Read `erisdb://pair/<base64url-nopad(JSON)>`.
    ///
    /// Everything the format says to refuse is refused here: a wrong
    /// prefix, a payload that does not decode, an unknown version, no
    /// address at all, no code, or an endpoint id that is not 64 hex
    /// characters. Refusing loudly beats storing half a config.
    pub fn parse(s: &str) -> Result<Self> {
        let payload = s
            .trim()
            .strip_prefix(TICKET_SCHEME)
            .ok_or_else(|| anyhow!("a pairing ticket starts with {TICKET_SCHEME}"))?;
        let json = B64
            .decode(payload)
            .map_err(|_| anyhow!("ticket payload is not unpadded base64url"))?;
        let v: Value =
            serde_json::from_slice(&json).context("ticket payload is not a ticket")?;

        let version = v["v"].as_u64().ok_or_else(|| anyhow!("ticket names no version"))?;
        if version != TICKET_VERSION {
            return Err(anyhow!(
                "ticket version {version} is not supported; this client speaks v{TICKET_VERSION}"
            ));
        }
        let text = |key: &str| v[key].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let ticket = Ticket {
            v: version,
            name: text("name"),
            eid: text("eid"),
            url: text("url"),
            token: text("token").ok_or_else(|| anyhow!("ticket carries no code"))?,
        };
        if ticket.eid.is_none() && ticket.url.is_none() {
            return Err(anyhow!(
                "ticket names no address: it needs an endpoint id, a url, or both"
            ));
        }
        if let Some(eid) = &ticket.eid {
            if eid.len() != 64 || !eid.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(anyhow!("endpoint id must be 64 hex characters"));
            }
        }
        Ok(ticket)
    }

    /// The endpoint id to dial. A ticket may legally carry only a `url` —
    /// one QR serves a browser on the LAN and a phone anywhere — but this
    /// client speaks QUIC and nothing else, so it says so plainly.
    pub fn endpoint_id(&self) -> Result<&str> {
        self.eid.as_deref().ok_or_else(|| {
            anyhow!("this ticket carries no endpoint id, only a url; this client dials iroh")
        })
    }
}

/// A stop signal for a wait, shareable and clonable. Every clone names
/// the same signal, so the thread showing a pairing screen can hand one
/// to the wait and keep one for the back button.
#[derive(Clone)]
pub struct Cancel {
    tx: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self { tx: std::sync::Arc::new(tx), rx }
    }

    /// Stop the wait. Idempotent; a cancelled signal stays cancelled.
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Sleep, waking early if cancelled. True means cancelled — the
    /// watch channel carries the current value, so a cancel that lands
    /// before the wait starts is still seen.
    async fn sleep(&self, how_long: Duration) -> bool {
        let mut rx = self.rx.clone();
        if *rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = rx.changed() => changed.is_ok() && *rx.borrow(),
            _ = tokio::time::sleep(how_long) => *rx.borrow(),
        }
    }
}

/// Where a pairing stands, from the client's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pairing {
    /// Redeemed; no human has answered yet.
    Waiting,
    /// Approved. The core hands the token over exactly once, so this is
    /// the only time it is ever seen: persist it here or pair again.
    Approved { token: String, granted: Vec<String> },
    /// A human said no. Nothing was granted.
    Denied,
    /// The deadline passed with no answer.
    TimedOut,
    /// The caller stopped waiting.
    Cancelled,
}

/// What a token holds, as `GET /v1/permissions` reports it.
#[derive(Debug, Clone)]
pub struct Permissions {
    /// Permission patterns: `tasks:read`, `meta:facets:write`, `*`.
    pub grants: Vec<String>,
    /// Unix seconds; `None` never expires.
    pub exp: Option<i64>,
    /// Unix seconds past which no refresh extends this line.
    pub max_exp: Option<i64>,
    /// The signed identity this token writes as. Attribution, not
    /// privilege.
    pub user: Option<String>,
    /// The core's answer as it sent it.
    pub raw: Value,
}

/// Pair against the core a scanned ticket names: dial it with the code,
/// redeem the code with this client's manifest, and wait for a human.
///
/// The endpoint id is resolved through discovery, which is what a phone
/// holding nothing but a QR code has. A caller that already knows the
/// address — a test, or an app that paired here before — dials it itself
/// and uses [`Client::pair`].
pub async fn pair(
    ticket: &Ticket,
    client_name: &str,
    requested: &[&str],
    identity: Option<[u8; 32]>,
    within: Duration,
    cancel: &Cancel,
) -> Result<Pairing> {
    let client =
        Client::dial(ticket.endpoint_id()?, &ticket.token, client_name, identity).await?;
    client.pair(client_name, requested, within, cancel).await
}

/// Percent-encode everything outside the unreserved set, so a facet name
/// survives as one query value whatever characters it uses.
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
/// client, every answer a value.
///
/// Nothing here panics across a boundary. Failures come back as JSON
/// envelopes, poisoned locks are opened anyway, and [`guard`] catches an
/// unwind before it can reach an `extern "system"` frame — where it would
/// abort the whole process rather than return.
pub mod blocking {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use tokio::sync::Mutex as TokioMutex;

    /// The facade's runtime.
    ///
    /// Every exchange spawns a hyper driver onto it, and a live
    /// subscription keeps one there for as long as the feed is open —
    /// they are tasks and share workers cooperatively, but a pool of two
    /// leaves very little room when an app syncs and serves a screen at
    /// the same time. Four is the floor, and a bigger machine gets more,
    /// up to eight.
    fn runtime() -> &'static tokio::runtime::Runtime {
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            let workers =
                std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(4, 8);
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .expect("runtime builds")
        })
    }

    /// Lock, poisoned or not. A panic elsewhere in the process poisons
    /// every lock it held; treating that as fatal would turn one bug into
    /// a permanently dead FFI surface, so the data is taken as it stands.
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `f` with unwinding contained. A panic becomes `on_panic` of its
    /// message — data, like every other failure. Every FFI entry point
    /// goes through here: unwinding out of an `extern "system"` fn aborts
    /// the process, taking the app with it.
    pub fn guard<T>(f: impl FnOnce() -> T, on_panic: impl FnOnce(String) -> T) -> T {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            Ok(value) => value,
            Err(payload) => on_panic(panic_message(&*payload)),
        }
    }

    /// The best one-line account of a caught panic.
    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            return format!("panic: {s}");
        }
        if let Some(s) = payload.downcast_ref::<String>() {
            return format!("panic: {s}");
        }
        "panic: (no message)".to_string()
    }

    /// The envelope a caught panic reports as. It carries both this
    /// module's failure shapes — `status: 0` and `ok: false` — so every
    /// caller, whichever one it reads, sees a failure.
    pub fn panic_envelope(message: String) -> String {
        serde_json::json!({"status": 0, "ok": false, "error": message}).to_string()
    }

    /// 64 characters of ASCII hex as 32 bytes; anything else is `None`.
    ///
    /// Length is counted in characters, not bytes, and the digits are
    /// read one character at a time: a 64-byte string of multi-byte
    /// characters is rejected rather than sliced through the middle of
    /// one.
    pub fn decode_identity_hex(s: &str) -> Option<[u8; 32]> {
        let digits: Vec<char> = s.trim().chars().collect();
        if digits.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (byte, pair) in out.iter_mut().zip(digits.as_chunks::<2>().0) {
            let hi = pair[0].to_digit(16)?;
            let lo = pair[1].to_digit(16)?;
            *byte = (hi * 16 + lo) as u8;
        }
        Some(out)
    }

    /// The status of a refusal, or 0 when the failure was the transport's.
    fn status_of(error: &anyhow::Error) -> u16 {
        error.downcast_ref::<Refused>().map_or(0, |refused| refused.status)
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
        *lock(client_slot()) = Some(std::sync::Arc::new(client));
        Ok(())
    }

    /// One API call; the response is always a JSON string:
    /// `{"status": n, "body": …}` on an exchange, `{"status": 0, "error": …}`
    /// when the transport failed or nothing is configured.
    pub fn request(method: &str, path: &str, body_json: Option<&str>) -> String {
        let client = match lock(client_slot()).clone() {
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
    /// success, `{"ok":false,"status":n,"error":…}` otherwise. Persisting
    /// the token is the caller's job.
    ///
    /// `status` is the core's answer — 401 for a token too dead to
    /// refresh, 403 for one the core will not widen — and 0 when the call
    /// never got an answer at all. Callers branch on that number; the
    /// message is for humans.
    pub fn refresh_capability(ttl_secs: i64) -> String {
        let client = match lock(client_slot()).clone() {
            Some(c) => c,
            None => return r#"{"ok":false,"status":0,"error":"not configured"}"#.to_string(),
        };
        match runtime().block_on(client.refresh_capability(ttl_secs)) {
            Ok(token) => serde_json::json!({"ok": true, "token": token}).to_string(),
            Err(e) => failure(&e),
        }
    }

    /// The caller's own grants: `{"ok":true,"permissions":{grants, exp,
    /// max_exp, user}}`, or the usual failure envelope. Needs no
    /// permission, so an app can always ask what it may offer to do.
    pub fn permissions() -> String {
        let client = match lock(client_slot()).clone() {
            Some(c) => c,
            None => return r#"{"ok":false,"status":0,"error":"not configured"}"#.to_string(),
        };
        match runtime().block_on(client.permissions()) {
            Ok(held) => serde_json::json!({"ok": true, "permissions": held.raw}).to_string(),
            Err(e) => failure(&e),
        }
    }

    /// The `{"ok":false,"status":n,"error":…}` envelope. `status` is the
    /// core's answer, or 0 when the call never got one.
    fn failure(e: &anyhow::Error) -> String {
        serde_json::json!({"ok": false, "status": status_of(e), "error": e.to_string()}).to_string()
    }

    // ------------------------------------------------------------ pairing
    //
    // Pairing is a pull, like the change feed: redeem once, then ask for
    // an answer with a timeout on every call. A human may never answer,
    // so no call here blocks forever, and `pair_cancel` wakes a parked
    // one rather than leaving a thread to run its timeout out.

    /// The pairing in progress: a client dialed with the code, and the
    /// signal that stops waiting on it.
    struct PairingSession {
        client: Arc<Client>,
        cancel: Cancel,
    }

    fn pairing_slot() -> &'static Mutex<Option<PairingSession>> {
        static PAIRING: OnceLock<Mutex<Option<PairingSession>>> = OnceLock::new();
        PAIRING.get_or_init(|| Mutex::new(None))
    }

    /// Read a `erisdb://pair/…` ticket:
    /// `{"ok":true,"ticket":{v, name, eid, url, token}}`, or
    /// `{"ok":false,"error":…}` naming what was wrong with it.
    ///
    /// Separate from redeeming on purpose: an app shows the core's name
    /// and asks the user before it dials anything.
    pub fn parse_ticket(ticket: &str) -> String {
        match Ticket::parse(ticket) {
            Ok(t) => serde_json::json!({"ok": true, "ticket": {
                "v": t.v,
                "name": t.name,
                "eid": t.eid,
                "url": t.url,
                "token": t.token,
            }})
            .to_string(),
            Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
        }
    }

    /// Dial `server` with a ticket's `code` and redeem it, naming this
    /// client and the grants it wants. `requested_json` is a JSON array
    /// of permission strings.
    ///
    /// `server` is what [`configure`] takes — the ticket's `eid`, or an
    /// `EndpointAddr` JSON for an address already known. The answer is
    /// `{"ok":true,"pairing":…}` with the session as the core sees it,
    /// or the usual failure envelope. Redeeming replaces any pairing
    /// already in progress.
    pub fn pair_redeem(
        server: &str,
        code: &str,
        client_name: &str,
        requested_json: &str,
        identity: &[u8],
    ) -> String {
        let identity: [u8; 32] = match identity.try_into() {
            Ok(bytes) => bytes,
            Err(_) => {
                return r#"{"ok":false,"status":0,"error":"identity must be 32 bytes"}"#.to_string()
            }
        };
        let requested: Vec<String> = match serde_json::from_str(requested_json) {
            Ok(r) => r,
            Err(e) => {
                return serde_json::json!({
                    "ok": false,
                    "status": 0,
                    "error": format!("requested must be a JSON array of permissions: {e}"),
                })
                .to_string()
            }
        };
        let asked: Vec<&str> = requested.iter().map(String::as_str).collect();

        let outcome = runtime().block_on(async {
            let client = Client::dial(server, code, client_name, Some(identity)).await?;
            let session = client.redeem_pairing(client_name, &asked).await?;
            Ok::<_, anyhow::Error>((client, session))
        });
        match outcome {
            Ok((client, session)) => {
                *lock(pairing_slot()) =
                    Some(PairingSession { client: Arc::new(client), cancel: Cancel::new() });
                serde_json::json!({"ok": true, "pairing": session}).to_string()
            }
            Err(e) => failure(&e),
        }
    }

    /// Wait up to `timeout_ms` for the human's answer. Always a value:
    ///
    /// ```text
    /// {"ok":true,"status":"waiting"}                        ask again
    /// {"ok":true,"status":"approved","token":…,"granted":[…]}
    /// {"ok":true,"status":"denied"}
    /// {"ok":true,"status":"cancelled"}
    /// {"ok":false,"status":n,"error":…}
    /// ```
    ///
    /// `approved` is the only time the token is ever seen — the core
    /// hands it over once — so persist it before returning. A settled
    /// pairing clears the slot, and asking again says there is none.
    pub fn pair_poll(timeout_ms: u64) -> String {
        let (client, cancel) = match lock(pairing_slot()).as_ref() {
            Some(s) => (s.client.clone(), s.cancel.clone()),
            None => return r#"{"ok":false,"status":0,"error":"no pairing in progress"}"#.to_string(),
        };
        let waited =
            runtime().block_on(client.await_pairing(Duration::from_millis(timeout_ms), &cancel));
        let settled = |answer: serde_json::Value| {
            lock(pairing_slot()).take();
            answer.to_string()
        };
        match waited {
            // A timeout is not an outcome, it is "no answer yet": the
            // caller loops, exactly as it does on the change feed.
            Ok(Pairing::Waiting) | Ok(Pairing::TimedOut) => {
                r#"{"ok":true,"status":"waiting"}"#.to_string()
            }
            Ok(Pairing::Approved { token, granted }) => settled(
                serde_json::json!({"ok": true, "status": "approved", "token": token, "granted": granted}),
            ),
            Ok(Pairing::Denied) => settled(serde_json::json!({"ok": true, "status": "denied"})),
            Ok(Pairing::Cancelled) => {
                settled(serde_json::json!({"ok": true, "status": "cancelled"}))
            }
            // A blip on the way to the core is not the end of the
            // session: the slot stays and the caller may ask again.
            Err(e) => failure(&e),
        }
    }

    /// Stop waiting and drop the pairing. Wakes a parked [`pair_poll`],
    /// which answers `cancelled`. A no-op when nothing is pairing.
    pub fn pair_cancel() {
        if let Some(session) = lock(pairing_slot()).take() {
            session.cancel.cancel();
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
        let client = match lock(client_slot()).clone() {
            Some(c) => c,
            None => return Err("not configured".to_string()),
        };
        let sub = runtime()
            .block_on(client.subscribe_changes(since, facet))
            .map_err(|e| e.to_string())?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let handle = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        lock(subscriptions()).insert(handle, Arc::new(TokioMutex::new(sub)));
        Ok(handle)
    }

    /// Block for up to `timeout_ms` on the next change. The response is
    /// always a JSON string: `{"ok":true,"change":…}` when one arrives,
    /// `{"ok":true}` when the wait elapsed with the stream still live,
    /// and `{"ok":false,"error":…}` when the stream ended, the handle is
    /// unknown, or a frame failed to parse.
    pub fn next_change(handle: SubscriptionHandle, timeout_ms: u64) -> String {
        let sub = match lock(subscriptions()).get(&handle) {
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
        lock(subscriptions()).remove(&handle);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// One panic must not brick the surface. A poisoned lock still
        /// hands over its data, so the call after the bad one works.
        #[test]
        fn a_poisoned_lock_still_opens() {
            let m = Arc::new(Mutex::new(vec!["before"]));
            let poisoner = m.clone();
            let _ = std::thread::spawn(move || {
                let _held = poisoner.lock().unwrap();
                panic!("poison it");
            })
            .join();

            assert!(m.lock().is_err(), "the lock is poisoned");
            assert_eq!(*lock(&m), vec!["before"]);
            lock(&m).push("after");
            assert_eq!(*lock(&m), vec!["before", "after"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only methods that change nothing are safe to send twice.
    #[test]
    fn nullipotent_methods_are_the_readable_ones() {
        for method in ["GET", "get", "HEAD", "OPTIONS"] {
            assert!(nullipotent(method), "{method}");
        }
        for method in ["POST", "post", "PUT", "DELETE", "PATCH"] {
            assert!(!nullipotent(method), "{method}");
        }
    }

    /// A body reaches the caller whatever it is.
    #[test]
    fn a_body_is_json_text_or_nothing() {
        assert_eq!(parse_body(br#"{"ok":true}"#), serde_json::json!({"ok": true}));
        assert_eq!(parse_body(b"gateway exploded"), serde_json::json!("gateway exploded"));
        assert_eq!(parse_body(b""), Value::Null);
    }
}
