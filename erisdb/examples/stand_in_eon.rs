//! A stand-in for eon, for testing the local launcher: one device, in a mesh
//! of its own on loopback endpoints, answering the agent socket at
//! `$XDG_RUNTIME_DIR/eon/agent.sock`. It approves whatever it is asked, as
//! eon does once the person says yes, and approving an app to serve a
//! capability a mesh has a single server of, such as `database/1`, revokes
//! the server of it it approved before, so the mesh has one. It asks no one,
//! so it belongs in tests and nowhere else.
//!
//! ```sh
//! XDG_RUNTIME_DIR=/tmp/somewhere cargo run --example stand_in_eon
//! ```

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Context;
use erisoverlaynetwork::agent::{self, Reply, Request};
use erisoverlaynetwork::devices::Op;
use erisoverlaynetwork::mesh::{self, Config, Local, MemoryStore, Node};
use erisoverlaynetwork::net::{self, Network};
use erisoverlaynetwork::{Capability, NoteId};
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
    // The server of each single capability approved last; approving another revokes it.
    let singles: Arc<tokio::sync::Mutex<HashMap<Capability, NoteId>>> = Arc::default();
    loop {
        let (stream, _) = listener.accept().await?;
        let (node, singles) = (node.clone(), singles.clone());
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let reply = match serde_json::from_str::<Request>(&line) {
                Ok(Request::Approve { ask, .. }) => approve(&node, &singles, ask).await,
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
    singles: &tokio::sync::Mutex<HashMap<Capability, NoteId>>,
    ask: agent::Ask,
) -> Reply {
    let single: Vec<Capability> = ask
        .serves
        .iter()
        .filter(|s| s.single)
        .map(|s| s.capability.clone())
        .collect();
    let note = match node.issue(ask.app, &ask.name, ask.uses, ask.serves, None) {
        Ok(note) => note,
        Err(e) => return Reply::Error(e.to_string()),
    };
    let mut last = singles.lock().await;
    let replaced: BTreeSet<NoteId> = single
        .into_iter()
        .filter_map(|capability| last.insert(capability, note.id))
        .filter(|old| *old != note.id)
        .collect();
    for old in replaced {
        if let Err(e) = node.append(Op::Revoke { note: old }).await {
            return Reply::Error(e.to_string());
        }
    }
    drop(last);
    match node.snapshot().await {
        Some(snapshot) => Reply::Approved { note, snapshot },
        None => Reply::Error("in no mesh".into()),
    }
}
