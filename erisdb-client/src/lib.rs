//! The client of `database/1`, the database a mesh has one of and ErisDB
//! serves: HTTP/1.1 per QUIC bi-stream, ALPN `eris/database/1`.
//!
//! An app asks a device in its owner's mesh to use [`CAPABILITY`]: the
//! device approves the app's key with a note, the person types the
//! database's passphrase, and the device enrolls the note at the mesh's
//! database over the mesh library's
//! [`enroll`](erisoverlaynetwork::enroll). From then on the app's key is its
//! credential. It dials the database as that key and sends no token; the
//! database knows it by the key the QUIC handshake proved.
//!
//! [`sign_in`] does the app's side of that through the device's eon in one
//! call.
//!
//! The async [`Client`] is the real thing; [`blocking`] wraps it in an owned
//! runtime for FFI callers (JNI has no executor). The Android bindings live
//! in `android` and compile only for that target.
//!
//! Two rules run through all of it. A failed call is repeated only when
//! repeating it is provably harmless, because the protocol has no
//! idempotency key and a retried `POST` is a second item. And nothing
//! leaves the FFI surface as a panic: refusals carry a status, transport
//! failures carry a message, and an unwind is caught before it reaches a
//! frame that would abort the process.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use erisoverlaynetwork::net::{self, Network};
use erisoverlaynetwork::{Capability, Note, Snapshot};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use serde_json::{json, Value};

#[cfg(target_os = "android")]
mod android;

/// What this crate is a client of: the database ErisDB serves, and what
/// apps ask a device to use.
pub const CAPABILITY: &str = "database/1";

/// [`CAPABILITY`], parsed.
pub fn capability() -> Capability {
    CAPABILITY.parse().expect("database/1 is a capability")
}

/// The ALPN the database answers on: `eris/database/1`.
pub fn alpn() -> Vec<u8> {
    capability().alpn()
}

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
    method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") || method.eq_ignore_ascii_case("OPTIONS")
}

/// A response body as a value: JSON where it is JSON, the raw text where
/// it is not, and null where there is none. An error page from something
/// other than the core still reaches the caller instead of vanishing
/// behind a bare status.
fn parse_body(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|value| value.as_str().map(str::to_string)).collect()
}

/// Where the mesh's database is, as `snapshot` has it: the app approved to
/// serve `database/1` whose note counts, the most recently approved should
/// there be more than one. Nothing ever talks to any other.
pub fn locate(snapshot: &Snapshot) -> Result<EndpointAddr> {
    snapshot
        .server(CAPABILITY)
        .map(|presence| presence.endpoint_addr())
        .ok_or_else(|| anyhow!("this mesh has no database, or the snapshot does not say where it is"))
}

/// One app's connection to one database.
pub struct Client {
    endpoint: Endpoint,
    server: EndpointAddr,
    client_name: String,
    conn: tokio::sync::Mutex<Option<Connection>>,
}

impl Client {
    /// Dial `server` as the app holding `key`, on the network `ERIS_NETWORK`
    /// names (the internet when it names none). `name` is stamped into
    /// every write's source as the claimed client.
    ///
    /// The key is the app's credential: keep the same one across launches,
    /// since it is the key a device approved.
    pub async fn dial(server: EndpointAddr, key: SecretKey, name: &str) -> Result<Self> {
        Self::dial_on(server, key, name, &Network::from_env()).await
    }

    /// [`Client::dial`] on a network the caller names.
    pub async fn dial_on(server: EndpointAddr, key: SecretKey, name: &str, network: &Network) -> Result<Self> {
        let endpoint = net::endpoint(key, network).await?;
        Ok(Self::over(endpoint, server, name))
    }

    /// A client on an endpoint the caller already runs.
    pub fn over(endpoint: Endpoint, server: EndpointAddr, name: &str) -> Self {
        Self { endpoint, server, client_name: name.to_string(), conn: tokio::sync::Mutex::new(None) }
    }

    /// The database this client dials: what an app saves to dial it again.
    pub fn server(&self) -> &EndpointAddr {
        &self.server
    }

    /// The key this client calls as.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
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
    pub async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let conn = self.connection(false).await?;
        match self.exchange(&conn, method, path, &body).await {
            Ok(r) => Ok(r),
            Err(failed) if failed.stage == Stage::BeforeSend || nullipotent(method) => {
                let conn = self.connection(true).await?;
                self.exchange(&conn, method, path, &body).await.map_err(|f| f.error)
            }
            Err(failed) => Err(failed.error),
        }
    }

    /// Invoke one plugin operation and buffer its response. The core starts
    /// one fresh executable for this call and stores nothing. Any status is
    /// returned to the caller because a non-2xx response may be the external
    /// API's ordinary error format rather than a core refusal.
    pub async fn call_plugin(&self, plugin: &str, operation: &str, input: Value) -> Result<(u16, Value)> {
        self.request("POST", "/v1/call", Some(json!({ "plugin": plugin, "operation": operation, "input": input }))).await
    }

    /// Invoke one plugin operation without buffering its response. This is
    /// the path for SSE and any other streamed plugin body. A `POST` is never
    /// retried after a transport failure: the external system may already
    /// have received it.
    pub async fn stream_plugin(&self, plugin: &str, operation: &str, input: Value) -> Result<PluginStream> {
        let conn = self.connection(false).await?;
        let body = Some(json!({ "plugin": plugin, "operation": operation, "input": input }));
        self.open_response(&conn, "POST", "/v1/call", &body).await.map_err(|failed| failed.error)
    }

    /// What this app holds: its grants, and the installation they belong to.
    ///
    /// Needs no permission, so it is the one call that always works for an
    /// enrolled app, and the right way for one to find out what it may
    /// offer to do.
    pub async fn permissions(&self) -> Result<Permissions> {
        let (status, body) = self.request("GET", "/v1/permissions", None).await?;
        if status != 200 {
            return Err(refused("permissions", status, body));
        }
        Ok(Permissions {
            grants: strings(&body["grants"]),
            exp: body["exp"].as_i64(),
            max_exp: body["max_exp"].as_i64(),
            user: body["user"].as_str().map(str::to_string),
            client: body["client_id"].as_str().map(str::to_string),
            raw: body,
        })
    }

    /// Subscribe to the change feed from `since` (exclusive), optionally
    /// narrowed to one facet. The subscription holds one bi-stream open
    /// for its lifetime and yields events as the core emits them; it ends
    /// when the core closes it, as it does once this app stops counting.
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
        let conn = tokio::time::timeout(Duration::from_secs(30), self.endpoint.connect(self.server.clone(), &alpn()))
            .await
            .context("connecting to the database timed out")??;
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
        let mut response = self.open_response(conn, method, path, body).await?;
        let bytes = (&mut response.body).collect().await.map_err(Failed::after_send)?.to_bytes();
        Ok((response.status, parse_body(&bytes)))
    }

    async fn open_response(
        &self,
        conn: &Connection,
        method: &str,
        path: &str,
        body: &Option<Value>,
    ) -> std::result::Result<PluginStream, Failed> {
        // Everything up to `send_request` is local: opening a QUIC stream
        // reserves an id without a round trip, and hyper's HTTP/1
        // handshake writes nothing. A failure in here never reached the
        // server.
        let (send, recv) = conn.open_bi().await.map_err(Failed::before_send)?;
        let io = TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io).await.map_err(Failed::before_send)?;
        let driver = tokio::spawn(driver);

        let mut req = hyper::Request::builder()
            .method(hyper::Method::from_bytes(method.as_bytes()).map_err(Failed::before_send)?)
            .uri(path)
            .header("host", "erisdb")
            .header("accept", "application/json, text/event-stream")
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
        let header = |name| resp.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
        Ok(PluginStream {
            status: resp.status().as_u16(),
            content_type: header("content-type"),
            request_id: header("x-request-id"),
            body: resp.into_body(),
            driver,
            _sender: sender,
        })
    }

    async fn open_stream(&self, conn: &Connection, path: &str) -> Result<Subscription> {
        let mut stream = self.open_response(conn, "GET", path, &None).await.map_err(|failed| failed.error)?;
        if stream.status != 200 {
            let bytes = (&mut stream.body).collect().await?.to_bytes();
            return Err(refused("change stream", stream.status, parse_body(&bytes)));
        }
        Ok(Subscription { stream, buf: Vec::new(), done: false })
    }
}

/// What an app holds, as `GET /v1/permissions` reports it.
#[derive(Debug, Clone)]
pub struct Permissions {
    /// Permission patterns: `tasks:read`, `meta:facets:write`, `*`.
    pub grants: Vec<String>,
    /// Unix seconds when a token stops working; `None` for an app calling
    /// by its key, which does not expire.
    pub exp: Option<i64>,
    /// Unix seconds past which a token's refresh chain ends.
    pub max_exp: Option<i64>,
    /// The signed user a token writes as. Attribution, not privilege.
    pub user: Option<String>,
    /// The installation these grants belong to.
    pub client: Option<String>,
    /// The core's answer as it sent it.
    pub raw: Value,
}

/// How signing in through eon ended. Made once per sign-in, so its size is
/// no matter.
#[allow(clippy::large_enum_variant)]
pub enum SignIn {
    /// The person approved, and the database enrolled the app: a client
    /// dialed at the mesh's database, the note that approves the app, and the
    /// mesh as the device saw it. Save [`Client::server`] with the key to
    /// dial again; the note's grants in [`CAPABILITY`] may be fewer than were
    /// asked for.
    Approved { client: Client, note: Note, snapshot: Snapshot },
    /// The person, or eon, said no.
    Denied(String),
}

/// Sign in through this device's eon: ask it to approve the app holding
/// `key` to use the database with `grants`. The person approves on eon's
/// screen and types the database's passphrase there; eon enrolls the app at
/// the mesh's database and answers once the database has taken it. The
/// returned client is dialed there, on the network `ERIS_NETWORK` names.
///
/// `None` means no eon is listening here. Dropping the future withdraws the
/// request.
#[cfg(unix)]
pub async fn sign_in(key: SecretKey, name: &str, grants: &[String]) -> Result<Option<SignIn>> {
    match erisoverlaynetwork::agent::socket_path() {
        Some(socket) => sign_in_at(&socket, &Network::from_env(), key, name, grants).await,
        None => Ok(None),
    }
}

/// [`sign_in`] with eon listening at `socket`, dialing the database on `network`.
#[cfg(unix)]
pub async fn sign_in_at(
    socket: &std::path::Path,
    network: &Network,
    key: SecretKey,
    name: &str,
    grants: &[String],
) -> Result<Option<SignIn>> {
    use erisoverlaynetwork::agent::{Answer, Ask};
    let Some(mut eon) = erisoverlaynetwork::agent::connect_to(socket).await? else { return Ok(None) };
    let uses = erisoverlaynetwork::Uses::from([(capability(), grants.to_vec())]);
    let ask = Ask { name: name.into(), uses, serves: Vec::new(), app: key.public() };
    match eon.approve(&ask).await? {
        Answer::Denied(why) => Ok(Some(SignIn::Denied(why))),
        Answer::Approved { note, snapshot } => {
            let server = locate(&snapshot)?;
            let client = Client::dial_on(server, key, name, network).await?;
            Ok(Some(SignIn::Approved { client, note, snapshot }))
        }
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

/// Percent-encode everything outside the unreserved set, so a facet name
/// survives as one query value whatever characters it uses.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
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
    stream: PluginStream,
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
            match self.stream.body.frame().await {
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

/// A database's address as an app keeps it: a JSON `EndpointAddr`, or a bare
/// endpoint id (hex, with or without an `iroh:` prefix) for discovery to
/// resolve.
pub fn parse_server(server: &str) -> Result<EndpointAddr> {
    let server = server.trim();
    if server.starts_with('{') {
        return serde_json::from_str(server).context("parsing the database's address JSON");
    }
    let id: EndpointId =
        server.strip_prefix("iroh:").unwrap_or(server).parse().map_err(|e| anyhow!("bad endpoint id: {e}"))?;
    Ok(id.into())
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
            let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(4, 8);
            tokio::runtime::Builder::new_multi_thread().worker_threads(workers).enable_all().build().expect("runtime builds")
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
    pub fn decode_key_hex(s: &str) -> Option<[u8; 32]> {
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

    /// The endpoint id a key dials as, in hex: what an app names to eon as
    /// itself. `None` for anything but 64 hex characters.
    pub fn endpoint_id(key_hex: &str) -> Option<String> {
        decode_key_hex(key_hex).map(|secret| SecretKey::from_bytes(&secret).public().to_string())
    }

    /// Where the mesh's database is, from the snapshot eon handed over with
    /// an approval: `{"ok":true,"server":<EndpointAddr JSON>}`, or
    /// `{"ok":false,"error":…}`. Save `server` and dial it with [`dial`].
    pub fn locate(snapshot_json: &str) -> String {
        let located = serde_json::from_str::<Snapshot>(snapshot_json)
            .context("reading the snapshot")
            .and_then(|snapshot| super::locate(&snapshot));
        match located {
            Ok(addr) => serde_json::json!({"ok": true, "server": addr}).to_string(),
            Err(e) => serde_json::json!({"ok": false, "error": format!("{e:#}")}).to_string(),
        }
    }

    /// The status of a refusal, or 0 when the failure was the transport's.
    fn status_of(error: &anyhow::Error) -> u16 {
        error.downcast_ref::<Refused>().map_or(0, |refused| refused.status)
    }

    fn client_slot() -> &'static Mutex<Option<Arc<Client>>> {
        static CLIENT: OnceLock<Mutex<Option<Arc<Client>>>> = OnceLock::new();
        CLIENT.get_or_init(|| Mutex::new(None))
    }

    /// (Re)dial the process-wide client: `server` is what [`locate`] gave
    /// (or a bare endpoint id), `key` the app's 32-byte key, on the network
    /// `ERIS_NETWORK` names.
    pub fn dial(server: &str, client_name: &str, key: &[u8]) -> std::result::Result<(), String> {
        dial_on(server, client_name, key, &Network::from_env())
    }

    /// [`dial`] on a network the caller names.
    pub fn dial_on(server: &str, client_name: &str, key: &[u8], network: &Network) -> std::result::Result<(), String> {
        let key: [u8; 32] = key.try_into().map_err(|_| "key must be 32 bytes".to_string())?;
        let server = parse_server(server).map_err(|e| e.to_string())?;
        let client = runtime()
            .block_on(Client::dial_on(server, SecretKey::from_bytes(&key), client_name, network))
            .map_err(|e| e.to_string())?;
        *lock(client_slot()) = Some(Arc::new(client));
        Ok(())
    }

    /// One API call; the response is always a JSON string:
    /// `{"status": n, "body": …}` on an exchange, `{"status": 0, "error": …}`
    /// when the transport failed or nothing is dialed.
    pub fn request(method: &str, path: &str, body_json: Option<&str>) -> String {
        let client = match lock(client_slot()).clone() {
            Some(c) => c,
            None => return r#"{"status":0,"error":"not dialed"}"#.to_string(),
        };
        let body = match body_json {
            Some(s) => match serde_json::from_str(s) {
                Ok(v) => Some(v),
                Err(e) => return serde_json::json!({"status": 0, "error": format!("bad body: {e}")}).to_string(),
            },
            None => None,
        };
        match runtime().block_on(client.request(method, path, body)) {
            Ok((status, body)) => serde_json::json!({"status": status, "body": body}).to_string(),
            Err(e) => serde_json::json!({"status": 0, "error": e.to_string()}).to_string(),
        }
    }

    /// The app's own grants: `{"ok":true,"permissions":{grants, client_id, …}}`,
    /// or `{"ok":false,"status":n,"error":…}`. Needs no permission, so an
    /// enrolled app can always ask what it may offer to do; 401 means it is
    /// not, or no longer, let in.
    pub fn permissions() -> String {
        let client = match lock(client_slot()).clone() {
            Some(c) => c,
            None => return r#"{"ok":false,"status":0,"error":"not dialed"}"#.to_string(),
        };
        match runtime().block_on(client.permissions()) {
            Ok(held) => serde_json::json!({"ok": true, "permissions": held.raw}).to_string(),
            Err(e) => serde_json::json!({"ok": false, "status": status_of(&e), "error": e.to_string()}).to_string(),
        }
    }

    /// A change-feed subscription, addressed by handle. Callbacks across
    /// FFI are painful, so the feed is a pull: the caller parks a thread
    /// in [`next_change`] and gets one event per return.
    pub type SubscriptionHandle = u64;

    type Subscriptions = Mutex<HashMap<SubscriptionHandle, Arc<TokioMutex<Subscription>>>>;

    fn subscriptions() -> &'static Subscriptions {
        static SUBS: OnceLock<Subscriptions> = OnceLock::new();
        SUBS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Open a subscription from `since` (exclusive), optionally narrowed
    /// to one facet, and return its handle. The caller owns the cursor:
    /// remember the `seq` of the last event it processed and pass it as
    /// `since` on the next subscribe to resume without loss.
    pub fn subscribe_changes(since: i64, facet: Option<&str>) -> std::result::Result<SubscriptionHandle, String> {
        let client = match lock(client_slot()).clone() {
            Some(c) => c,
            None => return Err("not dialed".to_string()),
        };
        let sub = runtime().block_on(client.subscribe_changes(since, facet)).map_err(|e| e.to_string())?;
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
                Ok(Ok(Some(event))) => serde_json::json!({"ok": true, "change": event.raw}).to_string(),
                Ok(Ok(None)) => r#"{"ok":false,"error":"stream ended"}"#.to_string(),
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
