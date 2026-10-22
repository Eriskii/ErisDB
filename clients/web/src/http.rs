//! HTTP/1.1 on one QUIC bi-stream: the exchange the database serves under ALPN `eris/database/1`.
//!
//! Each request opens a stream of its own and speaks one HTTP/1.1 exchange on it with hyper's
//! client connection, driven as a task of the page. The request carries no token: ErisDB knows
//! the app by the key the QUIC handshake proved.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use n0_future::task::AbortOnDropHandle;
use serde_json::Value;

/// One bi-stream as hyper's I/O.
struct Stream {
    recv: RecvStream,
    send: SendStream,
}

impl hyper::rt::Read for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut chunk = [0u8; 16 * 1024];
        let room = buf.remaining().min(chunk.len());
        let mut read = tokio::io::ReadBuf::new(&mut chunk[..room]);
        ready!(tokio::io::AsyncRead::poll_read(Pin::new(&mut self.get_mut().recv), cx, &mut read))?;
        buf.put_slice(read.filled());
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Write for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(Pin::new(&mut self.get_mut().send), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(Pin::new(&mut self.get_mut().send), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(Pin::new(&mut self.get_mut().send), cx)
    }
}

/// How far an exchange got before it failed, which decides whether repeating it is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Nothing of the request left the page: ErisDB has not seen it.
    BeforeSend,
    /// The request went out; ErisDB may have carried it out and lost only the answer.
    AfterSend,
}

#[derive(Debug)]
pub struct Failed {
    pub stage: Stage,
    pub message: String,
}

fn before(e: impl std::fmt::Display) -> Failed {
    Failed { stage: Stage::BeforeSend, message: e.to_string() }
}

fn after(e: impl std::fmt::Display) -> Failed {
    Failed { stage: Stage::AfterSend, message: e.to_string() }
}

/// A response whose body is still arriving. Dropping it closes the stream.
pub struct Response {
    pub status: u16,
    body: Incoming,
    _driver: AbortOnDropHandle<()>,
    _sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
}

impl Response {
    /// The next piece of the body, or `None` once it has ended.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, String> {
        loop {
            match self.body.frame().await {
                None => return Ok(None),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }

    /// The whole body.
    pub async fn bytes(mut self) -> Result<Bytes, Failed> {
        let collected = (&mut self.body).collect().await.map_err(after)?;
        Ok(collected.to_bytes())
    }
}

/// Send one request on a stream of its own and read the response's head.
pub async fn open(
    conn: &Connection,
    client: &str,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<Response, Failed> {
    // Up to `send_request`, everything is local: opening a QUIC stream sends nothing until the
    // first byte, and hyper's handshake writes nothing.
    let (send, recv) = conn.open_bi().await.map_err(before)?;
    let (mut sender, driver) = hyper::client::conn::http1::handshake(Stream { recv, send }).await.map_err(before)?;
    let driver = AbortOnDropHandle::new(n0_future::task::spawn(async move {
        let _ = driver.await;
    }));
    let mut request = hyper::Request::builder()
        .method(hyper::Method::from_bytes(method.as_bytes()).map_err(before)?)
        .uri(path)
        .header("host", "erisdb")
        .header("accept", "application/json, text/event-stream")
        .header("x-erisdb-client", client);
    let payload = match body {
        Some(bytes) => {
            request = request.header("content-type", "application/json");
            Bytes::from(bytes)
        }
        None => Bytes::new(),
    };
    let request = request.body(Full::new(payload)).map_err(before)?;
    let response = sender.send_request(request).await.map_err(after)?;
    Ok(Response { status: response.status().as_u16(), body: response.into_body(), _driver: driver, _sender: sender })
}

/// A response body as a value: JSON where it is JSON, the text where it is not, null where
/// there is none.
pub fn parse_body(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

/// Whether asking twice is the same as asking once: only for methods that change nothing. A
/// repeated `PUT` or `DELETE` meets ErisDB's revision checks and answers 409 or 404 instead of
/// the truth.
pub fn nullipotent(method: &str) -> bool {
    ["GET", "HEAD", "OPTIONS"].iter().any(|m| method.eq_ignore_ascii_case(m))
}

/// Server-sent events, read frame by frame.
#[derive(Default)]
pub struct Events {
    buf: Vec<u8>,
}

impl Events {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The data of the next whole `change` event in what has arrived; keep-alive comments and
    /// other events are passed over.
    pub fn next_change(&mut self) -> Option<String> {
        while let Some((text_end, next)) = frame_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..next).collect();
            let frame = String::from_utf8_lossy(&frame[..text_end]).into_owned();
            let (mut event, mut data) = (String::new(), String::new());
            for line in frame.lines() {
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let (field, value) = match line.split_once(':') {
                    Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                    None => (line, ""),
                };
                match field {
                    "event" => event = value.to_string(),
                    "data" => {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(value);
                    }
                    _ => {}
                }
            }
            if event == "change" && !data.is_empty() {
                return Some(data);
            }
        }
        None
    }
}

/// Where a frame's text ends and the next frame begins: a blank line, in either newline style.
fn frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).find_map(|i| {
        if buf[i..].starts_with(b"\n\n") {
            Some((i + 1, i + 2))
        } else if buf[i..].starts_with(b"\r\n\r\n") {
            Some((i + 2, i + 4))
        } else {
            None
        }
    })
}
