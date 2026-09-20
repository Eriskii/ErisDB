# erisdb-client

Dials an ErisDB over Iroh. One QUIC connection, one HTTP/1.1 exchange per
bi-stream, ALPN `bezel/0` — the same router the core serves over TCP,
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
    &token,                     // bz1.…
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
ERISDB_SERVER=$(erisdb endpoint-id --secret …) ERISDB_TOKEN=bz1.… \
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
a human answers. A photographed pairing screen is therefore worth
nothing on its own: redeeming it raises a prompt on the operator's
device, naming a client they did not install, asking for permissions
they can refuse.

```rust
let ticket = erisdb_client::Ticket::parse(scanned)?;   // bezel://pair/…
let cancel = erisdb_client::Cancel::new();             // the back button

match erisdb_client::pair(
    &ticket,
    "Tasks (Android) v0.3",
    &["tasks:read", "tasks:create", "tasks:update"],
    Some(identity),
    Duration::from_secs(300),
    &cancel,
).await? {
    Pairing::Approved { token, granted } => store(token, granted), // once!
    Pairing::Denied    => "the operator said no",
    Pairing::TimedOut  => "nobody answered",
    Pairing::Cancelled => "the user closed the screen",
    // `Waiting` is what one poll of `pairing_status()` says; a wait
    // that returns has stopped waiting.
    Pairing::Waiting   => unreachable!(),
}
```

The approved set is not always the requested set: an operator may select
the requested set or a subset. Read `granted` and render from it — and ask what
you hold at any time with `client.permissions()`, which needs no
permission because the answer is already inside the token.

**The installation proves possession of its Iroh key.** A copied QR cannot
collect another installation's approved token. Preserve the key passed to `dial`:
a different key cannot use, collect or renew its registered credentials.

- **Waiting is bounded.** A human may never answer, so `pair` takes a
  deadline and returns `TimedOut` rather than parking forever.
- **Waiting is cancellable.** A `Cancel` is clonable and every clone
  names the same signal; `cancel()` wakes the wait between polls.
- **Redeeming is never repeated.** It is a `POST` that moves a session
  from pending to requested; a second one is a 409. A lost answer is
  reported, exactly like any other write.

`erisdb_client::pair` resolves the ticket's `eid` through discovery, which
is all a phone holding a QR code has. A caller that already knows the
address dials it itself and calls `Client::pair`. A ticket may legally
carry only a `url` — one QR serving a browser on the LAN and a phone
anywhere — and this client says so plainly rather than half-dialing it:
it speaks QUIC and nothing else.

### Retries

A failed call is repeated only when repeating it is provably harmless.
The protocol carries no idempotency key, so a `POST` whose bytes already
left the process is never sent again — the core may have created the item
and lost only the answer, and a retry there is a second item. What does
get repeated: any failure that happened before the request went out
(a dead cached connection, a stream that would not open), and reads,
where asking twice changes nothing.

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

`status` is the core's own answer — 401 for a token too dead to refresh,
403 for a scope the core will not widen — and 0 when there was no answer
at all. Branch on the number; the message is for humans.

Pairing is a pull, like the change feed, because callbacks across FFI are
painful: parse the ticket, redeem it once, then loop on `nativePairPoll`
from a background thread while the status is `waiting`. Every call is
bounded by its own timeout, so no thread parks forever, and the back
button calls `nativePairCancel`, which wakes a parked poll with
`cancelled`. `approved` is the one and only sighting of the token —
write it to storage before anything else, and read `granted` rather than
assuming the request was granted whole.

Refreshing moves time, not privilege: the same grants, the same signed
user, a fresh expiry. Persisting the returned token is the app's job —
the client holds it only for its own lifetime.

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
real Iroh endpoint speaking `bezel/0` that reads a request and then dies
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
`client` ID, and the old bounded-chain endpoint for manual tokens. Current
registration permissions bound every request, including delegated tokens.
