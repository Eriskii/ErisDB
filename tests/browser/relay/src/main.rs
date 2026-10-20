//! An Iroh relay for the browser suite, which browsers need because they reach nothing but
//! relays: plain HTTP on a free loopback port. It prints its URL, the value every participant
//! takes as `ERIS_NETWORK`, and relays until it is stopped.

use std::net::Ipv4Addr;

use iroh_relay::server::{RelayConfig, Server, ServerConfig};

#[tokio::main]
async fn main() {
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let server = Server::spawn(config).await.expect("the relay starts");
    let addr = server.http_addr().expect("the relay serves plain HTTP");
    println!("http://{addr}");
    let _ = tokio::signal::ctrl_c().await;
    let _ = server.shutdown().await;
}
