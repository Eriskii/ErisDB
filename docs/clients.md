# Building a client

Everything that is not the core is a client: the apps, the MCP bridge, the
poker, the Android builds, whatever you write next. A client is a thing
holding a capability token. There is no registration, no client id, no
handshake beyond the transport's own.

One-shot plugins are the exception to “everything”: they are
operator-installed executables invoked through `/v1/call`, not persistent
clients. An app calling one is still an ordinary client and pairs for the
operation permission such as `openai:chat`. Plugin calls have no cursor or
stored server state; ordinary responses are direct and streaming responses
last only as long as their connection.

This page is the shape a good one takes. [api.md](api.md) is the reference
it implements.

## 1. Ask for a token, and get an address

Two strings: where the core is, and what to say. A ticket carries the
address and a **pairing code** as one QR code, which is the intended path
for anything with a camera — see [pairing.md](pairing.md) for the format
and how to read one on every platform. Copy-paste of a URL and a token
works too and is what the web apps do.

The code in a ticket is not a capability over data. Redeeming it is where a
client says what it is and what it wants:

```
POST /v1/pair/redeem   { "client": "Tasks (Android) v0.3",
                         "requested": ["tasks:read", "tasks:create",
                                       "tasks:update", "tasks:delete"] }
GET  /v1/pair/status   → poll until status is approved or denied
```

`status` returns the token **once**. Persist it in the same write that
records the pairing; a second poll returns the same session with no token,
and nothing re-issues it.

Two rules for the manifest, both about the human at the other end:

- **Ask for what you use, and no more.** The request is a list a person
  reads and answers item by item. A client that asks for `*` because it is
  easier is asking to be denied.
- **Ask for optional things separately, and work without them.** A client
  that wants to register its own facet asks for `meta:facets:write`
  knowing that grant registers, changes and removes *every* facet on the
  core. Declining it should cost the operator a sentence — "ask whoever
  runs this to register `tasks`" — not the app.

After pairing, `GET /v1/permissions` tells you exactly what you got:

```json
{ "grants": ["tasks:read", "tasks:create"], "exp": 1788133838,
  "max_exp": 1790120945, "user": "phone" }
```

Draw the UI from that answer rather than from what you asked for. An app
granted three of the four actions should not offer a delete button that
403s.

Store the token and the address together and treat them as one credential.
A token is a bearer credential: whoever holds it holds exactly what it
grants until it expires. Do not log it, do not put it in a query string, and do not write
it back into the UI — the web apps show a *configured* state and never
re-render the secret.

Where the token lives depends on the platform, and none of the options are
good in an absolute sense: `localStorage` in a browser, an app-private
file on Android, a mode-0600 file for a daemon. Pick the one the platform
actually enforces, and hold grants narrow enough that the loss is bounded.

## 2. Seed a cache, then hold a cursor

The shape every app in this tree uses:

```
1. GET  /v1/items?facet=X&limit=1000      → bulk-load into a local cache
2. GET  /v1/changes?since=0&facet=X       → take `next` as the cursor
3. loop: GET /v1/changes?since=<cursor>&facet=X
         apply each row, cursor = next
4. alongside: GET /v1/changes/stream?since=<cursor>&facet=X
```

Seed once. After that the feed is the only thing you read, because each
row carries the full body and the revision it produced — enough to keep a
mirror true with no per-item refetch.

Applying a row is four cases:

- `created` / `updated` — put `{id, facet, body, revision}` in the cache.
- `deleted` — remove it.
- `lapsed` — the item is unchanged; do whatever a lapse means to you
  (notify, badge, nothing) and update the cache from the body anyway,
  since it is there.
- `tick` — only visible on an unfiltered feed. Usually nothing.

Advance the cursor to the row's `seq` and persist it with the cache, in
the same write. A cache newer than its cursor replays rows it has already
applied — harmless if apply is idempotent, and it should be. A cursor
newer than its cache loses data permanently.

`updated_since` on `GET /v1/items` is a bulk-load filter, not a sync
cursor. It is a timestamp with strict `>`, so a group of items sharing one
`updated_at` cannot be split across pages. Seed with it if you like; sync
with `seq`.

### Poll and stream, not one or the other

The SSE stream is an optimisation. The poll is the source of correctness.
Streams end — a token expires, a connection drops, a proxy times out, all
32 stream slots are taken and you get a 503 — and every one of those ends
looks the same from the client side: the response simply closes, with no
final event.

So run both. Catch up by poll on a timer; hold a stream open for latency;
on any drop, catch up and reconnect with backoff. Because the cursor is
client-held and every connection opens at `?since=<cursor>`, a drop costs
nothing but the delay. `apps/tasks/index.html` polls every ten seconds
while the stream is down and every minute while it holds, and syncs again
on `visibilitychange` — a phone that was asleep is exactly the case the
cursor exists for.

## 3. Write with revisions

Every write is whole-body. `PUT /v1/items/{id}` replaces the body with
what you sent, so a client that renders a partial view and writes it back
destroys every field it did not echo. Keep the last body you saw, overlay
your change on it, send the result.

Every write carries a `revision`, and a stale one is a 409. That is the
concurrency model in full: last-writer-wins is not available, and a 409 is
not an error condition so much as a message that says *read again and
retry*. Handle it by refetching the item (or waiting for the feed to bring
you the newer version) and reapplying your intent, not by resending the
same body with a bumped number.

`DELETE` takes an optional `revision`. Pass it if you have one — a delete
racing someone else's edit becomes a 409 instead of silently winning.

Two conventions worth stealing from the web apps:

**Write to the outbox before the network.** Every mutation is queued
locally first, then sent, then dropped from the queue. Closing the tab
mid-write loses nothing and the op replays on next load. The screen shows
the server snapshot with queued ops replayed on top, so an edit is visible
instantly and stays visible if the network is not there.

**A patch value of `null` deletes the key.** Both shipped contracts close
their objects with `additionalProperties: false`, so clearing an optional
field means removing it, not setting it to null. Doing that in the local
patch representation keeps the distinction from ever reaching the wire.

### Retries

The protocol has no idempotency key, so retrying a `POST` whose bytes
already left the process risks a second item: the core may have created
the first and lost only the answer. `erisdb-client`'s rule is the right
one — repeat only what is provably harmless. Reads, always. Failures that
happened before the request went out (a dead cached connection, a stream
that would not open), always. A write whose response was lost, never.

## 4. Refresh at half-life

Read the token's `exp` yourself. The payload is the middle dot-separated
segment, unpadded base64url of JSON:

```js
function tokenExp(token) {
  try {
    const b64 = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    const exp = JSON.parse(atob(b64)).exp;
    return typeof exp === "number" ? exp : null;   // null: never expires
  } catch { return null; }
}
```

The signature is the core's business; the client only needs the clock.

Remember the lifetime you started with — the token's remaining life when
it was first configured is the lifetime the operator chose. Once less than
half of it remains, `POST /v1/capabilities/refresh` with that same
`ttl_secs`, and replace the stored token with the one that comes back. One
refresh in flight at a time; a failure just waits for the next sync.

Two answers to handle:

- **201** — swap the token. The fresh `exp` rides inside it, and the
  response also names `exp` and `chain_ends` outright. If `exp` came back
  shorter than you asked for, the chain is running out and a human will
  need to re-mint.
- **401** — the token is past its own expiry or past its chain. Refresh
  cannot resurrect a session. Stop, say so plainly, and ask for a new
  token or a new pairing.

A token with no `exp` never expires and must never be refreshed — the
trade would swap an unbounded token for a bounded one. `if (exp === null)
return` is the whole guard.

An open stream keeps the old token it was authorized with and picks up the
fresh one when it reconnects. It will be closed by the core when the old
token's `exp` passes, which is a reconnect, not a failure.

Details of what refresh does and does not carry:
[capabilities.md](capabilities.md#what-refresh-does-and-does-not-do).

## `X-Bezel-Client`

A header naming what wrote this. The core copies it verbatim into
`source.client` on the item and on every change row.

```
X-Bezel-Client: Tasks (Web) v0.1
```

It is the weakest rung of the trust gradient — the caller says it and
nothing checks it — and that is fine, because it answers a question
nothing else can. `source.addr` tells you which machine and `source.user`
tells you which identity, but only `client` tells you *which build*, which
is what you want at two in the morning when one item in a thousand has a
malformed body.

So: include the version, and make it the same string the release is
tagged with. Every subproject in this tree does — a crate's `Cargo.toml`
version, its tag, and the string it stamps into `X-Bezel-Client` are one
string, so a `source` names the exact build that wrote it.

Bounded at 128 characters, printable, no control characters. Over that, or
outside ASCII, is a 400 — it lands in every row this caller writes, so an
unbounded one is a way to grow the table.

## Worked examples

Four clients, four different shapes, all against the same API.

- **`apps/tasks/index.html`** and **`apps/lists/index.html`** — the
  fullest examples, and each is one file you can read end to end.
  Offline-first outbox, cursor sync over `fetch`, SSE read off a
  `ReadableStream` (so the `Authorization` header can be set — `EventSource`
  cannot carry one), refresh at half-life, and facet self-registration on
  connect for an app that was granted `meta:facets:write` and degrades
  politely when it was not. Tasks also drives its UI off
  `GET /v1/permissions`, and takes due notifications from two sources:
  `lapsed` rows from the poker's sweep, and a local due-check between
  ticks, deduped.

- **`erisdb-client/`** — the Rust client, dialing over Iroh: one QUIC
  connection, one HTTP/1.1 exchange per bi-stream, ALPN `bezel/0`. Read it
  for `subscribe_changes` (the cursor is the caller's, every event carries
  its `seq`), for the retry rule above, and for `identity` — passing the
  same 32 bytes every launch is what makes a device one device across
  restarts rather than a new stranger each time, since `source.addr` is
  the endpoint id the core observed. It also carries the blocking facade
  and the JNI surface the two Android apps sit on.

- **`erisdb-mcp/`** — the API as MCP tools over stdio. Worth reading for
  what it *refuses*: `update_item` and `revert_item` make `revision`
  required, because a model that renders a partial view and writes it back
  would otherwise destroy every field it did not echo, and with the
  revision required the worst case is a 409 telling it to read again.
  `mint_capability` is off unless an env switch turns it on, because a
  minted token would land in the transcript. Both are bounds a capability
  token cannot express, held on the client side instead.

- **`poker/poke.sh`** — the smallest possible client: two lines of curl,
  one URL, one token, no state.

## A checklist

- Seed with `GET /v1/items`, sync with `seq`, persist the cursor with the
  cache.
- Poll on a timer; stream for latency; treat every stream end as ordinary.
- Send whole bodies with the revision you hold; treat 409 as "read again".
- Queue writes before touching the network.
- Refresh at half-life; stop on 401; never refresh a token with no `exp`.
- Send `X-Bezel-Client` with your version in it.
- Handle 429 (back off) and 503 on the stream route (all slots taken —
  retry, or fall back to polling).
- Ask for the permissions you use and no more; read `GET /v1/permissions`
  and draw the UI from what you actually hold.
- Register your facet on connect *if* you were granted
  `meta:facets:write`, and treat 409 and 403 alike: someone else
  registered it, or you were not given that grant. Neither is fatal — say
  who to ask and carry on.
