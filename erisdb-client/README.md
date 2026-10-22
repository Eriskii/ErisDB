# erisdb-client

The client of `database/1`: dials the mesh's database, which ErisDB serves,
over Iroh as an app its owner's mesh let in. One QUIC connection, one HTTP/1.1
exchange per bi-stream, ALPN `eris/database/1` — the same router the core serves
over TCP, reached from anywhere without a port to forward or a certificate to
manage. The app's key is its credential: requests carry no
token, and ErisDB knows the app by the key the QUIC handshake proved.

Three layers, each a thin wrapper over the one below:

```
Client          async, the real thing: sign in, dial, request, subscribe
blocking        one process-wide runtime and client, every answer a value
android         the JNI surface, bound to dev.erisdb.client.ErisDB
```

`Client` is what Rust callers want. `blocking` exists because FFI callers
have no executor to hand: it owns a runtime, holds one client in a slot,
and returns JSON strings instead of `Result`. `android` compiles only for
that target and does nothing but marshal strings.

## Signing in

An app gets into ErisDB through a device of its owner's mesh. Apps ask for
capabilities, not programs: `CAPABILITY` is `"database/1"`, and ErisDB is what
serves it. `sign_in` asks the device's `eon`, over its agent socket
(`$XDG_RUNTIME_DIR/eon/agent.sock`), to approve the app's key to use
`database/1` with the grants it wants, serving nothing. The person approves on
`eon`'s screen — all of them or fewer — and types ErisDB's passphrase there;
`eon` enrolls the app at the mesh's database and answers once ErisDB has taken
it. `sign_in` then dials the database, which it finds in the snapshot as the app
approved to serve `database/1`:

```rust
use erisdb_client::{sign_in, Client, SignIn, CAPABILITY};

let key = load_or_create_the_apps_key()?; // iroh::SecretKey, the same every launch
let grants = ["tasks:read", "tasks:create"].map(String::from);
match sign_in(key.clone(), "Tasks (Linux)", &grants).await? {
    Some(SignIn::Approved { client, note, .. }) => {
        save(client.server(), &key)?;          // where ErisDB is, and the key
        render(note.uses(CAPABILITY).unwrap_or_default()); // may be fewer than asked
    }
    Some(SignIn::Denied(why)) => { /* the person said no */ }
    None => { /* no eon listening on this machine */ }
}
```

Later launches dial what they saved:

```rust
let client = Client::dial(saved_server, key, "Tasks (Linux)").await?;
let (status, body) = client.request("GET", "/v1/items?facet=tasks", None).await?;
```

`dial` binds an endpoint as the key on the network `ERIS_NETWORK` names —
`loopback`, a relay URL, or the internet when it names neither — and `dial_on`
takes the network as an argument. `name` is stamped into every write's
`source.client`; `source.addr` is the key. A 401 means the app is not, or no
longer, let in: its installation was revoked, its device removed from the mesh,
or its note revoked. Only a device lets it back in.

`locate(&snapshot)` reads where the mesh's database is from any snapshot of the
mesh: the app approved to serve `database/1` whose note counts, the most
recently approved should there be more than one (`Snapshot::server`). When the
person approves a new database, the old one's note is revoked and it lets no
more apps in; apps find the new one in any later snapshot, from `sign_in` or
from the agent's `Agent::snapshot`. `alpn()` is the ALPN it answers on,
`eris/database/1`, derived from `CAPABILITY`.
`parse_server` reads an address an app kept as text: an
`EndpointAddr` as JSON, or a bare endpoint id for discovery to resolve.

## Enrolling, as a device

Letting an app in is the device's side, and the mesh library's, not this
crate's: `eon` calls `erisoverlaynetwork::enroll` from the device's own
endpoint, which is what ErisDB checks against the mesh, over ALPN
`eris-enroll/0`. It syncs with ErisDB first, so ErisDB knows the device:

```rust
use erisoverlaynetwork::enroll::{self, Outcome};

let database = node.server(erisdb_client::CAPABILITY).expect("the mesh has a database");
match enroll::enroll(&node, database.id, &note, &typed_passphrase).await? {
    Outcome::Done => { /* answer the app: approved */ }
    Outcome::Wrong { retry_after } => { /* ask again; retry_after when this guess locked the device */ }
    Outcome::Locked { retry_after } => { /* say how long to wait */ }
    Outcome::NoPassphrase => { /* the operator sets one on the server */ }
}
```

`enroll::change(&node, database.id, current, new)` changes the passphrase, with
the same answers. The first three wrong guesses in a row from a device cost
nothing; the third locks it out for a minute, and each one after doubles the
wait, up to a day. A right guess clears the count. Any other refusal — not a
device of the mesh, a note that is not the device's own or does not use
`database/1`, a new passphrase under 8 characters, a database the mesh no longer
approves — is an error.

## Retries

The client retries reads and failures it can identify as occurring before
request transmission. It does not retry a write after an ambiguous transport
failure: a create may have committed even when its response was lost, and the
API has no idempotency key.

## Change feed

`subscribe_changes(since, facet)` holds one bi-stream open and yields events as
the core emits them. The caller owns the cursor: every event carries its `seq`,
and resubscribing with the last `seq` seen picks up exactly there. A sleeping
device loses nothing. The feed ends (`next()` is `None`) when the core closes
it, as it does the moment the app stops counting.

## One-shot plugins

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

Nothing crosses the boundary as a panic. Refusals carry a numeric `status`,
transport failures carry a message, poisoned locks are opened anyway, and every
JNI entry point catches unwinding before it can reach a frame where it would
abort the app rather than return.

An Android app signs in through ErisOverlayNetwork's bound service on the phone,
which answers with the app's note and a snapshot of the mesh, both JSON. The JNI
surface takes it from there:

```
nativeEndpointId(keyHex)                 -> endpoint id hex, the `app` to ask eon for
                                          | ""  (not 64 hex characters)
nativeLocate(snapshotJson)               -> {"ok": true, "server": <EndpointAddr JSON>}
                                          | {"ok": false, "error": …}
nativeDial(server, clientName, keyHex)   -> ""  | error message
nativeRequest(method, path, body)        -> {"status": n, "body": …}
                                          | {"status": 0, "error": …}
nativePermissions()                      -> {"ok": true, "permissions": {grants, client_id, exp, max_exp, user}}
                                          | {"ok": false, "status": n, "error": …}
nativeSubscribeChanges(since, facet)     -> handle, or 0
nativeNextChange(handle, timeoutMs)      -> {"ok": true, "change": …}
                                          | {"ok": true}   (timed out, feed alive)
                                          | {"ok": false, "error": …}
nativeCloseSubscription(handle)
```

All bound to `dev.erisdb.client.ErisDB`. The flow:

1. Hold the app's key: 32 bytes, as 64 hex characters, kept in the Android
   Keystore and the same every launch. `nativeEndpointId(keyHex)` is the `app`
   to hand `ErisOverlayNetwork.approve(name, mapOf("database/1" to grants), app = …)`.
2. On `Approved(note, snapshot)`, `nativeLocate(snapshot)` gives `server`.
   Persist it with the key. Once dialed, `nativePermissions()` says what the
   person granted, which may be fewer than asked.
3. `nativeDial(server, clientName, keyHex)` dials the process-wide client, on
   this launch and every later one. `server` may also be a bare endpoint id.
4. `nativeRequest` and the subscription calls are the API. `body` may be null.

`status` carries the HTTP response code, or 0 when no response arrived. 401 means
the app is not, or no longer, let in; 403 means it lacks the permission. Branch
on the number; the message is for humans. The change feed is a pull because
callbacks across FFI are painful: park a background thread in
`nativeNextChange` and remember each change's `seq` as the resume cursor.

The facade's runtime carries a live subscription plus a handful of concurrent
calls: four workers minimum, more on a bigger machine, capped at eight. Each
exchange spawns a hyper driver onto it and a subscription keeps one there for
the life of the feed.

`examples/dial.rs` is the same surface from a terminal, one request as a
reachability probe:

```sh
ERISDB_SERVER=<endpoint id or address JSON> ERISDB_KEY=<64 hex> cargo run --example dial
```

It asks `GET /v1/permissions`: what the key holds when a device enrolled it,
401 when not.

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
worth doing, since nothing else compiles that module on a host. A build outside
the Eris monorepo takes ErisOverlayNetwork from its git repository.

## Tests

**Docker is required.** The suite runs against a real core in a real mesh:
Postgres in a container via testcontainers, the erisdb binary (`ERISDB_BIN`, or
`../erisdb` built on demand) joined to a mesh and serving over Iroh, a device
node built from the mesh library in the test process, and this client dialing
as the apps the device let in. `eon` is a stand-in on its agent socket that
does what `eon` does once the person says yes: it signs the note and enrolls it
from the device's endpoint with `erisoverlaynetwork::enroll`. Every endpoint is on
loopback. No mocks.

```sh
cargo test
```

The sign-in tests run both halves for real: the app asks the stand-in, the
person approves fewer grants than asked or says no, and what the app gets back
is checked by using it. One test removes the app's device from the mesh and
watches the app's next request get 401 and its open feed end.

The retry tests instead put a deliberately broken peer on the wire — a real Iroh
endpoint speaking `eris/database/1`, itself an app the device let in, forwarding to the
real core and then dying without answering. Whether a call was safe to repeat is
invisible from the calling side, where a lost request and a lost response look
alike; only the server knows how many requests actually arrived.

## License

MIT. See [LICENSE](LICENSE). The core server is AGPL-3.0; the clients are
deliberately not.
