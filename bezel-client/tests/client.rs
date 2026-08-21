//! The bezel-client contract, proven against a real core: real Postgres
//! (Docker via testcontainers), a real bezel serving over real Iroh QUIC,
//! and this client dialing it. No mocks.

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

const SECRET: &[u8] = b"client-e2e-secret";

/// The blocking facade is process-wide by design — one runtime, one
/// client slot — so the tests that drive it take turns.
fn facade() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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

#[tokio::test]
async fn the_client_refreshes_its_capability() {
    let (_pg, addr) = spawn_core().await;
    // A short-lived token, the shape a device holds between refreshes.
    let old = bezel::auth::mint(SECRET, &["*"], &["read", "write"], Some(30), Some("droid")).unwrap();

    let client = bezel_client::Client::dial_addr(addr.clone(), &old, "Refresher v0", Some([8u8; 32]))
        .await
        .expect("dial");

    let fresh = client.refresh_capability(3600).await.expect("refresh");
    assert_ne!(fresh, old);

    // The client swapped its own token: later requests carry the fresh
    // one, with the scope and signed user intact.
    let (status, item) = client
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {"name": "refreshed/v1", "schema": {}}})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["source"]["user"], "droid");

    // The returned string is the whole story: a second client built from
    // it alone talks to the same core.
    let other = bezel_client::Client::dial_addr(addr.clone(), &fresh, "Fresh v0", Some([10u8; 32]))
        .await
        .expect("dial with fresh token");
    let (status, body) = other.request("GET", "/v1/items?facet=facet", None).await.unwrap();
    assert_eq!(status, 200, "{body}");

    // An expired token cannot refresh: refresh keeps a session alive, it
    // does not resurrect one. The failure is an error, and the client
    // keeps the token it had.
    let dead = bezel::auth::mint(SECRET, &["*"], &["read"], Some(-10), None).unwrap();
    let stale = bezel_client::Client::dial_addr(addr, &dead, "Stale v0", Some([11u8; 32]))
        .await
        .expect("dial");
    assert!(stale.refresh_capability(3600).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_refreshes_its_capability() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let old = bezel::auth::mint(SECRET, &["*"], &["read"], Some(30), Some("droid")).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();
    let old_for_thread = old.clone();

    let response = std::thread::spawn(move || {
        bezel_client::blocking::configure(
            &addr_json,
            &old_for_thread,
            "Test (Blocking) v0",
            &[12u8; 32],
        )
        .expect("configure");
        bezel_client::blocking::refresh_capability(3600)
    })
    .join()
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["ok"], true, "{response}");
    let fresh = v["token"].as_str().unwrap().to_string();
    assert_ne!(fresh, old);

    // The process-wide client now speaks with the fresh token.
    let after = std::thread::spawn(|| bezel_client::blocking::request("GET", "/v1/health", None))
        .join()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["status"], 200, "{after}");
}

// Multi-threaded: the subscription holds one bi-stream open while a
// second connection writes through the same core.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_streams_the_change_feed() {
    let (_pg, addr) = spawn_core().await;
    let token =
        bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), Some("droid"))
            .unwrap();

    let reader = bezel_client::Client::dial_addr(addr.clone(), &token, "Reader v0", Some([3u8; 32]))
        .await
        .expect("dial reader");
    let writer = bezel_client::Client::dial_addr(addr, &token, "Writer v0", Some([4u8; 32]))
        .await
        .expect("dial writer");

    let (status, _) = writer
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {
                "name": "notes/v1",
                "schema": {"type": "object", "required": ["text"]}
            }})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201);

    // The caller owns the cursor: start from where the feed is now.
    let (status, feed) = reader.request("GET", "/v1/changes?since=0", None).await.unwrap();
    assert_eq!(status, 200);
    let since = feed["next"].as_i64().unwrap();
    assert!(since > 0);

    let mut sub =
        reader.subscribe_changes(since, Some("notes/v1")).await.expect("subscribe");

    // A write through the other connection lands on the open stream.
    let (status, item) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "notes/v1", "body": {"text": "hi"}})))
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");

    let event = tokio::time::timeout(std::time::Duration::from_secs(10), sub.next())
        .await
        .expect("event arrives")
        .expect("stream alive")
        .expect("not end of stream");
    assert!(event.seq > since);
    assert_eq!(event.facet, "notes/v1");
    assert_eq!(event.op, "created");
    assert_eq!(event.body.as_ref().unwrap()["text"], "hi");
    assert_eq!(event.item_id.as_deref(), item["id"].as_str());
    assert_eq!(event.raw["seq"], event.seq);

    // A second write continues on the same stream, in order.
    let (_, item2) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "notes/v1", "body": {"text": "again"}})))
        .await
        .unwrap();
    let next = tokio::time::timeout(std::time::Duration::from_secs(10), sub.next())
        .await
        .expect("second event arrives")
        .expect("stream alive")
        .expect("not end of stream");
    assert!(next.seq > event.seq);
    assert_eq!(next.item_id.as_deref(), item2["id"].as_str());

    // The facet filter holds: a write elsewhere is not delivered here.
    let (status, _) = writer
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {"name": "other/v1", "schema": {}}})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201);
    let (_, item3) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "notes/v1", "body": {"text": "third"}})))
        .await
        .unwrap();
    let third = tokio::time::timeout(std::time::Duration::from_secs(10), sub.next())
        .await
        .expect("third event arrives")
        .expect("stream alive")
        .expect("not end of stream");
    assert_eq!(third.item_id.as_deref(), item3["id"].as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_subscription_pulls_changes() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let token =
        bezel::auth::mint(SECRET, &["*"], &["read", "write", "admin"], Some(3600), Some("droid"))
            .unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    let writer = bezel_client::Client::dial_addr(addr, &token, "Writer v0", Some([5u8; 32]))
        .await
        .expect("dial writer");
    writer
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {
                "name": "memo/v1",
                "schema": {"type": "object", "required": ["text"]}
            }})),
        )
        .await
        .unwrap();

    let sub = std::thread::spawn(move || {
        bezel_client::blocking::configure(&addr_json, &token, "Test (Blocking) v0", &[6u8; 32])
            .expect("configure");
        bezel_client::blocking::subscribe_changes(0, Some("memo/v1")).expect("subscribe")
    })
    .join()
    .unwrap();

    // Nothing on the feed yet for this facet: the pull times out as data.
    let idle = std::thread::spawn(move || bezel_client::blocking::next_change(sub, 500))
        .join()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&idle).unwrap();
    assert_eq!(v["ok"], true, "{idle}");
    assert!(v.get("change").is_none(), "{idle}");

    let (_, item) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "memo/v1", "body": {"text": "yo"}})))
        .await
        .unwrap();

    let got = std::thread::spawn(move || bezel_client::blocking::next_change(sub, 10_000))
        .join()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(v["ok"], true, "{got}");
    assert_eq!(v["change"]["item_id"], item["id"], "{got}");
    assert_eq!(v["change"]["facet"], "memo/v1");
    assert!(v["change"]["seq"].as_i64().unwrap() > 0);

    bezel_client::blocking::close_subscription(sub);
    // A closed handle answers as data, not a panic.
    let after = bezel_client::blocking::next_change(sub, 100);
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["ok"], false, "{after}");
}

// Multi-threaded: the test thread blocks in join() while the in-process
// core keeps serving on the other workers.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_works_from_sync_code() {
    let _facade = facade();
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
