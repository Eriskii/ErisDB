# Building a client

The browser apps, Android apps, MCP client, and poker call the server API.
Apps are let in by a device of their owner's mesh and call over Iroh by their
own keys; the poker uses a manually minted token. [api.md](api.md) documents the
server routes, [the mesh](mesh.md) how apps get in, and [clients.md](clients.md)
their installations.

Agents building clients can use the repository's
[client-development skill](../.agents/skills/erisdb-client-development/SKILL.md),
which routes to the relevant contracts and implementation examples.

## 1. Get in through a device

An app holds one Iroh key for as long as it is the same app; generate it once
and keep it where the platform protects secrets. That key is its credential.

1. Ask the device's ErisAuth to approve the key for service `erisdb`, not
   serving, with the grants you want: over the agent socket
   (`$XDG_RUNTIME_DIR/erisauth/agent.sock`, one JSON line, see
   [ErisOverlayNetwork's agent](https://github.com/Eriskii/ErisOverlayNetwork))
   on a desktop; through ErisAuth's bound service on Android; by redeeming a
   device's `eris://pair/` ticket where no ErisAuth runs beside the app.
2. The person approves on the device, possibly fewer grants than asked, and
   types ErisDB's passphrase. ErisAuth enrolls the app at the mesh's ErisDB
   and only then answers.
3. The answer is the app's note and a snapshot of the mesh. The mesh's ErisDB is
   the app the snapshot shows approved to serve `erisdb` — the most recently
   approved, should there be more than one; save its address with the key.
4. Dial that endpoint id with ALPN `erisdb/0`, as the key, and send requests
   with no `Authorization` header.

In Rust, `erisdb_client::sign_in(key, name, grants)` is all four steps, and
`Client::dial` the last one on later launches. Android apps get the snapshot
from ErisAuth's service and hand it to the JNI surface's `nativeLocate`, then
`nativeDial`; see the [client library](../erisdb-client/README.md). A denial is
an answer, not an error; no ErisAuth at all means the app has no way in on this
machine.

Two rules for the manifest, both about the human at the other end:

- **Ask for what you use, and no more.** The request is a list a person
  reads and answers item by item. A client that asks for `*` because it is
  easier is asking to be denied.
- **Initialize only your own namespace.** `tasks:create` allows registering
  the missing `tasks` schema as well as creating tasks. It does not allow
  registering other namespaces or altering existing schemas. Ordinary apps
  do not need `meta:facets:write`. A read-only client skips initialization.

Once in, `GET /v1/permissions` tells you exactly what you got:

```json
{ "grants": ["tasks:read", "tasks:create"], "exp": null,
  "max_exp": null, "user": null,
  "client_id": "ad6caf6b-ece2-442a-a178-a93cc5aa0802" }
```

Use these grants to control the UI. Re-read them during sync, because the
operator can change them. The server checks current authority on every request;
cached permissions only help the UI.

Store the key and ErisDB's address together. The key is the whole credential:
do not log it, and keep it out of anything that leaves the device. Where it
lives depends on the platform, and none of the options are good in an absolute
sense: the Android Keystore, a mode-0600 file for a daemon, whatever a browser
can hold. Pick the one the platform actually enforces, and ask for grants narrow
enough that the loss is bounded.

A 401 means the app is not, or no longer, let in: its installation was revoked,
its device removed from the mesh, or its note revoked. Nothing it can do here
gets it back in; it asks a device again.

## 2. Seed a cache, then hold a cursor

For an application facet, reconstruct its data by reading the change feed from
sequence zero. This uses the existing API and includes records whose timestamps
are identical:

```text
1. Start with an empty cache and cursor = 0.
2. GET /v1/changes?since=<cursor>&facet=X&limit=500
3. Apply the returned changes in sequence order.
4. Persist the resulting cache and response.next together.
5. Repeat from step 2 until the response contains no changes.
6. Continue polling from the saved cursor, or subscribe from that cursor.
```

URL-encode `X`. Each facet needs its own cache and cursor; do not reuse a cursor
from a different filter. If loading a saved cache, use its saved cursor instead
of zero. Replaying old history can take time because the feed is not pruned.

Apply each row by `item_id`:

- `created` / `updated`: replace the cached body and revision with the snapshot
  in the row, retaining the item ID and facet.
- `deleted`: remove the item, even if it is already absent.
- `lapsed`: carries the unchanged body and revision; it does not represent an
  item edit. Suppress historical due notifications during initial replay.
- Rows without an item ID, including ticks and installation audit events on a
  global feed, do not update the item cache.

The feed contains full item bodies and revisions, but does not return the entire
[Item envelope](api.md#item). For example, a lapse event's `at` is the event time,
not the item's `updated_at`. Fetch an item by ID when its exact current envelope
metadata is needed. Built-in facet definitions are inserted during database
initialization and should be read through `GET /v1/items?facet=facet`.

Persist the cache and cursor in one transaction or atomic file replacement.
Advancing a cursor without saving the corresponding changes loses data. If both
polling and streaming feed the cache, serialize their application, keep sequence
order, and ignore already-processed rows. Keep unsent local edits separately so
applying server changes does not discard them.

If that saved snapshot is missing, unreadable, or belongs to another core, start
initialization again. Never combine an empty cache with a previously advanced
cursor: unchanged server records would remain invisible. Keep the items and
their cursor together in memory too, including when several screens can sync.

`GET /v1/items` provides a current snapshot with a maximum of 1000 rows per call.
Its `updated_since` filter uses strict `>` and has no ID continuation: if a page
ends inside a group with the same timestamp, advancing to that timestamp skips
the remaining records. The Android demos currently use this snapshot approach;
the browser demos load one item page. Neither is a complete initialization
recipe for arbitrary data sizes. Use the feed replay above when completeness
is required. See [item listing](api.md#get-v1items) and
[change pagination](api.md#get-v1changes).

### Polling and streaming

Polling alone is sufficient for sync. SSE adds lower latency and accepts the
same `since` cursor; reconnect with the last saved cursor after a disconnect.
The server sends `event: change` with a JSON change row, plus keepalive comments.
It does not send SSE event IDs or read `Last-Event-ID`. A browser calling over
HTTP with a token uses streaming `fetch` to set `Authorization`, since native
`EventSource` cannot set that header; the browser demos read the feed over Iroh
with [ErisDB's browser client](../clients/web/README.md).

A stream closes on lost permission, revocation, the mesh ending the app, token
expiry for a token holder, or a transport or backend failure. Catch up and
reconnect with backoff; a 401 on reconnecting means the app is out.
If no stream slot is available, opening one returns 503; poll while waiting.
The demos combine periodic polling with SSE in the browser; Android polls.
See the [stream API](api.md#get-v1changesstream) for the response format.

## 3. Write with revisions

`PUT /v1/items/{id}` replaces the whole body. Read the item, overlay your intended
changes on its full body, and send the result with the revision you read.
A matching revision checks concurrent edits only: omitted optional fields are
removed even when the revision is correct. Required omissions instead fail schema
validation. See the [update example](api.md#put-v1itemsid).

Updates and reverts require the current `revision`; a stale one is a 409. That is the
concurrency model in full: last-writer-wins is not available, and a 409 is
not an error condition so much as a message that says *read again and
retry*. Handle it by refetching the item (or waiting for the feed to bring
you the newer version) and reapplying your intent, not by resending the
same body with a bumped number.

`DELETE` takes an optional `revision`. Pass it if you have one — a delete
racing someone else's edit becomes a 409 instead of silently winning.

Two conventions worth stealing from the web apps:

**Persist the outbox before sending.** Keep local edits until their outcome is
known, and show them over the server cache. Check that local persistence succeeded;
a full browser storage quota can prevent a durable save. The demos queue writes,
but their outboxes do not resolve ambiguous create outcomes: a lost response can
lead to a repeated create and a duplicate item. Account for that limitation when
adapting their code.

**The demos use `null` in local patches to remove a key.** This is client-side
editing logic, not an HTTP PATCH endpoint. Their optional top-level fields do
not allow null values, so clearing one removes it before sending the complete
body. `additionalProperties: false` rejects unknown keys; whether a known field
allows null depends on that field's schema.

### Retries

The protocol has no idempotency key or client-chosen item IDs. If a create request
may have reached the server but its response was lost, do not blindly resend it:
it may already have created an item. Retain the uncertain operation for
reconciliation rather than promising exactly-once delivery.

Reads can be retried. The SDK also retries failures it can identify as occurring
before transmission. Enrollment is safe to repeat: the same note gives the same
installation the same grants.

## 4. Tokens: refresh at half-life

Apps calling by their keys hold no tokens and have nothing to renew: an
installation lasts until it is revoked or the mesh ends it. This section is for
token holders — daemons, plugins, and whatever an app delegates to with
`POST /v1/capabilities`.

Manual tokens use `POST /v1/capabilities/refresh` and remain bounded by their
expiry and refresh-chain deadline.

Read the token's `exp` yourself. The payload is the middle dot-separated
segment, unpadded base64url of JSON:

```js
function tokenExp(token) {
  try {
    const b64 = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    const exp = JSON.parse(atob(b64)).exp;
    if (exp === null || (Number.isSafeInteger(exp) && exp > 0)) return exp;
  } catch { /* Invalid token metadata. */ }
  throw new Error("Token has no valid expiry metadata");
}
```

The signature is the core's business; the client only needs the clock.

Track the token's remaining lifetime when configured, or the lifetime returned
by a successful refresh. This is a scheduling estimate; tokens contain no issue
timestamp from which to recover their original lifetime. Once less than half
of the tracked lifetime remains, `POST /v1/capabilities/refresh` with that same
`ttl_secs`, and replace the stored token with the one that comes back. One
refresh in flight at a time; a failure just waits for the next sync.

Two answers to handle:

- **201** — swap the token. The fresh `exp` rides inside it, and the
  response also names `exp` and `chain_ends` outright. If `exp` came back
  shorter than you asked for, the chain is running out and a human will
  need to re-mint.
- **401** — the token is past its own expiry or past its chain, or the
  installation that minted it no longer counts. Refresh cannot resurrect a
  session. Stop, say so plainly, and ask for a new token.

A token with `exp: null` never expires and does not need refreshing — the
trade would swap an unbounded token for a bounded one. `if (exp === null)
return` is the whole guard.

An open stream keeps the old token it was authorized with and picks up the
fresh one when it reconnects. It will be closed by the core when the old
token's `exp` passes, which is a reconnect, not a failure.

Details of what refresh does and does not carry:
[capabilities.md](capabilities.md#what-refresh-does-and-does-not-do).

## `X-ErisDB-Client`

Send an optional display label in this header:

```http
X-ErisDB-Client: Tasks (Web) v0.1
```

The server copies it to `source.client` on writes. The label is supplied by the
caller and does not authenticate an app or prove its version. It is limited to
128 printable ASCII characters; invalid values return 400.

`source.addr` records the observed TCP peer or Iroh endpoint ID: for an app
calling by its key, that key. Behind a local reverse proxy it identifies the
proxy. `source.user` is a signed attribution label, and `source.installation`
identifies the installation when there is one. See [item source fields](api.md#item).

## Worked examples

Four clients, four different shapes, all against the same API.

- **`apps/tasks/index.html`** and **`apps/lists/index.html`** — the
  fullest examples of a cache and an outbox: offline-first writes, cursor
  sync, SSE, and initialization of their own missing schema with their create
  permission. Setup is retried before queued creations are sent. Tasks also
  drives its UI off `GET /v1/permissions`, and takes due notifications from two
  sources: `lapsed` rows from the poker's sweep, and a local due-check between
  ticks, deduped.

- **`erisdb-client/`** — the Rust client, dialing over Iroh: one QUIC
  connection, one HTTP/1.1 exchange per bi-stream, ALPN `erisdb/0`. Read it
  for `sign_in` (getting in through the device's ErisAuth in one call), for
  `subscribe_changes` (the cursor is the caller's, every event carries its
  `seq`), and for the retry rule above. The key passed to `dial` is the app:
  the same 32 bytes every launch is what keeps it the installation a device let
  in. It also carries the blocking facade and the JNI surface for Android apps,
  and `enroll`, ErisAuth's side of letting an app in.

- **`clients/mcp/`** — a separate client exposing MCP tools over stdio, signed
  in through ErisAuth. `update_item` and `revert_item` require the same
  revisions as the server API. Updates still replace the whole body; callers
  must preserve fields they want to keep. `mint_capability` is disabled unless
  explicitly enabled because its result exposes a credential to the MCP host.

- **`poker/poke.sh`** — the smallest possible client: two lines of curl,
  one URL, one token, no state.

## A checklist

- Initialize application data through the change feed from `since=0`; persist
  each processed cursor together with its cache.
- Poll on a timer; optionally stream for latency and reconnect from the cursor.
- Send whole bodies with the revision you hold; treat 409 as "read again".
- Persist writes before sending; reconcile uncertain outcomes before retrying.
- Keep the app's key; a 401 means the app is out and must ask a device again.
  Tokens refresh at half-life within their bounded chain.
- Send `X-ErisDB-Client` with your version in it.
- Handle 429 (back off) and 503 on the stream route (all slots taken —
  retry, or fall back to polling).
- Ask for the permissions you use and no more; read `GET /v1/permissions`
  and draw the UI from what you actually hold.
- Initialize your missing facet with its own `NAME:create` grant. On 409, keep
  the existing schema; never overwrite it. Inspect it if you have
  `meta:facets:read`, and handle validation failures when writing items.
  Retain queued creations if initialization fails; retry after reconnecting.
