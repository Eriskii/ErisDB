# erisdb-client

Dials an ErisDB over Iroh. One QUIC connection, one HTTP/1.1 exchange per
bi-stream, ALPN `erisdb/0` — the same router the core serves over TCP,
reached from anywhere without a port to forward or a certificate to
manage. Registered tokens are bound to the caller’s persistent Iroh installation key.
Postgres supplies current grants and revocation status on every request.

Three layers, each a thin wrapper over the one below:

```
Client          async, the real thing: dial, request, subscribe, pair
blocking        one process-wide runtime and client, every answer a value
android         the JNI surface, bound to dev.erisdb.client.ErisDB
```

`Client` is what Rust callers want. `blocking` exists because FFI callers
have no executor to hand: it owns a runtime, holds one client in a slot,
and returns JSON strings instead of `Result`. `android` compiles only for
that target and does nothing but marshal strings.

## Dialing

The address is whatever the user pasted. A bare endpoint id resolves
through discovery; a JSON `EndpointAddr` dials directly.

```rust
let client = erisdb_client::Client::dial(
    "iroh:8f1c…",              // or the addr JSON, or the bare id
    &token,                     // erisdb1.…
    "Lists (Android) v0.1",     // stamped into every write's source
    Some(identity),             // 32 bytes, pinning this device's endpoint key
).await?;

let (status, body) = client.request("GET", "/v1/items?facet=lists", None).await?;
```

`identity` matters more than it looks. The core stamps `source.addr` with
the endpoint id it observed, so passing the same 32 bytes every launch is
what makes a device one device across restarts instead of a new stranger
each time. `None` gives a fresh identity per process.

The endpoint id a core serves under is derived from its deployment
secret, so it is stable and answerable without a running server:

```sh
erisdb endpoint-id --secret …
```

`examples/dial.rs` is a one-request reachability probe against exactly
that id:

```sh
ERISDB_SERVER=$(erisdb endpoint-id --secret …) ERISDB_TOKEN=erisdb1.… \
  cargo run --example dial
```

It asks `GET /v1/permissions`, which needs no permission — so it probes
reachability and nothing else, and prints what the token turned out to
hold.

## Pairing

A client gets its token by pairing, and the QR code it scans is not the
token. The ticket carries a **pairing code**: a token holding exactly
`meta:pairing:redeem`, naming one session, good for minutes. The client
redeems it with a manifest — who it is, which permissions it wants — and
a human answers. Compare the request's fingerprint on both screens before
approving; display names are untrusted. A photographed ticket can race to request
enrollment, but it cannot collect another installation's approved credential.

Use the separate redemption and waiting methods so the app can display the
fingerprint before approval. This example takes an already-persisted installation
key and returns the pairing result for the caller to save:

```rust
use erisdb_client::{Cancel, Client, Pairing, Ticket};
use std::time::Duration;

async fn pair_ticket(
    scanned: &str,
    identity: [u8; 32],
    cancel: &Cancel,
) -> anyhow::Result<Pairing> {
    let ticket = Ticket::parse(scanned)?;
    let client = Client::dial(
        ticket.endpoint_id()?,
        &ticket.token,
        "Tasks (Rust)",
        Some(identity),
    ).await?;
    let session = client.redeem_pairing(
        "Tasks (Rust)",
        &["tasks:read", "tasks:create", "tasks:update"],
    ).await?;
    let fingerprint = session["body"]["fingerprint"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing pairing fingerprint"))?;
    println!("Compare this fingerprint with the operator: {fingerprint}");
    client.await_pairing(Duration::from_secs(300), cancel).await
}
```

`Pairing::Approved { token, granted }` supplies the access credential. Save it with
the core address and the same private key before using it. Dial a data client with
that access token; the client above was configured with the pairing token. Handle
`Denied`, `TimedOut`, and `Cancelled` in the UI. `Waiting` is an intermediate result
from `pairing_status()`; `await_pairing()` waits for a settled result.

The convenience functions `erisdb_client::pair` and `Client::pair` also redeem and
wait, but discard the redemption response and expose no fingerprint callback.
Use the separate methods above when implementing the comparison screen.

The approved set is not always the requested set: an operator may select
the requested set or a subset. Read `granted` and render from it — and ask what
you hold at any time with `client.permissions()`, which needs no additional
permission and returns the token's grants intersected with current registry grants.

**The installation proves possession of its Iroh key.** A copied QR cannot
collect another installation's approved token. Preserve the key passed to `dial`:
a different key cannot use, collect or renew its registered credentials.

- **Waiting is bounded.** `await_pairing` takes a waiting duration and returns
  `TimedOut` if approval has not arrived; network calls also have timeouts.
- **Waiting is cancellable.** A `Cancel` is clonable and every clone
  names the same signal; `cancel()` wakes the wait between polls.
- **Redemption is not blindly replayed.** Once the session is requested, another
  redemption returns 409. After a lost response, inspect the session state before
  deciding whether to submit again.

`Client::dial` resolves a bare endpoint ID through Iroh discovery. A caller with a
known `EndpointAddr` can dial that address directly. An Iroh client needs the
ticket's `eid`; `Ticket::endpoint_id()` returns an error for a URL-only ticket.

Save pending enrollment and the key before sending redemption. If its response
is lost, use `GET /v1/pair/status` through `Client::request` to inspect the raw state
and fingerprint. Only `pending` permits another redemption attempt; `requested`
means wait with the same identity. Successful collection is repeatable while the
session remains live. `pairing_status()` maps both pending states to `Waiting`,
so it cannot by itself distinguish whether resubmission is safe.

### Retries

The client retries reads and failures it can identify as occurring before request
transmission. It does not retry a write after an ambiguous transport failure:
a create may have committed even when its response was lost, and the API has no
idempotency key.

Registered requests receiving an explicit 401 can renew with the persistent Iroh
key and retry once. Pairing collection is safe to repeat with the same key while
the ticket lives, although that GET also writes registration state.

### Change feed

`subscribe_changes(since, facet)` holds one bi-stream open and yields
events as the core emits them. The caller owns the cursor: every event
carries its `seq`, and resubscribing with the last `seq` seen picks up
exactly there. A sleeping device loses nothing.

### One-shot plugins

`call_plugin(plugin, operation, input)` buffers an ordinary plugin response.
`stream_plugin(plugin, operation, input)` returns a `PluginStream` exposing
the response status, content type, upstream request id, and raw body chunks:

```rust
let mut stream = client.stream_plugin(
    "openai",
    "chat.completions",
    serde_json::json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true
    }),
).await?;

while let Some(bytes) = stream.next_chunk().await? {
    consume_sse(bytes);
}
```

The client does not parse SSE or assume JSON because plugin media types are
open. Dropping the stream closes the response, which causes the core to kill
that invocation's process. Plugin `POST`s are never retried after sending.

## The FFI surface

Nothing crosses the boundary as a panic. Refusals carry a numeric
`status`, transport failures carry a message, poisoned locks are opened
anyway, and every JNI entry point catches unwinding before it can reach a
frame where it would abort the app rather than return.

```
nativeConfigure(server, token, clientName, identityHex) -> ""  | error message
nativeRequest(method, path, body)                       -> {"status": n, "body": …}
                                                         | {"status": 0, "error": …}
nativeRefreshCapability(ttlSecs)                        -> {"ok": true, "token": …}
                                                         | {"ok": false, "status": n, "error": …}
nativePermissions()                                     -> {"ok": true, "permissions": {grants, exp, max_exp, user}}
nativeParseTicket(ticket)                               -> {"ok": true, "ticket": {v, name, eid, url, token}}
                                                         | {"ok": false, "error": …}
nativePairRedeem(server, code, clientName,
                 requestedJson, identityHex)            -> {"ok": true, "pairing": …}
                                                         | {"ok": false, "status": n, "error": …}
nativePairPoll(timeoutMs)                               -> {"ok": true, "status": "waiting"}
                                                         | {"ok": true, "status": "approved", "token": …, "granted": […]}
                                                         | {"ok": true, "status": "denied"}
                                                         | {"ok": true, "status": "cancelled"}
                                                         | {"ok": false, "status": n, "error": …}
nativePairCancel()
nativeSubscribeChanges(since, facet)                    -> handle, or 0
nativeNextChange(handle, timeoutMs)                     -> {"ok": true, "change": …}
                                                         | {"ok": true}   (timed out, feed alive)
                                                         | {"ok": false, "error": …}
nativeCloseSubscription(handle)
```

`status` carries the HTTP response code, or 0 when no response arrived. For
example, 401 can mean an invalid credential, revoked registration, or rejected
installation proof; 403 means insufficient permission. Registered access can renew
after token expiry. Branch on the number; the message is for humans.

Pairing is a pull, like the change feed, because callbacks across FFI are
painful: parse the ticket, call `nativePairRedeem`, and display
`pairing.body.fingerprint` for comparison. Then loop on `nativePairPoll` from a
background thread while the status is `waiting`. Every call is
bounded by its own timeout, so no thread parks forever, and the back
button calls `nativePairCancel`, which wakes a parked poll with
`cancelled`. Persist `approved` before proceeding —
write it to storage before anything else, and read `granted` rather than
assuming the request was granted whole.

Registered renewal issues the registration's current grants and signed user
label; permissions can therefore change. Manual-token refresh keeps its scope
bounded by the existing token. Persisting an explicitly refreshed token is the
app's job. Automatic renewal also updates the in-memory token; retaining the
installation key allows renewal again after a restart with an expired token.

The facade's runtime carries a live subscription plus a handful of
concurrent calls: four workers minimum, more on a bigger machine, capped
at eight. Each exchange spawns a hyper driver onto it and a subscription
keeps one there for the life of the feed.

## Building for Android

The crate is a `cdylib`; the JNI symbols bind `dev.erisdb.client.ErisDB`,
which is app-neutral on purpose — any app can load the same `.so`.

```sh
cargo install cargo-ndk
cargo ndk -t arm64-v8a -t x86_64 -o app/src/main/jniLibs build --release
```

`cargo-ndk` wants `ANDROID_NDK_HOME` pointing at an NDK. Without it,
`cargo check --target aarch64-linux-android` still type-checks the JNI
surface as long as `CC_aarch64_linux_android` names the NDK's clang —
worth doing, since nothing else compiles that module on a host.

## Tests

**Docker is required.** The suite runs against a real core: Postgres in a
container via testcontainers, a real erisdb serving over real Iroh QUIC,
and this client dialing it. No mocks.

```sh
cargo test
```

The pairing tests run both halves for real: the client redeems a code
the core cut, and an operator on a second connection approves a subset,
denies, or never answers at all. What the client gets back is checked by
using it — dialing with the granted token and watching the core refuse
the one permission the human withheld.

The retry tests instead put a deliberately broken peer on the wire — a
real Iroh endpoint speaking `erisdb/0` that reads a request and then dies
without answering. Whether a call was safe to repeat is invisible from
the calling side, where a lost request and a lost response look alike;
only the server knows how many requests actually arrived.

The suite builds the core from source via a `erisdb = { path = "../erisdb" }`
dev-dependency, so it runs inside a ErisDB checkout.

## License

MIT. See [LICENSE](LICENSE). The core server is AGPL-3.0; the clients are
deliberately not.

## Installation renewal

Registered requests that receive 401 renew with their persistent Iroh key, even
after access expiry, then retry once. Revocation makes renewal fail too. An
explicit 401 is safe to retry because authorization preceded effects; ambiguous
transport failures retain the existing no-retry rule for writes.

`refresh_capability` selects installation renewal when the token carries a
`client` ID, and the bounded-chain endpoint for manual tokens. Current
registration permissions bound every request, including delegated tokens.
