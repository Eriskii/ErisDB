//! The bezel-client contract, proven against a real core: real Postgres
//! (Docker via testcontainers), a real bezel serving over real Iroh QUIC,
//! and this client dialing it. No mocks.

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

const SECRET: &[u8] = b"client-e2e-secret";

/// A migrated store, a core serving over Iroh, and the addr to dial.
async fn spawn_core() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    iroh::EndpointAddr,
) {
    let container = Postgres::default().start().await.expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("pg port");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"))
        .await
        .expect("db connect");
    bezel::MIGRATOR.run(&pool).await.expect("migrate");
    let app = bezel::app(pool, SECRET.to_vec());
    let ep = bezel::net::endpoint(SECRET).await.expect("endpoint");
    let addr = bezel::net::advertised_addr(&ep).await.expect("addr");
    tokio::spawn(bezel::net::serve(ep, app));
    (container, addr)
}

#[tokio::test]
async fn the_client_speaks_bezel_over_iroh() {
    let (_pg, addr) = spawn_core().await;
    let token =
        bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), Some("droid"))
            .unwrap();

    // A deterministic client identity: the same secret dials as the same
    // endpoint id every time, so source.addr is stable per device.
    let identity = [7u8; 32];
    let client = bezel_client::Client::dial_addr(addr, &token, "Lists (Android) v0.1", Some(identity))
        .await
        .expect("dial");

    // Health, unauthenticated path shape.
    let (status, body) = client.request("GET", "/v1/health", None).await.expect("health");
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);

    // Register the lists facet and write through the client.
    let (status, _) = client
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {
                "name": "lists/v1",
                "schema": {"type": "object", "required": ["list", "name"]}
            }})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201);
    let (status, item) = client
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "lists/v1", "body": {"list": "books", "name": "Piranesi"}})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");
    // The full source pipeline holds over QUIC: signed user, claimed
    // client, and the DERIVED device identity as the observed addr.
    assert_eq!(item["source"]["user"], "droid");
    assert_eq!(item["source"]["client"], "Lists (Android) v0.1");
    let device_id = iroh::SecretKey::from_bytes(&identity).public().to_string();
    assert_eq!(item["source"]["addr"], format!("iroh:{device_id}"));

    // Reads round-trip.
    let (status, listed) = client
        .request("GET", "/v1/items?facet=lists/v1", None)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    // Several requests share one connection (one bi-stream each); the
    // client survives sequential use without redialing.
    for _ in 0..3 {
        let (status, _) = client.request("GET", "/v1/health", None).await.unwrap();
        assert_eq!(status, 200);
    }
}

// Multi-threaded: the test thread blocks in join() while the in-process
// core keeps serving on the other workers.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_works_from_sync_code() {
    let (_pg, addr) = spawn_core().await;
    let token = bezel::auth::mint(SECRET, &["system"], &["read"], Some(3600), None).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    // The facade owns its runtime: callable from a plain thread, exactly
    // like a JNI entry point.
    let handle = std::thread::spawn(move || {
        bezel_client::blocking::configure(&addr_json, &token, "Test (Blocking) v0", &[9u8; 32])
            .expect("configure");
        bezel_client::blocking::request("GET", "/v1/health", None)
    });
    let response = handle.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["status"], 200, "{response}");
    assert_eq!(v["body"]["ok"], true);

    // Errors come back as data, never panics across the FFI boundary.
    // (Still on a plain thread: the facade is for executor-less callers.)
    let bad = std::thread::spawn(|| bezel_client::blocking::request("GET", "/v1/items?facet=lists/v1", None))
        .join()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&bad).unwrap();
    assert_eq!(v["status"], 403); // token scoped to `system` cannot read lists/v1
}
