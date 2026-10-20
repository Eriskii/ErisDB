//! ErisDB for browser apps, as WebAssembly: an app signs in by an ErisAuth ticket and calls
//! ErisDB over Iroh, through a relay, as its own key.
//!
//! The app holds a key of its own, which is its endpoint id and, once a device has let it in,
//! its only credential. It signs in by redeeming a ticket a device of its owner's mesh showed
//! (ErisAuth's **Add a browser app**): both screens show the request's fingerprint, the person
//! approves on the device and types ErisDB's passphrase there, the device enrolls the app at
//! ErisDB, and the app collects its note and a snapshot of the mesh. It finds ErisDB in the
//! snapshot, as the app approved to serve `erisdb`, and dials it with ALPN `erisdb/0`: one
//! HTTP/1.1 exchange per QUIC bi-stream, carrying no token.
//!
//! Everything a call can fail with arrives in JavaScript as an `Error` with a `code`: `bad_key`,
//! `bad_ticket`, `no_relay`, `no_erisdb`, `unreachable`, `used`, `expired`, `refused`, `bad_request`,
//! `internal`, and, from a change feed ErisDB would not open, `refused` with the `status` and `body`
//! it answered.

use std::cell::RefCell;
use std::rc::Rc;

use erisoverlaynetwork::Snapshot;
use erisoverlaynetwork::ticket::Ticket;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, RelayUrl, SecretKey};
use js_sys::{Promise, Reflect};
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

mod http;
mod net;
mod sign_in;

/// ErisDB's wire protocol: HTTP/1.1 inside QUIC bi-streams.
pub const ALPN: &[u8] = b"erisdb/0";

/// How long dialing ErisDB may take.
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(console_error_panic_hook::hook));
}

/// An `Error` carrying `code`.
fn error(code: &str, message: impl AsRef<str>) -> JsValue {
    let error = js_sys::Error::new(message.as_ref());
    let _ = Reflect::set(&error, &"code".into(), &code.into());
    error.into()
}

/// A refusal from the mesh's side of things, as an `Error` a page can act on.
fn mesh_error(e: erisoverlaynetwork::Error) -> JsValue {
    use erisoverlaynetwork::Error;
    match e {
        Error::Unreachable(why) => error("unreachable", why),
        Error::Conflict(_) => error("used", "this ticket has been used already"),
        Error::Expired | Error::Unauthorized => error("expired", "this ticket no longer works"),
        Error::Forbidden { permission } => error("refused", format!("the device requires {permission}")),
        Error::BadRequest(why) => error("bad_request", why),
        other => error("internal", other.to_string()),
    }
}

fn to_js(value: &impl Serialize) -> Result<JsValue, JsValue> {
    let text = serde_json::to_string(value).map_err(|e| error("internal", e.to_string()))?;
    js_sys::JSON::parse(&text)
}

fn from_js<T: serde::de::DeserializeOwned>(value: &JsValue, what: &str) -> Result<T, JsValue> {
    if value.is_undefined() || value.is_null() {
        return Err(error("bad_request", format!("{what} is missing")));
    }
    let text: String =
        js_sys::JSON::stringify(value).map_err(|_| error("bad_request", format!("{what} is not JSON")))?.into();
    serde_json::from_str(&text).map_err(|e| error("bad_request", format!("{what} does not read: {e}")))
}

fn secret(key: &[u8]) -> Result<SecretKey, JsValue> {
    let bytes: [u8; 32] = key.try_into().map_err(|_| error("bad_key", "an app's key is 32 bytes"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// A new key for an app: 32 bytes for the page to keep, the same every time it loads.
#[wasm_bindgen(js_name = generateKey)]
pub fn generate_key() -> Vec<u8> {
    SecretKey::generate().to_bytes().to_vec()
}

/// The endpoint id `key` calls as, in hex: what ErisAuth and ErisDB know the app by.
#[wasm_bindgen(js_name = appId)]
pub fn app_id(key: &[u8]) -> Result<String, JsValue> {
    Ok(secret(key)?.public().to_string())
}

#[derive(Serialize)]
struct TicketView {
    /// The device's endpoint id.
    device: String,
    /// The device's name, as the ticket gives it. Trusted for nothing.
    name: Option<String>,
    relay: Option<String>,
}

fn ticket(text: &str) -> Result<(Ticket, RelayUrl), JsValue> {
    let mut ticket = Ticket::parse(text).map_err(|e| {
        let why = e.to_string();
        error("bad_ticket", why.strip_prefix("bad request: ").unwrap_or(&why))
    })?;
    let relay = ticket.relay.as_deref().and_then(|relay| relay.parse::<RelayUrl>().ok()).ok_or_else(|| {
        error("no_relay", "the ticket names no relay, and a browser reaches a device only through one")
    })?;
    // A browser has no direct paths to dial.
    ticket.addrs.clear();
    Ok((ticket, relay))
}

/// Read an `eris://pair/` ticket: `{device, name, relay}`. Throws `bad_ticket` or `no_relay`.
#[wasm_bindgen(js_name = readTicket)]
pub fn read_ticket(text: &str) -> Result<JsValue, JsValue> {
    let (ticket, relay) = ticket(text)?;
    to_js(&TicketView { device: ticket.eid.to_string(), name: ticket.name.clone(), relay: Some(relay.to_string()) })
}

/// What a page keeps of a sign-in under way, to wait for its answer again after a reload.
#[derive(Serialize, Deserialize)]
struct Saved {
    ticket: String,
    name: String,
    grants: Vec<String>,
    host: String,
}

/// A sign-in under way: the request is with the person on the device.
#[wasm_bindgen]
pub struct Joining {
    joining: Rc<sign_in::Joining>,
    saved: Rc<Saved>,
}

#[wasm_bindgen]
impl Joining {
    /// The request's fingerprint, computed on this page: the person compares it with the one
    /// the device shows before approving.
    #[wasm_bindgen(getter)]
    pub fn fingerprint(&self) -> String {
        self.joining.fingerprint().to_string()
    }

    /// The device's name for itself.
    #[wasm_bindgen(getter)]
    pub fn host(&self) -> String {
        self.joining.host().to_string()
    }

    /// What to keep to wait for this answer on a later page: give it to `resumeSignIn`.
    #[wasm_bindgen(getter)]
    pub fn saved(&self) -> Result<JsValue, JsValue> {
        to_js(&*self.saved)
    }

    /// The person's answer: `{note, snapshot}` once they approved and ErisDB enrolled the app,
    /// or `null` when they said no. Throws `expired` once the ticket ends unanswered.
    pub fn approval(&self) -> Promise {
        let joining = self.joining.clone();
        future_to_promise(async move {
            match joining.wait().await.map_err(mesh_error)? {
                Some(erisoverlaynetwork::ceremony::Approval::App { note, snapshot }) => {
                    to_js(&serde_json::json!({"note": note, "snapshot": snapshot}))
                }
                Some(_) => Err(error("refused", "the device answered as if this app were a device")),
                None => Ok(JsValue::NULL),
            }
        })
    }

    /// Take the request back: the device forgets it.
    pub fn withdraw(&self) -> Promise {
        let joining = self.joining.clone();
        future_to_promise(async move {
            joining.withdraw().await.map_err(mesh_error)?;
            Ok(JsValue::UNDEFINED)
        })
    }
}

/// Sign in by `ticket` as the app holding `key`, called `name`, asking for `grants` in ErisDB.
/// Resolves to a `Joining` once the device has the request.
#[wasm_bindgen(js_name = signIn)]
pub fn sign_in(key: &[u8], ticket_text: &str, name: &str, grants: Vec<String>) -> Promise {
    let parts = secret(key).and_then(|key| ticket(ticket_text).map(|(ticket, relay)| (key, ticket, relay)));
    let name = name.to_string();
    future_to_promise(async move {
        let (key, ticket, relay) = parts?;
        let endpoint = net::endpoint(&key, &relay).await.map_err(|why| error("unreachable", why))?;
        let joining =
            sign_in::redeem(&endpoint, &ticket, sign_in::ask(&name, grants.clone())).await.map_err(mesh_error)?;
        let saved = Saved {
            ticket: ticket.encode().map_err(|e| error("internal", e.to_string()))?,
            name,
            grants,
            host: joining.host().to_string(),
        };
        Ok(Joining { joining: Rc::new(joining), saved: Rc::new(saved) }.into())
    })
}

/// Wait again for the answer to a sign-in an earlier page made under `key`, from what its
/// `Joining.saved` held.
#[wasm_bindgen(js_name = resumeSignIn)]
pub fn resume_sign_in(key: &[u8], saved: JsValue) -> Promise {
    let parts = secret(key).and_then(|key| {
        let saved: Saved = from_js(&saved, "the saved sign-in")?;
        let (ticket, relay) = ticket(&saved.ticket)?;
        Ok((key, saved, ticket, relay))
    });
    future_to_promise(async move {
        let (key, saved, ticket, relay) = parts?;
        let endpoint = net::endpoint(&key, &relay).await.map_err(|why| error("unreachable", why))?;
        let joining =
            sign_in::resume(&endpoint, ticket, sign_in::ask(&saved.name, saved.grants.clone()), saved.host.clone())
                .map_err(mesh_error)?;
        Ok(Joining { joining: Rc::new(joining), saved: Rc::new(saved) }.into())
    })
}

/// Close the endpoint `key` runs on, and every connection on it.
#[wasm_bindgen(js_name = closeEndpoint)]
pub fn close_endpoint(key: &[u8]) -> Promise {
    let key = secret(key);
    future_to_promise(async move {
        net::close(&key?).await;
        Ok(JsValue::UNDEFINED)
    })
}

struct Inner {
    endpoint: Endpoint,
    server: EndpointAddr,
    name: String,
    conn: tokio::sync::Mutex<Option<Connection>>,
}

impl Inner {
    async fn connection(&self, fresh: bool) -> Result<Connection, JsValue> {
        let mut slot = self.conn.lock().await;
        if fresh {
            slot.take();
        }
        if let Some(conn) = slot.as_ref().filter(|conn| conn.close_reason().is_none()) {
            return Ok(conn.clone());
        }
        let conn = n0_future::time::timeout(DIAL_TIMEOUT, self.endpoint.connect(self.server.clone(), ALPN))
            .await
            .map_err(|_| error("unreachable", "ErisDB did not answer in time"))?
            .map_err(|e| error("unreachable", format!("could not reach ErisDB: {e}")))?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    async fn open(
        &self,
        conn: &Connection,
        method: &str,
        path: &str,
        body: &Option<Vec<u8>>,
    ) -> Result<http::Response, http::Failed> {
        http::open(conn, &self.name, method, path, body.clone()).await
    }

    /// One exchange, repeated on a fresh connection only when repeating it is harmless: when
    /// nothing was sent, or when the method changes nothing.
    async fn exchange(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> Result<(u16, Vec<u8>), JsValue> {
        let once = |conn: Connection| {
            let body = body.clone();
            async move {
                let response = self.open(&conn, method, path, &body).await?;
                let status = response.status;
                Ok::<_, http::Failed>((status, response.bytes().await?.to_vec()))
            }
        };
        match once(self.connection(false).await?).await {
            Ok(answer) => Ok(answer),
            Err(failed) if failed.stage == http::Stage::BeforeSend || http::nullipotent(method) => {
                once(self.connection(true).await?).await.map_err(|failed| error("unreachable", failed.message))
            }
            Err(failed) => Err(error("unreachable", failed.message)),
        }
    }
}

/// The app's connection to its mesh's ErisDB.
#[wasm_bindgen]
pub struct Client {
    inner: Rc<Inner>,
}

/// Connect to the ErisDB in `snapshot` as the app holding `key`, stamping `name` into every
/// write's source. ErisDB is the app approved to serve `erisdb`; the snapshot says which relay
/// reaches it. Dialing happens with the first request.
#[wasm_bindgen]
pub fn connect(key: &[u8], snapshot: JsValue, name: &str) -> Promise {
    let parts = secret(key).and_then(|key| {
        let snapshot: Snapshot = from_js(&snapshot, "the snapshot")?;
        let erisdb = snapshot
            .provider(sign_in::SERVICE)
            .ok_or_else(|| error("no_erisdb", "this mesh has no ErisDB, or the snapshot does not say where it is"))?;
        let relay = erisdb
            .relay
            .as_deref()
            .and_then(|relay| relay.parse::<RelayUrl>().ok())
            .ok_or_else(|| error("no_relay", "the mesh names no relay that reaches its ErisDB"))?;
        Ok((key, net::through(erisdb.id, &relay), relay))
    });
    let name = name.to_string();
    future_to_promise(async move {
        let (key, server, relay) = parts?;
        let endpoint = net::endpoint(&key, &relay).await.map_err(|why| error("unreachable", why))?;
        let inner = Inner { endpoint, server, name, conn: tokio::sync::Mutex::new(None) };
        Ok(Client { inner: Rc::new(inner) }.into())
    })
}

#[wasm_bindgen]
impl Client {
    /// ErisDB's endpoint id.
    #[wasm_bindgen(getter)]
    pub fn server(&self) -> String {
        self.inner.server.id.to_string()
    }

    /// One API call: `{status, body}`, the body JSON where it is JSON. `body`, when given, goes
    /// as JSON. Throws `unreachable` when ErisDB could not be asked, or did not answer.
    pub fn request(&self, method: String, path: String, body: JsValue) -> Promise {
        let inner = self.inner.clone();
        let body = if body.is_undefined() || body.is_null() {
            Ok(None)
        } else {
            js_sys::JSON::stringify(&body)
                .map(|text| Some(String::from(text).into_bytes()))
                .map_err(|_| error("bad_request", "the body is not JSON"))
        };
        future_to_promise(async move {
            let (status, bytes) = inner.exchange(&method, &path, body?).await?;
            let answer = js_sys::Object::new();
            Reflect::set(&answer, &"status".into(), &status.into())?;
            Reflect::set(&answer, &"body".into(), &to_js(&http::parse_body(&bytes))?)?;
            Ok(answer.into())
        })
    }

    /// Follow the change feed from `since` (exclusive), narrowed to `facet` when given:
    /// `/v1/changes/stream`. Resolves once ErisDB opens it; throws `refused`, with the `status`
    /// and `body` ErisDB answered, when it will not.
    pub fn changes(&self, since: f64, facet: Option<String>) -> Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let mut path = format!("/v1/changes/stream?since={}", since as i64);
            if let Some(facet) = facet {
                path.push_str("&facet=");
                path.push_str(&String::from(js_sys::encode_uri_component(&facet)));
            }
            // Opening a feed reads from a cursor the caller holds: opening it twice changes
            // nothing, so any failure redials.
            let response = match inner.open(&inner.connection(false).await?, "GET", &path, &None).await {
                Ok(response) => response,
                Err(_) => inner
                    .open(&inner.connection(true).await?, "GET", &path, &None)
                    .await
                    .map_err(|failed| error("unreachable", failed.message))?,
            };
            if response.status != 200 {
                let status = response.status;
                let body = response.bytes().await.map(|b| http::parse_body(&b)).unwrap_or_default();
                let refused = error("refused", format!("ErisDB would not open the change feed ({status})"));
                Reflect::set(&refused, &"status".into(), &status.into())?;
                Reflect::set(&refused, &"body".into(), &to_js(&body)?)?;
                return Err(refused);
            }
            Ok(Changes::follow(response).into())
        })
    }

    /// Hang up on ErisDB. The next request dials again.
    pub fn close(&self) {
        let inner = self.inner.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(conn) = inner.conn.lock().await.take() {
                conn.close(0u32.into(), b"closed");
            }
        });
    }
}

/// A change feed: each `next()` is the next change, or `null` once the feed has ended.
#[wasm_bindgen]
pub struct Changes {
    events: Rc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Result<String, String>>>>,
    reader: RefCell<Option<AbortOnDropHandle<()>>>,
}

impl Changes {
    fn follow(mut response: http::Response) -> Self {
        let (send, events) = tokio::sync::mpsc::unbounded_channel();
        let reader = n0_future::task::spawn(async move {
            let mut frames = http::Events::default();
            loop {
                while let Some(data) = frames.next_change() {
                    if send.send(Ok(data)).is_err() {
                        return;
                    }
                }
                match response.chunk().await {
                    Ok(Some(bytes)) => frames.push(&bytes),
                    Ok(None) => return,
                    Err(why) => {
                        let _ = send.send(Err(why));
                        return;
                    }
                }
            }
        });
        Changes {
            events: Rc::new(tokio::sync::Mutex::new(events)),
            reader: RefCell::new(Some(AbortOnDropHandle::new(reader))),
        }
    }
}

#[wasm_bindgen]
impl Changes {
    /// The next change row, as ErisDB sent it; `null` once the feed has ended, as it does when
    /// ErisDB stops letting the app in. Throws `unreachable` when the stream broke.
    pub fn next(&self) -> Promise {
        let events = self.events.clone();
        future_to_promise(async move {
            match events.lock().await.recv().await {
                Some(Ok(data)) => js_sys::JSON::parse(&data),
                Some(Err(why)) => Err(error("unreachable", why)),
                None => Ok(JsValue::NULL),
            }
        })
    }

    /// Stop following: the stream closes, and a waiting `next()` resolves to `null`.
    pub fn close(&self) {
        self.reader.borrow_mut().take();
    }
}
