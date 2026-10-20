//! A stand-in for ErisAuth, for testing the local launcher: one device, in a
//! mesh of its own on loopback endpoints, answering the agent socket at
//! `$XDG_RUNTIME_DIR/erisauth/agent.sock`. It approves whatever it is asked,
//! as ErisAuth does once the person says yes, and approving an app to serve
//! `erisdb` revokes the ErisDB it approved before, so the mesh has one. It
//! asks no one, so it belongs in tests and nowhere else.
//!
//! ```sh
//! XDG_RUNTIME_DIR=/tmp/somewhere cargo run --example stand_in_erisauth
//! ```

use std::sync::Arc;

use anyhow::Context;
use erisoverlaynetwork::agent::{self, Reply, Request};
use erisoverlaynetwork::devices::Op;
use erisoverlaynetwork::mesh::{self, Config, Local, MemoryStore, Node};
use erisoverlaynetwork::net::{self, Network};
use erisoverlaynetwork::NoteId;
use iroh::protocol::Router;
use iroh::SecretKey;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket =
        agent::socket_path().context("XDG_RUNTIME_DIR names where the agent socket goes")?;
    std::fs::create_dir_all(socket.parent().expect("the socket is in a directory"))?;
    let _ = std::fs::remove_file(&socket);

    let key = SecretKey::generate();
    let endpoint = net::endpoint(key.clone(), &Network::Loopback).await?;
    let node = Node::start(Config {
        endpoint: endpoint.clone(),
        network: Network::Loopback,
        local: Local::Device(key),
        store: Arc::new(MemoryStore::default()),
    })
    .await?;
    node.create("stand-in").await?;
    let _router = Router::builder(endpoint)
        .accept(mesh::ALPN, node.protocol())
        .spawn();

    let listener = tokio::net::UnixListener::bind(&socket)?;
    println!("listening at {}", socket.display());
    // The ErisDB approved last; approving another revokes it.
    let erisdb: Arc<tokio::sync::Mutex<Option<NoteId>>> = Arc::default();
    loop {
        let (stream, _) = listener.accept().await?;
        let (node, erisdb) = (node.clone(), erisdb.clone());
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let reply = match serde_json::from_str::<Request>(&line) {
                Ok(Request::Approve { ask, .. }) => approve(&node, &erisdb, ask).await,
                Ok(Request::Snapshot { .. }) => match node.snapshot().await {
                    Some(snapshot) => Reply::Snapshot(snapshot),
                    None => Reply::Error("in no mesh".into()),
                },
                Err(e) => Reply::Error(e.to_string()),
            };
            let mut out = serde_json::to_vec(&reply).expect("a reply serializes");
            out.push(b'\n');
            let _ = write.write_all(&out).await;
        });
    }
}

async fn approve(
    node: &Node,
    erisdb: &tokio::sync::Mutex<Option<NoteId>>,
    ask: agent::Ask,
) -> Reply {
    let note = match node.issue(
        ask.app,
        &ask.name,
        &ask.service,
        ask.grants,
        ask.serves,
        None,
    ) {
        Ok(note) => note,
        Err(e) => return Reply::Error(e.to_string()),
    };
    if ask.serves && ask.service == erisdb::SERVICE {
        let mut last = erisdb.lock().await;
        if let Some(old) = last.replace(note.id).filter(|old| *old != note.id) {
            if let Err(e) = node.append(Op::Revoke { note: old }).await {
                return Reply::Error(e.to_string());
            }
        }
    }
    match node.snapshot().await {
        Some(snapshot) => Reply::Approved { note, snapshot },
        None => Reply::Error("in no mesh".into()),
    }
}
