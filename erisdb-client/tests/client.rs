//! The erisdb-client interface, proven against a real core: real Postgres
//! (Docker via testcontainers), a real erisdb serving over real Iroh QUIC,
//! and this client dialing it. No mocks.

// The facade lock is held across awaits on purpose: it serializes whole
// tests against a process-wide client, which is exactly the span it has
// to cover.
#![allow(clippy::await_holding_lock)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use erisdb_client::{Cancel, Client, Pairing};
use serde_json::{json, Value};
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
    erisdb::MIGRATOR.run(&pool).await.expect("migrate");
    let app = erisdb::app(pool, SECRET.to_vec());
    let ep = erisdb::net::endpoint(SECRET).await.expect("endpoint");
    let addr = erisdb::net::advertised_addr(&ep).await.expect("addr");
    tokio::spawn(erisdb::net::serve(ep, app));
    (container, addr)
}

#[tokio::test]
async fn the_client_speaks_erisdb_over_iroh() {
    let (_pg, addr) = spawn_core().await;
    let token = erisdb::auth::mint(SECRET, &["*"], Some(3600), Some("droid")).unwrap();

    // A deterministic client identity: the same secret dials as the same
    // endpoint id every time, so source.addr is stable per device.
    let identity = [7u8; 32];
    let client = Client::dial_addr(addr, &token, "Lists (Android) v0.1", Some(identity))
        .await
        .expect("dial");

    // Health, unauthenticated path shape.
    let (status, body) = client.request("GET", "/v1/health", None).await.expect("health");
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);

    // Register the lists facet and write through the client. The facet's
    // name is its permission namespace; the schema version rides in the
    // body, so `lists:read` survives a v2.
    let (status, _) = client
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {
                "name": "lists",
                "version": 1,
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
            Some(json!({"facet": "lists", "body": {"list": "books", "name": "Piranesi"}})),
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
    let (status, listed) = client.request("GET", "/v1/items?facet=lists", None).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    // Several requests share one connection (one bi-stream each); the
    // client survives sequential use without redialing.
    for _ in 0..3 {
        let (status, _) = client.request("GET", "/v1/health", None).await.unwrap();
        assert_eq!(status, 200);
    }
}

/// A token can always be asked what it holds — no permission required,
/// since the answer is already inside the token the caller is carrying.
#[tokio::test]
async fn the_client_reads_its_own_permissions() {
    let (_pg, addr) = spawn_core().await;
    let token =
        erisdb::auth::mint(SECRET, &["tasks:read", "lists:*"], Some(3600), Some("droid")).unwrap();
    let client = Client::dial_addr(addr, &token, "Curious v0", Some([16u8; 32])).await.expect("dial");

    let held = client.permissions().await.expect("permissions");
    assert_eq!(held.grants, ["tasks:read", "lists:*"]);
    assert_eq!(held.user.as_deref(), Some("droid"));
    assert!(held.exp.is_some(), "an expiring token says when");
}

#[tokio::test]
async fn the_client_refreshes_its_capability() {
    let (_pg, addr) = spawn_core().await;
    // A short-lived token, the shape a device holds between refreshes.
    let old = erisdb::auth::mint(SECRET, &["*"], Some(30), Some("droid")).unwrap();

    let client = Client::dial_addr(addr.clone(), &old, "Refresher v0", Some([8u8; 32]))
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
            Some(json!({"facet": "facet", "body": {"name": "refreshed", "version": 1, "schema": {}}})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["source"]["user"], "droid");

    // The returned string is the whole story: a second client built from
    // it alone talks to the same core.
    let other = Client::dial_addr(addr.clone(), &fresh, "Fresh v0", Some([10u8; 32]))
        .await
        .expect("dial with fresh token");
    let (status, body) = other.request("GET", "/v1/items?facet=facet", None).await.unwrap();
    assert_eq!(status, 200, "{body}");

    // An expired token cannot refresh: refresh keeps a session alive, it
    // does not resurrect one. The failure is an error, and the client
    // keeps the token it had.
    let dead = erisdb::auth::mint(SECRET, &["*"], Some(-10), None).unwrap();
    let stale = Client::dial_addr(addr, &dead, "Stale v0", Some([11u8; 32])).await.expect("dial");
    let refused = stale.refresh_capability(3600).await.unwrap_err();

    // The refusal carries its status as a number, so callers branch on
    // 401 instead of matching prose.
    let refused = refused.downcast_ref::<erisdb_client::Refused>().expect("a refusal");
    assert_eq!(refused.status, 401);
}

/// The status survives the FFI boundary as a field of the envelope: the
/// blocking facade hands Kotlin a number to branch on, not a sentence to
/// substring-match.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_reports_a_refusals_status() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let dead = erisdb::auth::mint(SECRET, &["*"], Some(-10), None).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    let response = std::thread::spawn(move || {
        erisdb_client::blocking::configure(&addr_json, &dead, "Stale (Blocking) v0", &[13u8; 32])
            .expect("configure");
        erisdb_client::blocking::refresh_capability(3600)
    })
    .join()
    .unwrap();
    let v: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["ok"], false, "{response}");
    assert_eq!(v["status"], 401, "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_refreshes_its_capability() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let old = erisdb::auth::mint(SECRET, &["*"], Some(30), Some("droid")).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();
    let old_for_thread = old.clone();

    let response = std::thread::spawn(move || {
        erisdb_client::blocking::configure(
            &addr_json,
            &old_for_thread,
            "Test (Blocking) v0",
            &[12u8; 32],
        )
        .expect("configure");
        erisdb_client::blocking::refresh_capability(3600)
    })
    .join()
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["ok"], true, "{response}");
    let fresh = v["token"].as_str().unwrap().to_string();
    assert_ne!(fresh, old);

    // The process-wide client now speaks with the fresh token.
    let after = std::thread::spawn(|| erisdb_client::blocking::request("GET", "/v1/health", None))
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
    let token = erisdb::auth::mint(SECRET, &["*"], Some(3600), Some("droid")).unwrap();

    let reader = Client::dial_addr(addr.clone(), &token, "Reader v0", Some([3u8; 32]))
        .await
        .expect("dial reader");
    let writer =
        Client::dial_addr(addr, &token, "Writer v0", Some([4u8; 32])).await.expect("dial writer");

    let (status, _) = writer
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {
                "name": "notes",
                "version": 1,
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

    let mut sub = reader.subscribe_changes(since, Some("notes")).await.expect("subscribe");

    // A write through the other connection lands on the open stream.
    let (status, item) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "notes", "body": {"text": "hi"}})))
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");

    let event = tokio::time::timeout(std::time::Duration::from_secs(10), sub.next())
        .await
        .expect("event arrives")
        .expect("stream alive")
        .expect("not end of stream");
    assert!(event.seq > since);
    assert_eq!(event.facet, "notes");
    assert_eq!(event.op, "created");
    assert_eq!(event.body.as_ref().unwrap()["text"], "hi");
    assert_eq!(event.item_id.as_deref(), item["id"].as_str());
    assert_eq!(event.raw["seq"], event.seq);

    // A second write continues on the same stream, in order.
    let (_, item2) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "notes", "body": {"text": "again"}})))
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
            Some(json!({"facet": "facet", "body": {"name": "other", "version": 1, "schema": {}}})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201);
    let (_, item3) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "notes", "body": {"text": "third"}})))
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
    let token = erisdb::auth::mint(SECRET, &["*"], Some(3600), Some("droid")).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    let writer =
        Client::dial_addr(addr, &token, "Writer v0", Some([5u8; 32])).await.expect("dial writer");
    writer
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {
                "name": "memo",
                "version": 1,
                "schema": {"type": "object", "required": ["text"]}
            }})),
        )
        .await
        .unwrap();

    let sub = std::thread::spawn(move || {
        erisdb_client::blocking::configure(&addr_json, &token, "Test (Blocking) v0", &[6u8; 32])
            .expect("configure");
        erisdb_client::blocking::subscribe_changes(0, Some("memo")).expect("subscribe")
    })
    .join()
    .unwrap();

    // Nothing on the feed yet for this facet: the pull times out as data.
    let idle =
        std::thread::spawn(move || erisdb_client::blocking::next_change(sub, 500)).join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&idle).unwrap();
    assert_eq!(v["ok"], true, "{idle}");
    assert!(v.get("change").is_none(), "{idle}");

    let (_, item) = writer
        .request("POST", "/v1/items", Some(json!({"facet": "memo", "body": {"text": "yo"}})))
        .await
        .unwrap();

    let got = std::thread::spawn(move || erisdb_client::blocking::next_change(sub, 10_000))
        .join()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(v["ok"], true, "{got}");
    assert_eq!(v["change"]["item_id"], item["id"], "{got}");
    assert_eq!(v["change"]["facet"], "memo");
    assert!(v["change"]["seq"].as_i64().unwrap() > 0);

    erisdb_client::blocking::close_subscription(sub);
    // A closed handle answers as data, not a panic.
    let after = erisdb_client::blocking::next_change(sub, 100);
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["ok"], false, "{after}");
}

// Multi-threaded: the test thread blocks in join() while the in-process
// core keeps serving on the other workers.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_works_from_sync_code() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let token = erisdb::auth::mint(SECRET, &["meta:system:read"], Some(3600), None).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    // The facade owns its runtime: callable from a plain thread, exactly
    // like a JNI entry point.
    let handle = std::thread::spawn(move || {
        erisdb_client::blocking::configure(&addr_json, &token, "Test (Blocking) v0", &[9u8; 32])
            .expect("configure");
        erisdb_client::blocking::request("GET", "/v1/health", None)
    });
    let response = handle.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["status"], 200, "{response}");
    assert_eq!(v["body"]["ok"], true);

    // Errors come back as data, never panics across the FFI boundary.
    // (Still on a plain thread: the facade is for executor-less callers.)
    let bad =
        std::thread::spawn(|| erisdb_client::blocking::request("GET", "/v1/items?facet=lists", None))
            .join()
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&bad).unwrap();
    assert_eq!(v["status"], 403); // meta:system:read cannot read lists
}

// ---------------------------------------------------------------------
// Pairing: the code grants nothing, and a human decides what does.
// ---------------------------------------------------------------------

/// A ticket for this core, exactly as `erisdb pair` cuts one.
fn ticket_for(code: &str) -> String {
    erisdb::ticket::Ticket::new(
        code.to_string(),
        Some(erisdb::net::endpoint_id(SECRET).to_string()),
        None,
        Some("my-laptop".into()),
    )
    .expect("a ticket")
    .encode()
    .expect("encode")
}

/// The operator's side: cut a pairing code over the ordinary API.
async fn cut_a_pairing(op: &Client) -> (String, String) {
    let (status, cut) =
        op.request("POST", "/v1/pairings", Some(json!({"ttl_secs": 600}))).await.expect("cut");
    assert_eq!(status, 201, "{cut}");
    (
        cut["id"].as_str().expect("an id").to_string(),
        cut["secret"].as_str().expect("a code").to_string(),
    )
}

/// The human's side: wait for the request to appear, then answer it.
/// `granted` of `None` denies.
async fn answer(op: &Client, id: &str, granted: Option<&[&str]>) {
    for _ in 0..200 {
        let (_, session) = op.request("GET", &format!("/v1/pairings/{id}"), None).await.unwrap();
        if session["body"]["status"] == "requested" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (path, body) = match granted {
        Some(g) => {
            (format!("/v1/pairings/{id}/approve"), json!({"granted": g, "ttl_secs": 3600}))
        }
        None => (format!("/v1/pairings/{id}/deny"), json!({})),
    };
    let (status, answered) = op.request("POST", &path, Some(body)).await.unwrap();
    assert_eq!(status, 200, "{answered}");
}

/// A core, an operator holding everything, and the `tasks` facet the app
/// is about to ask for.
async fn a_core_to_pair_with() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    iroh::EndpointAddr,
    Client,
) {
    let (pg, addr) = spawn_core().await;
    let operator = erisdb::auth::mint(SECRET, &["*"], Some(3600), Some("me")).unwrap();
    let op = Client::dial_addr(addr.clone(), &operator, "Dashboard v0", Some([30u8; 32]))
        .await
        .expect("dial operator");
    let (status, body) = op
        .request(
            "POST",
            "/v1/items",
            Some(json!({"facet": "facet", "body": {"name": "tasks", "version": 1, "schema": {}}})),
        )
        .await
        .unwrap();
    assert_eq!(status, 201, "{body}");
    (pg, addr, op)
}

/// The whole two-phase flow: a code that grants nothing, a manifest, a
/// human who narrows it, and a token collected exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_pairs_itself_and_a_human_decides_what_it_gets() {
    let (_pg, addr, op) = a_core_to_pair_with().await;
    let (id, code) = cut_a_pairing(&op).await;

    // The code travels as a ticket; the client reads it back off the QR.
    let ticket = erisdb_client::Ticket::parse(&ticket_for(&code)).expect("the ticket parses");
    assert_eq!(ticket.v, 1);
    assert_eq!(ticket.name.as_deref(), Some("my-laptop"));
    assert_eq!(ticket.token, code);

    // The code is a capability over nothing: it redeems its own session
    // and cannot read a single item.
    let app = Client::dial_addr(addr.clone(), &ticket.token, "Tasks (Android) v0.3", Some([31u8; 32]))
        .await
        .expect("dial with the code");
    let held = app.permissions().await.expect("permissions");
    assert_eq!(held.grants, ["meta:pairing:redeem"]);
    let (status, _) = app.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(status, 403, "a photographed code reads nothing");

    // Ask for four, and a human approves two of them.
    let cancel = Cancel::new();
    let (outcome, ()) = tokio::join!(
        app.pair(
            "Tasks (Android) v0.3",
            &["tasks:read", "tasks:create", "tasks:update", "tasks:delete"],
            Duration::from_secs(60),
            &cancel,
        ),
        answer(&op, &id, Some(&["tasks:read", "tasks:create"])),
    );
    let (token, granted) = match outcome.expect("pairing ran") {
        Pairing::Approved { token, granted } => (token, granted),
        other => panic!("expected an approval, got {other:?}"),
    };
    assert_eq!(granted, ["tasks:read", "tasks:create"], "the human narrowed the request");

    // The token is real, and is exactly what was approved.
    let paired = Client::dial_addr(addr, &token, "Tasks (Android) v0.3", Some([31u8; 32]))
        .await
        .expect("dial paired");
    assert_eq!(paired.permissions().await.expect("permissions").grants, granted);
    let (status, item) = paired
        .request("POST", "/v1/items", Some(json!({"facet": "tasks", "body": {"text": "x"}})))
        .await
        .unwrap();
    assert_eq!(status, 201, "{item}");
    let (status, _) = paired
        .request("DELETE", &format!("/v1/items/{}", item["id"].as_str().unwrap()), None)
        .await
        .unwrap();
    assert_eq!(status, 403, "delete was asked for and not granted");

    // The bound installation can recover if a collection response is lost.
    assert!(matches!(app.pairing_status().await.unwrap(), Pairing::Approved { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_pairing_grants_nothing() {
    let (_pg, addr, op) = a_core_to_pair_with().await;
    let (id, code) = cut_a_pairing(&op).await;
    let app = Client::dial_addr(addr, &code, "Rejected v0", Some([32u8; 32])).await.expect("dial");

    let cancel = Cancel::new();
    let (outcome, ()) = tokio::join!(
        app.pair("Rejected v0", &["tasks:read"], Duration::from_secs(60), &cancel),
        answer(&op, &id, None),
    );
    assert_eq!(outcome.expect("pairing ran"), Pairing::Denied);
}

#[tokio::test(flavor = "multi_thread")]
async fn installation_keys_bind_collection_renewal_and_access_over_real_iroh() {
    let (_pg, addr, operator) = a_core_to_pair_with().await;
    let (id, code) = cut_a_pairing(&operator).await;
    let app = Client::dial_addr(addr.clone(), &code, "Phone", Some([71; 32])).await.unwrap();
    let intruder = Client::dial_addr(addr.clone(), &code, "Phone", Some([72; 32])).await.unwrap();
    let requested = app.redeem_pairing("Phone", &["tasks:read", "tasks:create"]).await.unwrap();
    let (_, seen) = operator.request("GET", &format!("/v1/pairings/{id}"), None).await.unwrap();
    assert_eq!(requested["body"]["fingerprint"], seen["body"]["fingerprint"]);
    let (status, approved) = operator.request("POST", &format!("/v1/pairings/{id}/approve"), Some(json!({"ttl_secs": 1}))).await.unwrap();
    assert_eq!(status, 200, "{approved}");
    assert_eq!(intruder.request("GET", "/v1/pair/status", None).await.unwrap().0, 401);
    let Pairing::Approved { token, .. } = app.pairing_status().await.unwrap() else { panic!("not approved") };
    let stolen = Client::dial_addr(addr.clone(), &token, "Phone", Some([73; 32])).await.unwrap();
    assert_eq!(stolen.request("GET", "/v1/items?facet=tasks", None).await.unwrap().0, 401);
    assert!(stolen.refresh_capability(1).await.is_err());

    // The real installation sleeps past expiry, then reconnects and writes.
    // The explicit 401 is safely retried after identity-authenticated renewal.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let resumed = Client::dial_addr(addr, &token, "Phone", Some([71; 32])).await.unwrap();
    let (status, item) = resumed.request("POST", "/v1/items", Some(json!({"facet": "tasks", "body": {"text": "once"}}))).await.unwrap();
    assert_eq!(status, 201, "{item}");
    assert_eq!(item["source"]["installation"], id);
    let (_, items) = resumed.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(items["items"].as_array().unwrap().len(), 1, "renewal duplicated a write");
    assert_eq!(operator.request("POST", &format!("/v1/clients/{id}/revoke"), Some(json!({}))).await.unwrap().0, 200);
    assert_eq!(resumed.request("GET", "/v1/items?facet=tasks", None).await.unwrap().0, 401);
    assert!(resumed.refresh_capability(1).await.is_err());
}

/// A human may never answer, so waiting is always bounded.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_gives_up_rather_than_waiting_forever() {
    let (_pg, addr, op) = a_core_to_pair_with().await;
    let (_id, code) = cut_a_pairing(&op).await;
    let app = Client::dial_addr(addr, &code, "Ignored v0", Some([33u8; 32])).await.expect("dial");

    let cancel = Cancel::new();
    let started = Instant::now();
    let outcome = app
        .pair("Ignored v0", &["tasks:read"], Duration::from_secs(2), &cancel)
        .await
        .expect("pairing ran");
    assert_eq!(outcome, Pairing::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(30), "the deadline was not honoured");
}

/// Closing the pairing screen stops the wait, whatever deadline it was
/// given.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_pairing_is_cancellable() {
    let (_pg, addr, op) = a_core_to_pair_with().await;
    let (_id, code) = cut_a_pairing(&op).await;
    let app = Client::dial_addr(addr, &code, "Impatient v0", Some([34u8; 32])).await.expect("dial");

    let cancel = Cancel::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stopper.cancel();
    });

    let started = Instant::now();
    let outcome = app
        .pair("Impatient v0", &["tasks:read"], Duration::from_secs(3600), &cancel)
        .await
        .expect("pairing ran");
    assert_eq!(outcome, Pairing::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(30), "cancelling is not a deadline");
}

/// The FFI shape of pairing: redeem once, then pull for an answer, with
/// every wait bounded and every answer a value.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_facade_pairs() {
    let _facade = facade();
    let (_pg, addr, op) = a_core_to_pair_with().await;
    let (id, code) = cut_a_pairing(&op).await;
    let addr_json = serde_json::to_string(&addr).unwrap();

    // A ticket comes off the camera as one string; parsing it is a call
    // of its own, so the app can show the core's name before dialing.
    let encoded = ticket_for(&code);
    let parsed = std::thread::spawn(move || erisdb_client::blocking::parse_ticket(&encoded))
        .join()
        .unwrap();
    let v: Value = serde_json::from_str(&parsed).unwrap();
    assert_eq!(v["ok"], true, "{parsed}");
    assert_eq!(v["ticket"]["name"], "my-laptop");
    assert_eq!(v["ticket"]["token"], code.as_str());

    let redeemed = std::thread::spawn(move || {
        erisdb_client::blocking::pair_redeem(
            &addr_json,
            &code,
            "Tasks (Android) v0.3",
            r#"["tasks:read","tasks:create"]"#,
            &[35u8; 32],
        )
    })
    .join()
    .unwrap();
    let v: Value = serde_json::from_str(&redeemed).unwrap();
    assert_eq!(v["ok"], true, "{redeemed}");
    assert_eq!(v["pairing"]["body"]["client"], "Tasks (Android) v0.3", "{redeemed}");

    // Nobody has answered yet: the pull ends as data, not as a hang.
    let waiting =
        std::thread::spawn(|| erisdb_client::blocking::pair_poll(700)).join().unwrap();
    let v: Value = serde_json::from_str(&waiting).unwrap();
    assert_eq!(v["ok"], true, "{waiting}");
    assert_eq!(v["status"], "waiting", "{waiting}");

    answer(&op, &id, Some(&["tasks:read"])).await;

    let approved =
        std::thread::spawn(|| erisdb_client::blocking::pair_poll(20_000)).join().unwrap();
    let v: Value = serde_json::from_str(&approved).unwrap();
    assert_eq!(v["ok"], true, "{approved}");
    assert_eq!(v["status"], "approved", "{approved}");
    assert!(v["token"].as_str().unwrap().starts_with("erisdb1."), "{approved}");
    assert_eq!(v["granted"], json!(["tasks:read"]), "{approved}");

    // The session is spent; the slot is gone with it.
    let after = std::thread::spawn(|| erisdb_client::blocking::pair_poll(100)).join().unwrap();
    let v: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["ok"], false, "{after}");
}

/// A parked pull wakes when the app closes the pairing screen, rather
/// than holding a thread until its timeout runs out.
#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_pairing_poll_is_cancellable() {
    let _facade = facade();
    let (_pg, addr, op) = a_core_to_pair_with().await;
    let (_id, code) = cut_a_pairing(&op).await;
    let addr_json = serde_json::to_string(&addr).unwrap();

    std::thread::spawn(move || {
        erisdb_client::blocking::pair_redeem(
            &addr_json,
            &code,
            "Impatient (Blocking) v0",
            r#"["tasks:read"]"#,
            &[36u8; 32],
        )
    })
    .join()
    .unwrap();

    let parked = std::thread::spawn(|| erisdb_client::blocking::pair_poll(600_000));
    std::thread::sleep(Duration::from_millis(400));
    let started = Instant::now();
    std::thread::spawn(erisdb_client::blocking::pair_cancel).join().unwrap();
    let answer = parked.join().unwrap();

    assert!(started.elapsed() < Duration::from_secs(30), "cancelling is not a deadline");
    let v: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(v["ok"], true, "{answer}");
    assert_eq!(v["status"], "cancelled", "{answer}");
}

/// A ticket is read with what every platform already has, and refused
/// loudly rather than half-stored. The good case is a ticket the core
/// itself built.
#[test]
fn a_ticket_is_read_or_refused() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine;

    let eid = "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06";
    let good = erisdb::ticket::Ticket::new(
        "erisdb1.payload.sig".into(),
        Some(eid.into()),
        Some("http://10.0.0.2:7700".into()),
        Some("my-laptop".into()),
    )
    .unwrap()
    .encode()
    .unwrap();

    let t = erisdb_client::Ticket::parse(&format!("  {good}\n")).expect("whitespace is forgiven");
    assert_eq!(t.v, 1);
    assert_eq!(t.token, "erisdb1.payload.sig");
    assert_eq!(t.eid.as_deref(), Some(eid));
    assert_eq!(t.url.as_deref(), Some("http://10.0.0.2:7700"));
    assert_eq!(t.name.as_deref(), Some("my-laptop"));

    let encode = |v: Value| format!("erisdb://pair/{}", B64.encode(serde_json::to_vec(&v).unwrap()));
    for bad in [
        String::new(),
        good.strip_prefix("erisdb://pair/").unwrap().to_string(),
        format!("https://pair/{good}"),
        "erisdb://pair/!!!not-base64!!!".to_string(),
        format!("erisdb://pair/{}", B64.encode("not json")),
        // An unknown version is refused rather than guessed at.
        encode(json!({"v": 2, "token": "erisdb1.a.b", "eid": eid})),
        // No address is half a config.
        encode(json!({"v": 1, "token": "erisdb1.a.b"})),
        // No code to redeem.
        encode(json!({"v": 1, "eid": eid})),
        encode(json!({"v": 1, "token": "", "eid": eid})),
        // An endpoint id that is not one.
        encode(json!({"v": 1, "token": "erisdb1.a.b", "eid": "too-short"})),
        encode(json!({"v": 1, "token": "erisdb1.a.b", "eid": "g".repeat(64)})),
    ] {
        assert!(erisdb_client::Ticket::parse(&bad).is_err(), "accepted {bad:?}");
    }

    // A url-only ticket is legal, and this client says plainly that it
    // cannot dial one: it speaks QUIC and nothing else.
    let browser_only = encode(json!({"v": 1, "token": "erisdb1.a.b", "url": "http://10.0.0.2:7700"}));
    let t = erisdb_client::Ticket::parse(&browser_only).expect("legal ticket");
    assert_eq!(t.eid, None);
    let refused = t.endpoint_id().unwrap_err().to_string();
    assert!(refused.contains("endpoint id"), "{refused}");
}

// ---------------------------------------------------------------------
// The retry rule, watched from the other end of the wire.
//
// Whether a failed call is safe to repeat is invisible from the calling
// side: both a lost request and a lost response look like one error. The
// only witness is the server, which knows how many requests actually
// arrived. So these tests put a real Iroh peer speaking the erisdb ALPN on
// the other end and have it fail in specific, chosen ways.
// ---------------------------------------------------------------------

/// The proxy only changes delivery; every response comes from the real core.
#[derive(Clone, Copy, PartialEq)]
enum Misbehaviour {
    DieHoldingTheRequest,
    DieOnceThenAnswer,
    AnswerThenClose,
    PassThrough,
}

struct Peer {
    _pg: testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    core: Client,
    addr: iroh::EndpointAddr,
    requests: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
}

impl Peer {
    fn requests(&self) -> usize { self.requests.lock().unwrap().len() }
    fn first_request(&self) -> String { self.requests.lock().unwrap()[0].clone() }
    fn connections(&self) -> usize { self.connections.load(Ordering::SeqCst) }
}

/// A QUIC edge forwards complete HTTP exchanges to the actual ErisDB server.
/// It can discard the answer only AFTER the server completed the transaction,
/// reproducing an ambiguous write without inventing any service responses.
async fn misbehaving_peer(how: Misbehaviour) -> Peer {
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper_util::rt::TokioIo;
    let (pg, core_addr, core) = a_core_to_pair_with().await;
    let ep = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .alpns(vec![erisdb_client::ALPN.to_vec()]).bind().await.unwrap();
    let upstream = ep.connect(core_addr, erisdb_client::ALPN).await.unwrap();
    let addr = ep.addr();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let (seen, count) = (requests.clone(), connections.clone());
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let Ok(conn) = incoming.await else { continue };
            let nth = count.fetch_add(1, Ordering::SeqCst);
            let (seen, upstream) = (seen.clone(), upstream.clone());
            tokio::spawn(async move {
                while let Ok((send, recv)) = conn.accept_bi().await {
                    let (seen, upstream, conn) = (seen.clone(), upstream.clone(), conn.clone());
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                            let (seen, upstream, conn) = (seen.clone(), upstream.clone(), conn.clone());
                            async move {
                                seen.lock().unwrap().push(format!("{} {}", request.method(), request.uri()));
                                let (parts, body) = request.into_parts();
                                let request = hyper::Request::from_parts(parts, Full::new(body.collect().await?.to_bytes()));
                                let (send, recv) = upstream.open_bi().await?;
                                let (mut sender, driver) = hyper::client::conn::http1::handshake(
                                    TokioIo::new(tokio::io::join(recv, send))).await?;
                                tokio::spawn(driver);
                                let response = sender.send_request(request).await?;
                                let (parts, body) = response.into_parts();
                                let body = body.collect().await?.to_bytes();
                                if how == Misbehaviour::DieHoldingTheRequest ||
                                    how == Misbehaviour::DieOnceThenAnswer && nth == 0 {
                                    conn.close(1u32.into(), b"lost response after real core completed");
                                } else if how == Misbehaviour::AnswerThenClose {
                                    tokio::spawn(async move {
                                        tokio::time::sleep(Duration::from_millis(300)).await;
                                        conn.close(0u32.into(), b"one request per connection");
                                    });
                                }
                                Ok::<hyper::Response<Full<Bytes>>, anyhow::Error>(hyper::Response::from_parts(parts, Full::new(body)))
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(tokio::io::join(recv, send)), service).await;
                    });
                }
            });
        }
    });
    Peer { _pg: pg, core, addr, requests, connections }
}

async fn client_for(peer: &Peer, identity: [u8; 32]) -> Client {
    let token = erisdb::auth::mint(SECRET, &["*"], Some(3600), None).unwrap();
    Client::dial_addr(peer.addr.clone(), &token, "Retry v0", Some(identity)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_never_sent_twice() {
    let peer = misbehaving_peer(Misbehaviour::DieHoldingTheRequest).await;
    let client = client_for(&peer, [20u8; 32]).await;
    let outcome = client.request("POST", "/v1/items",
        Some(json!({"facet": "tasks", "body": {"text": "once"}}))).await;
    assert!(outcome.is_err(), "a lost answer is an error, not a silent retry");
    assert_eq!(peer.requests(), 1);
    let (status, stored) = peer.core.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(stored["items"].as_array().unwrap().len(), 1, "the real write committed exactly once");
    assert_eq!(stored["items"][0]["body"]["text"], "once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redeem_is_never_sent_twice() {
    let peer = misbehaving_peer(Misbehaviour::DieHoldingTheRequest).await;
    let (id, code) = cut_a_pairing(&peer.core).await;
    let client = Client::dial_addr(peer.addr.clone(), &code, "Retry v0", Some([24u8; 32])).await.unwrap();
    assert!(client.redeem_pairing("Tasks (Android) v0.3", &["tasks:read"]).await.is_err());
    assert_eq!(peer.requests(), 1);
    assert!(peer.first_request().starts_with("POST /v1/pair/redeem"));
    let (_, pairing) = peer.core.request("GET", &format!("/v1/pairings/{id}"), None).await.unwrap();
    assert_eq!(pairing["body"]["status"], "requested");
    assert_eq!(pairing["revision"], 2, "redeeming committed one state transition");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_poll_is_retried_on_a_broken_connection() {
    let peer = misbehaving_peer(Misbehaviour::DieOnceThenAnswer).await;
    let (_, code) = cut_a_pairing(&peer.core).await;
    let client = Client::dial_addr(peer.addr.clone(), &code, "Retry v0", Some([25u8; 32])).await.unwrap();
    assert_eq!(client.pairing_status().await.unwrap(), Pairing::Waiting);
    assert_eq!(peer.requests(), 2);
    assert_eq!(peer.connections(), 2);
    assert!(peer.first_request().starts_with("GET /v1/pair/status"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_is_retried_on_a_broken_connection() {
    let peer = misbehaving_peer(Misbehaviour::DieOnceThenAnswer).await;
    let client = client_for(&peer, [21u8; 32]).await;
    let (status, body) = client.request("GET", "/v1/health", None).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(peer.requests(), 2);
    assert_eq!(peer.connections(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_cached_connection_is_redialled_for_a_write() {
    let peer = misbehaving_peer(Misbehaviour::AnswerThenClose).await;
    let client = client_for(&peer, [22u8; 32]).await;
    client.request("GET", "/v1/health", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (status, item) = client.request("POST", "/v1/items",
        Some(json!({"facet": "tasks", "body": {"text": "redialled"}}))).await.unwrap();
    assert_eq!(status, 201);
    assert_eq!(item["body"]["text"], "redialled");
    assert_eq!(peer.requests(), 2);
    assert_eq!(peer.connections(), 2);
    let (_, stored) = peer.core.request("GET", "/v1/items?facet=tasks", None).await.unwrap();
    assert_eq!(stored["items"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_json_arrives_as_text() {
    let peer = misbehaving_peer(Misbehaviour::PassThrough).await;
    let client = client_for(&peer, [23u8; 32]).await;
    let (status, body) = client.request("GET", "/v1/items/not-a-uuid", None).await.unwrap();
    assert_eq!(status, 400);
    assert!(body.as_str().is_some_and(|text| text.contains("UUID")), "{body}");
}

// ---------------------------------------------------------------------
// The FFI safety net.
// ---------------------------------------------------------------------

/// A panic inside the FFI surface comes back as the same envelope every
/// other failure uses. Unwinding out of an `extern "system"` fn aborts the
/// process, so this is what keeps a bug in the client from killing the
/// app around it.
#[test]
fn a_panic_becomes_an_envelope() {
    let response =
        erisdb_client::blocking::guard(|| panic!("boom"), erisdb_client::blocking::panic_envelope);
    let v: Value = serde_json::from_str(&response).expect("the envelope is JSON");
    assert_eq!(v["ok"], false, "{response}");
    assert_eq!(v["status"], 0, "{response}");
    assert!(v["error"].as_str().unwrap().contains("boom"), "{response}");
}

/// Identity hex is decoded by character, not by byte offset: whatever
/// 64-byte string the JVM hands over, the answer is data.
#[test]
fn identity_hex_never_panics() {
    let good = "0".repeat(64);
    assert_eq!(erisdb_client::blocking::decode_identity_hex(&good), Some([0u8; 32]));
    assert_eq!(
        erisdb_client::blocking::decode_identity_hex(&"AB".repeat(32)),
        Some([0xABu8; 32]),
        "uppercase hex decodes too"
    );

    // 64 bytes, 22 characters: byte-slicing this lands inside a character.
    let multibyte = format!("{}0", "€".repeat(21));
    assert_eq!(multibyte.len(), 64);
    assert_eq!(erisdb_client::blocking::decode_identity_hex(&multibyte), None);

    assert_eq!(erisdb_client::blocking::decode_identity_hex(""), None);
    assert_eq!(erisdb_client::blocking::decode_identity_hex(&"z".repeat(64)), None);
    assert_eq!(erisdb_client::blocking::decode_identity_hex(&"0".repeat(63)), None);
}

/// A caught panic leaves the facade usable: the next call is answered
/// normally.
#[tokio::test(flavor = "multi_thread")]
async fn the_facade_survives_a_panicking_call() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let token = erisdb::auth::mint(SECRET, &["*"], Some(3600), None).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    let response = std::thread::spawn(move || {
        erisdb_client::blocking::configure(&addr_json, &token, "Guarded v0", &[14u8; 32])
            .expect("configure");
        erisdb_client::blocking::guard(|| panic!("boom"), erisdb_client::blocking::panic_envelope);
        erisdb_client::blocking::request("GET", "/v1/health", None)
    })
    .join()
    .unwrap();
    let v: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(v["status"], 200, "{response}");
}

/// Concurrent callers all make progress while a subscription is parked:
/// the facade's runtime is sized for a live feed plus a handful of calls,
/// which is what an app with a sync loop and a UI actually does.
#[tokio::test(flavor = "multi_thread")]
async fn the_facade_serves_calls_while_a_subscription_is_parked() {
    let _facade = facade();
    let (_pg, addr) = spawn_core().await;
    let token = erisdb::auth::mint(SECRET, &["*"], Some(3600), None).unwrap();
    let addr_json = serde_json::to_string(&addr).unwrap();

    let sub = std::thread::spawn(move || {
        erisdb_client::blocking::configure(&addr_json, &token, "Busy v0", &[15u8; 32])
            .expect("configure");
        erisdb_client::blocking::subscribe_changes(0, None).expect("subscribe")
    })
    .join()
    .unwrap();

    // One thread parked on the feed, four asking questions at once.
    let parked = std::thread::spawn(move || erisdb_client::blocking::next_change(sub, 3_000));
    let callers: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| erisdb_client::blocking::request("GET", "/v1/health", None)))
        .collect();
    for caller in callers {
        let response = caller.join().unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["status"], 200, "{response}");
    }
    parked.join().unwrap();
    erisdb_client::blocking::close_subscription(sub);
}
