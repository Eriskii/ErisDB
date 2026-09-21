//! Exercise user-visible argument validation through the installed CLI entrypoint.

use std::time::Duration;

async fn pair(args: &[&str]) -> std::process::Output {
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_erisdb"))
        .env_clear()
        .args(["pair", "--secret", "cli-test-secret", "--no-iroh"])
        .args(args)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("CLI exits promptly")
        .unwrap()
}

#[tokio::test]
async fn pairing_rejects_all_loopback_hosts_before_contacting_a_core() {
    for url in [
        "http://localhost:1",
        "http://127.0.0.1:1",
        "http://127.2.3.4:1",
        "http://[::1]:1",
    ] {
        let output = pair(&["--url", url]).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("pass --client-url"), "{url}: {stderr}");
        assert!(!stderr.contains("reaching a core"), "{url}: {stderr}");
    }
}

#[tokio::test]
async fn maximum_pairing_ttl_and_explicit_client_url_reach_the_core_without_panicking() {
    // Reserve a port, then close it so the real connection is refused promptly.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = pair(&[
        "--url",
        &url,
        "--client-url",
        "http://192.0.2.1:7700",
        "--ttl",
        "9223372036854775807",
    ])
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("reaching a core"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[tokio::test]
async fn endpoint_id_command_names_the_live_http_endpoint_and_honors_seed_override() {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;

    async fn endpoint_id(args: &[&str]) -> iroh::EndpointId {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_erisdb"))
            .env_clear()
            .arg("endpoint-id")
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    let default = endpoint_id(&["--secret", "cli-token-secret"]).await;
    assert_eq!(
        default,
        endpoint_id(&["--secret", "cli-token-secret"]).await
    );
    assert_ne!(
        default,
        endpoint_id(&["--secret", "another-token-secret"]).await
    );
    let overridden = endpoint_id(&[
        "--secret",
        "cli-token-secret",
        "--iroh-secret",
        "cli-endpoint-secret",
    ])
    .await;
    assert_ne!(default, overridden);
    assert_eq!(
        overridden,
        endpoint_id(&[
            "--secret",
            "rotated-token-secret",
            "--iroh-secret",
            "cli-endpoint-secret",
        ])
        .await
    );

    let client = erisdb::net::endpoint(b"cli-test-client").await.unwrap();
    for (id, seed) in [
        (default, "cli-token-secret"),
        (overridden, "cli-endpoint-secret"),
    ] {
        let server = erisdb::net::endpoint(seed.as_bytes()).await.unwrap();
        // Use the CLI's identity, with only the socket addresses from the
        // running server: a wrong identity cannot complete the QUIC handshake.
        let address = iroh::EndpointAddr::from_parts(id, server.addr().addrs);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres:postgres@127.0.0.1:1/erisdb")
            .unwrap();
        let app = erisdb::app(pool, b"cli-token-secret".to_vec());
        let serving = tokio::spawn(erisdb::net::serve(server.clone(), app));
        tokio::time::timeout(Duration::from_secs(5), async {
            let connection = client.connect(address, erisdb::net::ALPN).await.unwrap();
            let (send, recv) = connection.open_bi().await.unwrap();
            let (mut sender, driver) =
                hyper::client::conn::http1::handshake(TokioIo::new(tokio::io::join(recv, send)))
                    .await
                    .unwrap();
            let driver = tokio::spawn(driver);
            let response = sender
                .send_request(
                    hyper::Request::get("/v1/health")
                        .header("host", "erisdb")
                        .body(Full::<Bytes>::default())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["ok"], true);
            driver.abort();
        })
        .await
        .expect("the CLI identity must connect to the running core");
        server.close().await;
        serving.await.unwrap().unwrap();
    }
    client.close().await;
}
