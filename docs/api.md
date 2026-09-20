# The v1 HTTP API

One version, one prefix: everything lives under `/v1`. The same router is
served over plain TCP and over Iroh QUIC, so every route below behaves
identically on both paths — the transport changes what `source.addr` says
and nothing else.

Bodies are JSON. Core responses are JSON, except `204 No Content` and SSE
streams. A plugin response has the status, content type, allowed headers and
streaming body its one-shot executable supplies. Every route except
`/v1/health` needs a capability token:

```
Authorization: Bearer bz1.<b64url(payload)>.<b64url(sig)>
```

Every request may also carry `X-Bezel-Client`, a self-declared name that
lands in the write's attribution. See [clients.md](clients.md).

Each route names the **permission** it requires — one concrete permission
string the core computes from the request. A token's grants either cover it
or they do not. [permissions.md](permissions.md) is the authority on the
grammar and on what covers what; this page names the strings.

Every request and response below was captured from a running core at
`erisdb 0.1.0`, edited only to line-wrap. Ids and timestamps are real ones
from that session, which is why they are not all pretty.

## Contents

- [Objects](#objects) — the item and change envelopes
- [Permissions by route](#permissions-by-route)
- [Errors](#errors) — the envelope and every code
- [`GET /v1/health`](#get-v1health)
- [`GET /v1/plugins`](#get-v1plugins)
- [`POST /v1/call`](#post-v1call)
- [`POST /v1/items`](#post-v1items)
- [`GET /v1/items`](#get-v1items)
- [`GET /v1/items/{id}`](#get-v1itemsid)
- [`PUT /v1/items/{id}`](#put-v1itemsid)
- [`DELETE /v1/items/{id}`](#delete-v1itemsid)
- [`GET /v1/items/{id}/history`](#get-v1itemsidhistory)
- [`POST /v1/items/{id}/revert`](#post-v1itemsidrevert)
- [`GET /v1/changes`](#get-v1changes)
- [`GET /v1/changes/stream`](#get-v1changesstream)
- [`POST /v1/tick`](#post-v1tick)
- [`POST /v1/capabilities`](#post-v1capabilities)
- [`POST /v1/capabilities/refresh`](#post-v1capabilitiesrefresh)
- [`GET /v1/permissions`](#get-v1permissions)
- [`GET /v1/server`](#get-v1server)
- [Pairing](#pairing) — the six routes of the two-phase flow
- [Limits](#limits)

## Objects

### The item envelope

Everything the store holds is an item. The envelope is the core's;
`body` is the facet's.

| field | type | meaning |
|-------|------|---------|
| `id` | UUID v4 | Assigned by the core at create. |
| `facet` | string | The contract this body answers to, and the permission namespace it lives in. Fixed at create; no route changes it. |
| `body` | object | The facet's payload. Validated against the facet's schema when the facet is strict. |
| `revision` | integer | Starts at 1, increments on every body write. The token of optimistic concurrency. |
| `created_at` | RFC 3339 | Server clock at create. |
| `updated_at` | RFC 3339 | Server clock at the last body write. |
| `source` | object or null | Who wrote it last: `{addr, user, client}`. Null only on rows a migration created. |

`source` has a trust gradient, strongest first: `addr` is observed from
the connection and cannot be forged (`ip:port` over TCP,
`iroh:<endpoint id>` over QUIC); `user` is signed into the capability;
`client` is whatever the caller put in `X-Bezel-Client`. Any of the three
may be null.

A body is stored as `jsonb` and comes back the way Postgres holds it:
duplicate keys collapse to the last one, and key order is not preserved.
Neither matters to a JSON object, but it does mean the bytes you sent are
not the bytes you get.

### The change envelope

| field | type | meaning |
|-------|------|---------|
| `seq` | integer | Monotonic, in commit order, assigned by the store. The cursor. |
| `item_id` | UUID or null | Null only for `tick`, which is about no item. |
| `facet` | string | The item's facet, or `system` for `tick`. What `?facet=` filters on. |
| `op` | string | `created`, `updated`, `deleted`, `tick`, `lapsed`. |
| `at` | RFC 3339 | Commit time. |
| `body` | object or null | The body this change produced. Null for `deleted` and `tick`. |
| `revision` | integer or null | The revision this change produced. Null wherever `body` is. |
| `source` | object or null | Who caused it. |

A `created`, `updated` or `lapsed` row carries the item's full body, so a
client can apply the feed to its cache without ever refetching an item.
See [change-feed.md](change-feed.md).

## Permissions by route

`{facet}` is the facet the request names, or — for the by-id routes — the
facet of the item that id belongs to.

| route | permission |
|-------|------------|
| `GET /v1/health` | none; no token either |
| `GET /v1/plugins` | none beyond a valid token; results are filtered by held grants |
| `POST /v1/call` | the selected operation's manifest permission |
| `POST /v1/items` | `{facet}:create` |
| `GET /v1/items` | `{facet}:read` |
| `GET /v1/items/{id}` | `{facet}:read` |
| `PUT /v1/items/{id}` | `{facet}:update` |
| `DELETE /v1/items/{id}` | `{facet}:delete` |
| `GET /v1/items/{id}/history` | `{facet}:read` |
| `POST /v1/items/{id}/revert` | `{facet}:update` |
| `GET /v1/changes`, `GET /v1/changes/stream` | `{facet}:read` with `?facet=`, `meta:feed:read` without |
| `POST /v1/tick` | `meta:system:tick` |
| `POST /v1/capabilities` | `meta:capabilities:mint`, plus enclosure |
| `POST /v1/capabilities/refresh` | none beyond a valid token |
| `GET /v1/permissions` | none beyond a valid token |
| `GET /v1/server` | `meta:server:read` |
| `POST /v1/pairings` | `meta:pairing:create` |
| `GET /v1/clients`, `GET /v1/clients/{id}` | `meta:clients:read` |
| `PUT /v1/clients/{id}` | `meta:clients:write`, plus enclosure |
| `POST /v1/clients/{id}/revoke` | `meta:clients:revoke` |
| `POST /v1/clients/{id}/refresh` | Installation proof; no bearer required |
| `GET /v1/pairings`, `GET /v1/pairings/{id}` | `meta:pairing:read` |
| `POST /v1/pairings/{id}/approve`, `/deny` | `meta:pairing:approve`, plus enclosure on approve |
| `POST /v1/pair/redeem`, `GET /v1/pair/status` | `meta:pairing:redeem`, on a token naming a session |

The core's own three facets do not follow the `{facet}:{action}` rule,
because they are core operations and there is exactly one name for each:

| facet | action | permission |
|-------|--------|------------|
| `facet` | `read` | `meta:facets:read` |
| `facet` | `create`, `update`, `delete` | `meta:facets:write` |
| `system` | `read` | `meta:system:read` |
| `pair` | `read` | `meta:pairing:read` |
| `pair` | `create` | `meta:pairing:create` |
| `pair` | `update`, `delete` | `meta:pairing:approve` |

Two consequences worth stating plainly. `meta:facets:read` lists every
contract in the store but grants nothing over the data those contracts
describe. And `system` and `pair` are not writable as items at all: a
`POST`, `PUT` or `DELETE` naming either is refused with a 400 naming the
endpoints that own them, whatever the token holds. Reading a session as an
item is fine and reveals nothing, because a session never holds a token —
the token is minted when the client collects it.

## Errors

Every error the core itself raises comes back as:

```json
{ "error": "forbidden", "detail": "capability does not grant tasks:create" }
```

`error` is a stable code — branch on it. `detail` is prose for a human and
may change. A `forbidden` detail names the exact permission the request
wanted, which is the fastest way to find out what to ask for.

| code | status | what raises it |
|------|--------|----------------|
| `unauthorized` | 401 | No `Authorization` header, no `Bearer ` prefix, a token that is not three dot-separated parts, a wrong prefix (anything but `bz1`), payload or signature that is not base64url, a signature that does not verify, a payload that is not a capability, `exp` at or before now, or the chain end at or before now. Also: a refresh whose computed expiry is not in the future, and a pairing route reached with a token that holds `meta:pairing:redeem` but names no session. Every refusal is logged with the path and the peer. |
| `forbidden` | 403 | No grant covers the permission the route requires. Also: minting or approving a capability the caller's own does not enclose, in scope *or* in time. |
| `not_found` | 404 | No item with that id; for history, no change rows for that id; for a pairing route, no session with that id. |
| `unknown_facet` | 422 | The write names a facet with no registration in the `facet` meta-facet. |
| `schema_violation` | 422 | The body fails the facet's JSON Schema. `detail` carries the validator's own message. |
| `plugin_not_found` | 404 | The named plugin is not installed, or it has no operation with that name. |
| `plugin_schema_violation` | 422 | `input` fails the selected operation's manifest JSON Schema. |
| `plugin_unavailable` | 503 | A required allowlisted environment variable is absent for the selected plugin. The variable name is logged, not returned. |
| `plugin_failed` | 502 | The executable could not start, timed out before its response header, or violated the process protocol. The actual failure and bounded stderr go to the operator log. Once a valid response header has been sent, a later failure truncates/errors the response stream because its HTTP status is already committed. |
| `revision_conflict` | 409 | The `revision` supplied is not the item's current revision. Update, revert, and delete-with-revision. |
| `conflict` | 409 | A unique-index violation, mapped so it is not a 500 — in practice, registering a facet name that already exists, with `detail` `conflict: that value already exists`. Also the pairing state machine: `conflict: this pairing code is already approved`, `conflict: only a redeemed pairing code can be approved`. |
| `bad_request` | 400 | A `$ref` pointing outside the document, or one whose value is not a string, in a schema being registered; a facet name that is reserved or not a single lowercase segment; a registered schema that does not compile, raised on the first write to that facet; `X-Bezel-Client` that is non-ASCII, longer than 128, or contains a control character; a grant that is not a permission, an empty grant set, more than 64 grants, a grant over 128 characters, a `user` over 128; `ttl_secs` not positive; `max_ttl_secs` over the 31536000-second ceiling; a lifetime that overflows i64 seconds; a `revert` `seq` that is not a body snapshot of this item; a `client` on redeem that is empty or over 128 characters; a pairing code that has expired. |
| `too_many_requests` | 429 | The token bucket on `/v1/capabilities` and `/v1/capabilities/refresh` is empty. |
| `unavailable` | 503 | 32 change streams are already open, or 32 plugin processes are already running on this replica. |
| `internal` | 500 | A store failure or an internal one. `detail` is `the store failed; see the server log` or `internal failure; see the server log` — the real message goes to the log, because it carries constraint names, column names and query fragments. |

### Rejections that are not in the envelope

A request that never reaches a handler is refused by the framework's own
extractors, and those answer in **plain text** rather than `{error,
detail}`. A client that parses every failure as JSON breaks on these:

| what | status | body |
|------|--------|------|
| Missing or wrong `Content-Type` on a route that takes a body | 415 | ``Expected request with `Content-Type: application/json` `` |
| A body that is not valid JSON | 400 | `Failed to parse the request body as JSON: …` |
| Valid JSON missing a required field, or with a wrong type | 422 | ``Failed to deserialize the JSON body into the target type: missing field `revision` …`` |
| A query string that does not deserialize | 400 | ``Failed to deserialize query string: missing field `facet` `` |
| A path segment that is not a UUID | 400 | ``Invalid URL: Cannot parse `id` with value `xyz` …`` |
| A body over 2 MiB (`/v1/call`: over 16 MiB) | 413 | `Failed to buffer the request body: length limit exceeded` |

The token is checked before any of these, because the capability extractor
runs first: an unauthenticated request with a malformed body is a 401 in
the ordinary envelope.

## `GET /v1/health`

No capability, no token. Liveness, not readiness: it answers from the
process and does not touch the store.

```
GET /v1/health
```

```json
{ "ok": true }
```

**200** always.

## `GET /v1/plugins`

Describe installed one-shot plugin operations this capability can invoke.
The answer comes entirely from deployment manifests and capability grants;
the route does not touch Postgres. Operations not covered by the caller's
token are omitted rather than returned as unusable entries.

**Permission:** none beyond a valid token.

```json
{
  "plugins": [{
    "name": "openai",
    "description": "OpenAI API calls through a fresh process per request",
    "operations": [{
      "name": "chat.completions",
      "description": "Create or stream an OpenAI Chat Completion",
      "permission": "openai:chat",
      "request_schema": {
        "type": "object",
        "required": ["model", "messages"],
        "additionalProperties": true
      }
    }]
  }]
}
```

**200** with `plugins`, possibly empty.

## `POST /v1/call`

Validate and invoke one plugin operation. This is a direct exchange, not a
durable job: it starts one fresh executable, writes one invocation to stdin,
streams stdout into this response, and exits. It never reads or writes
Postgres and the core never retries it.

**Permission:** the operation's manifest permission; `openai:chat` for the
shipped operation.

```json
{
  "plugin": "openai",
  "operation": "chat.completions",
  "input": {
    "model": "gpt-5.4",
    "messages": [{"role": "user", "content": "hello"}],
    "tools": [{
      "type": "function",
      "function": {
        "name": "lookup",
        "parameters": {"type": "object"}
      }
    }],
    "stream": true
  }
}
```

The response is whatever the operation emits. The OpenAI executable forwards
Chat Completions JSON, errors, tool-call objects and `text/event-stream`
chunks without rewriting their bodies. The core adds `Cache-Control:
no-store` and removes unsafe or hop-by-hop headers.

**Any valid HTTP status** may be supplied by a correctly running plugin. A
failure before that response begins uses the core error envelope documented
above. The request limit is 16 MiB, the manifest timeout is at most one hour,
and dropping the client response kills the process.

The manifest and stdin/stdout protocol are specified in
[plugins.md](plugins.md).

## `POST /v1/items`

Create an item. Registering a facet is this route with `"facet": "facet"`
— there is no separate registration endpoint and no deploy.

**Permission:** `{facet}:create`.

```http
POST /v1/items HTTP/1.1
Authorization: Bearer bz1.eyJncmFudHMiOlsidGFza3M6Y3JlYXRlIl0s…
X-Bezel-Client: Tasks (Web) v0.1
Content-Type: application/json
```

```json
{
  "facet": "tasks",
  "body": { "title": "water the plants", "done": false }
}
```

**201 Created**, the whole item:

```json
{
  "id": "7fe22cf5-9928-40f2-be3d-6168f66f5fc8",
  "facet": "tasks",
  "body": { "done": false, "title": "water the plants" },
  "revision": 1,
  "created_at": "2026-08-23T23:52:13.245452Z",
  "updated_at": "2026-08-23T23:52:13.245452Z",
  "source": {
    "addr": "127.0.0.1:59294",
    "client": "Tasks (Web) v0.1",
    "user": "alice"
  }
}
```

The order of checks matters, because it decides which error you get:

1. `{facet}:create`, or **403**. This runs before anything looks at the
   store, so a token for the wrong facet learns nothing about whether that
   facet exists.
2. If the facet is `facet`, the submitted `schema` is walked for external
   `$ref`s, or **400**.
3. The facet's registration is loaded, or **422 `unknown_facet`**.
4. If the facet is strict, the body is validated, or **422
   `schema_violation`**.
5. If the facet is `facet`, the submitted `name` is checked as a permission
   namespace, or **400**. This is deliberately *after* the schema, so a
   registration with no name at all is a schema violation rather than a
   naming complaint.
6. Insert, and append the `created` change row in the same transaction.

**409 `conflict`** if the insert violates a unique index — the only one is
the facet-name index, so this means "a facet by that name is already
registered".

## `GET /v1/items`

Every item in one facet.

**Permission:** `{facet}:read`.

| param | required | default | meaning |
|-------|----------|---------|---------|
| `facet` | yes | — | Exactly one facet. There is no cross-facet list. Omitting it is a plain-text 400 from the query extractor. |
| `updated_since` | no | — | RFC 3339. Returns items with `updated_at` **strictly greater** than this. Anything that is not a timestamp is a plain-text 400. |
| `limit` | no | `100` | Clamped to `1..=1000`. Out-of-range values are clamped, not refused; a non-number is a plain-text 400. |

Ordered by `updated_at`, then `id`. The id is a tiebreaker so that items
sharing a timestamp come back in one fixed order rather than whatever the
planner felt like.

The registration is never consulted, so listing a facet nobody has
registered is `{"items": []}` and a 200, not a 422.

```
GET /v1/items?facet=tasks&limit=2
```

```json
{
  "items": [
    {
      "id": "7fe22cf5-9928-40f2-be3d-6168f66f5fc8",
      "facet": "tasks",
      "body": { "done": false, "title": "water the plants" },
      "revision": 1,
      "created_at": "2026-08-23T23:52:13.245452Z",
      "updated_at": "2026-08-23T23:52:13.245452Z",
      "source": { "addr": "127.0.0.1:59294", "client": "Tasks (Web) v0.1", "user": "alice" }
    },
    {
      "id": "1f05c80f-a57c-4af6-8524-ea5032e2bc68",
      "facet": "tasks",
      "body": { "done": false, "due": "2026-09-01T00:00:00Z", "title": "renew the domain" },
      "revision": 1,
      "created_at": "2026-08-23T23:52:22.238888Z",
      "updated_at": "2026-08-23T23:52:22.238888Z",
      "source": { "addr": "127.0.0.1:47840", "client": "Tasks (Web) v0.1", "user": "alice" }
    }
  ]
}
```

`updated_since` is a bulk-load filter, not a sync cursor. It is a
timestamp with strict `>`, so a group of items sharing one `updated_at`
cannot be split across pages: page past part of the group and the rest is
skipped forever. Use it to seed a cache and then hold a `seq` cursor on
[the change feed](change-feed.md), which is what every app in this tree
does.

## `GET /v1/items/{id}`

One item, by id.

**Permission:** `{facet}:read` — which the core knows only after it has
fetched the row.

```
GET /v1/items/7fe22cf5-9928-40f2-be3d-6168f66f5fc8
```

**200** with the item envelope, unwrapped.

**404** if no such id. **403** if the id exists and no grant covers its
facet. That ordering is deliberate: answering 403 tells a caller something
true about an id it already holds, and item ids are v4 UUIDs, so it is not
a way to discover one.

## `PUT /v1/items/{id}`

Replace the body. **Whole-body, not a patch** — whatever you send is what
the item becomes, so a caller that renders a partial view and writes it
back destroys every field it did not echo.

**Permission:** `{facet}:update`.

```json
{
  "body": { "title": "water the plants", "done": true },
  "revision": 1
}
```

`revision` is required and is the revision the caller believes is
current. The UPDATE is `WHERE id = … AND revision = …`; zero rows affected
is **409 `revision_conflict`**.

**200** with the item at its new revision:

```json
{
  "id": "7fe22cf5-9928-40f2-be3d-6168f66f5fc8",
  "facet": "tasks",
  "body": { "done": true, "title": "water the plants" },
  "revision": 2,
  "created_at": "2026-08-23T23:52:13.245452Z",
  "updated_at": "2026-08-23T23:52:22.266840Z",
  "source": { "addr": "127.0.0.1:47850", "client": "Tasks (Web) v0.1", "user": "alice" }
}
```

**404** if the id is unknown; the row is looked up and locked before the
permission check, so an unknown id is 404 whatever the token says. **422**
if the new body fails the facet's schema — updates are validated exactly as
creates are, and against the registration as it stands now, so a facet
whose registration has since been deleted answers **422 `unknown_facet`**.
`facet`, `id` and `created_at` are not writable.

## `DELETE /v1/items/{id}`

**Permission:** `{facet}:delete`.

| param | required | meaning |
|-------|----------|---------|
| `revision` | no | The revision the caller believes is current. |

Pass `revision` and a delete racing someone else's edit is **409
`revision_conflict`** instead of a silent win. Omit it and the delete is
unconditional.

```
DELETE /v1/items/7fe22cf5-9928-40f2-be3d-6168f66f5fc8?revision=2
```

**204 No Content**, no body. **404** for an unknown id.

The row leaves `items`. It does not leave `changes`: a `deleted` row is
appended with a null `body` — the state after a delete is absence — and
every prior snapshot stays exactly where it was. Deleting is not
forgetting; see [change-feed.md](change-feed.md).

## `GET /v1/items/{id}/history`

Every state the item has ever been in, oldest first.

**Permission:** `{facet}:read`, where the facet is read off the item's
*first* change row. This works for items that no longer exist: history
outlives its item.

```
GET /v1/items/7fe22cf5-9928-40f2-be3d-6168f66f5fc8/history
```

```json
{
  "history": [
    {
      "seq": 2,
      "op": "created",
      "at": "2026-08-23T23:52:13.245452Z",
      "body": { "done": false, "title": "water the plants" },
      "revision": 1,
      "source": { "addr": "127.0.0.1:59294", "client": "Tasks (Web) v0.1", "user": "alice" }
    },
    {
      "seq": 4,
      "op": "updated",
      "at": "2026-08-23T23:52:22.266840Z",
      "body": { "done": true, "title": "water the plants" },
      "revision": 2,
      "source": { "addr": "127.0.0.1:47850", "client": "Tasks (Web) v0.1", "user": "alice" }
    }
  ]
}
```

Rows are the change envelope minus `item_id` and `facet`, both of which
are constant across the response.

**404** when the item has no change rows at all. That is not quite the same
as "no such item", and it catches two real ones: the bootstrap `facet`
registration and the bootstrap `pair` registration, both inserted by
migrations, never written through the API, and so without a history despite
being perfectly real items.

## `POST /v1/items/{id}/revert`

Git-revert, not time travel. The body snapshotted at `seq` is written as a
**new** revision and lands on the feed as an ordinary `updated`. History
never rewinds.

**Permission:** `{facet}:update`. Reverting is editing; there is no
separate revert permission, and a token that may change an item may change
it to something it already was.

```json
{ "seq": 2, "revision": 2 }
```

- `seq` — the change whose body to restore. It must be a row for *this*
  item and it must carry a body, or **400 `bad_request`** with
  `no snapshot at seq N for this item`. A `deleted` row has a null body,
  so it is never a revert target.
- `revision` — optimistic concurrency, exactly as on `PUT`. Stale is
  **409**.

**200** with the item at its new revision: reverting to seq 2 while the
item stands at revision 2 returns revision 3, carrying the body that
revision 1 held.

The restored body is re-validated against the facet's *current* schema, so
a snapshot taken before the schema tightened is **422** rather than a way
around it.

**404** if the item does not currently exist. Reverting is not undelete:
recovering a deleted item means reading its history and creating a new one
from the last snapshot. It will get a new id.

## `GET /v1/changes`

The feed, cursor-paged. This is the bus and the audit log both.

**Permission:** `{facet}:read` when `facet` is given; **`meta:feed:read`**
when it is not. The unfiltered feed crosses every facet, so it is its own
permission rather than the sum of the ones it would reveal — which is why
`*:read` cannot read it, and why a token scoped to one facet can tail that
facet and nothing else.

| param | required | default | meaning |
|-------|----------|---------|---------|
| `since` | no | `0` | Exclusive: rows with `seq > since`. `0` is the beginning of time. |
| `facet` | no | — | One facet. Omit for the global feed. |
| `limit` | no | `500` | Clamped to `1..=5000`. |

```
GET /v1/changes?since=1&facet=tasks
```

```json
{
  "changes": [
    {
      "seq": 2,
      "item_id": "7fe22cf5-9928-40f2-be3d-6168f66f5fc8",
      "facet": "tasks",
      "op": "created",
      "at": "2026-08-23T23:52:13.245452Z",
      "body": { "done": false, "title": "water the plants" },
      "revision": 1,
      "source": { "addr": "127.0.0.1:59294", "client": "Tasks (Web) v0.1", "user": "alice" }
    },
    {
      "seq": 4,
      "item_id": "7fe22cf5-9928-40f2-be3d-6168f66f5fc8",
      "facet": "tasks",
      "op": "updated",
      "at": "2026-08-23T23:52:22.266840Z",
      "body": { "done": true, "title": "water the plants" },
      "revision": 2,
      "source": { "addr": "127.0.0.1:47850", "client": "Tasks (Web) v0.1", "user": "alice" }
    }
  ],
  "next": 4
}
```

`next` is the last `seq` in the page, or `since` unchanged when the page
is empty. Pass it back as `since`. It is a page cursor, not a high-water
mark of the whole feed: an empty page means you are caught up *for this
filter*.

Filtering by facet skips `tick`, which is filed under `system`. It does
not skip `lapsed`, which carries the item's own facet.

## `GET /v1/changes/stream`

The same rows, pushed. Server-Sent Events over one long-lived response,
woken by Postgres `NOTIFY` rather than polled.

**Permission:** identical to `GET /v1/changes`, and checked once, at
subscribe.

| param | required | default | meaning |
|-------|----------|---------|---------|
| `since` | no | `0` | Exclusive, as above. The stream first drains everything after `since`, then goes live — so no window is missed between catching up and subscribing. |
| `facet` | no | — | One facet, or the global feed. |

`limit` is accepted by the parser and ignored; the stream always drains in
batches of 500.

Every event is named `change` and its data is one change object, the same
shape `GET /v1/changes` returns:

```
event: change
data: {"seq":2,"item_id":"7fe22cf5-9928-40f2-be3d-6168f66f5fc8","facet":"tasks","op":"created","at":"2026-08-23T23:52:13.245452Z","body":{"done":false,"title":"water the plants"},"revision":1,"source":{"addr":"127.0.0.1:59294","client":"Tasks (Web) v0.1","user":"alice"}}

:

event: change
data: {"seq":4,…}
```

Bare `:` lines are keep-alive comments. There is no `id:` field — the
cursor is `seq` inside the data, held by the client, so `Last-Event-ID`
plays no part.

**503 `unavailable`** when 32 streams are already open: each one holds a
Postgres connection of its own, outside the pool.

The stream **ends when the subscribing token expires**. Authorization is
checked once, so without that a token with a minute left would hold an
open firehose for as long as the process ran. The response simply closes;
there is no final event. A client that reconnects with a refreshed token
and its last `seq` loses nothing.

A stream also ends on any store error. Treat every end as ordinary, catch
up with `GET /v1/changes?since=<cursor>`, and reconnect.

## `POST /v1/tick`

The poker's endpoint: put a tick on the bus, then sweep every facet that
declares a lapse rule.

**Permission:** `meta:system:tick`. It is the poker's whole job, and the
only thing that permission is for.

Takes no body, and needs no `Content-Type`: the handler never reads one.

```http
POST /v1/tick HTTP/1.1
Authorization: Bearer bz1.eyJncmFudHMiOlsibWV0YTpzeXN0ZW06dGljayJdLCJleHAiOm51bGx9.<sig>
```

```json
{ "seq": 7, "lapsed": 1 }
```

`seq` is the tick's own change row: `{item_id: null, facet: "system", op:
"tick", body: null, revision: null}`. `lapsed` counts the `lapsed` rows
this call appended.

The sweep is idempotent by construction. The tick's change row takes the
feed's append lock first and holds it for the whole transaction, so
overlapping pokes run one at a time and the second sees the first's rows
and finds nothing to do. An item lapses at most once per edit; see
[facets.md](facets.md) for the rule that decides.

**200** always when authorized — a sweep that finds nothing reports
`"lapsed": 0`, which is the normal answer most minutes.

## `POST /v1/capabilities`

Mint a narrower token from the one you hold. Delegation, not issuance:
there is no route that creates authority out of nothing. That needs the
secret and the CLI.

**Permission:** `meta:capabilities:mint`, plus enclosure of everything
requested.

| field | required | default | meaning |
|-------|----------|---------|---------|
| `grants` | yes | — | 1 to 64 permission patterns, each at most 128 characters. Each must be segments of `[a-z0-9][a-z0-9._-]*` or `*`, joined by `:`. The core does not check that a namespace exists. |
| `ttl_secs` | yes | — | Seconds from now. Must be positive. There is no HTTP spelling for a token that never expires. |
| `max_ttl_secs` | no | 2592000 (30 days) | How long the minted token may keep refreshing. Raised to `ttl_secs` if shorter, and refused over 31536000 (365 days). |
| `user` | no | — | A signed identity, at most 128 characters, stamped into `source.user` on every write the token makes. Attribution, never privilege — enclosure ignores it entirely. |

```json
{
  "grants": ["tasks:read", "tasks:create", "tasks:update"],
  "ttl_secs": 604800,
  "user": "agent-1"
}
```

**201 Created**:

```json
{
  "token": "bz1.eyJncmFudHMiOlsidGFza3M6cmVhZCIsInRhc2tzOmNyZWF0ZSIsInRhc2tzOnVwZGF0ZSJdLCJleHAiOjE3ODgxMzM5NTcsInVzZXIiOiJhZ2VudC0xIiwibWF4X2V4cCI6MTc5MDEyMTEzM30.GsaBWfSbx22BiXYSmrfg8CfrOGWtEbQlQ93eUKlNmiU"
}
```

The response is only the token. Decode the payload yourself if you want
its `exp` — the middle segment is unpadded base64url of the JSON
`{grants, exp, user?, max_exp?, pair?}` — or ask
[`GET /v1/permissions`](#get-v1permissions) while holding it. It is
signed, not encrypted.

Refusals, in the order they happen:

- **429** — the rate limit, checked *before* authorization. A caller
  without `meta:capabilities:mint` that grinds this route sees 429 rather
  than 403, which is confusing exactly once.
- **403** — no grant covers `meta:capabilities:mint`.
- **400** — the grant set is empty, oversized, or misspelt; `ttl_secs` is
  not positive; `max_ttl_secs` is over the ceiling.
- **403** — the caller's own capability does not enclose the request.
  Enclosure is scope *and* time: a token that dies in a minute cannot mint
  one that lives an hour, and a token holding `tasks:read` cannot mint
  `tasks:*`.

Leaving `max_ttl_secs` out never costs a refusal — an unrequested chain
silently takes whatever is left of the parent's. Naming one explicitly is
taken at face value, so asking for more than the parent has is a 403 and
not a surprise. [capabilities.md](capabilities.md) has the rules in full.

## `POST /v1/capabilities/refresh`

Trade a still-valid token for one with the same grants and a later expiry.

**Permission:** none beyond a valid token. This is how an app outlives its
TTL without a human re-minting.

```json
{ "ttl_secs": 86400 }
```

**201 Created**:

```json
{
  "token": "bz1.eyJncmFudHMiOlsidGFza3M6cmVhZCJdLCJleHAiOjE3ODc2MTU1NTcsIm1heF9leHAiOjE3ODc2MTU1NTd9.Akj5fRpXLEQ8PP6LUgOJOO9noUUoLjOdx55heGojExY",
  "exp": 1787615557,
  "chain_ends": 1787615557
}
```

`exp` is the new token's expiry in unix seconds; `chain_ends` is the end
of its refresh chain — the ceiling that does not move. When the requested
TTL reaches past the chain, `exp` is clamped to `chain_ends` and the
request still succeeds, so a client asking for a day and being given the
four hours it has left gets a 201 and can read what it actually got. The
response above is exactly that case: a token with a day of chain left,
asked for a day, given the chain.

Grants and the signed `user` carry over untouched. Refresh moves time, not
privilege: a token without `meta:capabilities:mint` cannot refresh itself
into one that has it, and a refreshed token cannot outlive the chain of the
token it came from — refreshing repeatedly does not walk the ceiling
forward.

- **429** — the rate limit, shared with `POST /v1/capabilities`.
- **400** — `ttl_secs` is not positive.
- **401** — the presenting token is already expired or past its chain
  (verification refuses it before the handler runs), or the computed
  expiry is not in the future. Refresh keeps a session alive; it cannot
  resurrect one.

A token minted with no expiry has no chain to run out. Refreshing it
returns a *bounded* token — the trade only ever narrows — so a daemon
holding a `--no-expiry` token should not call this route.

## `GET /v1/permissions`

What this token is. **No permission**: a caller may always ask what it
already holds, and the answer tells it nothing it could not learn by
decoding its own token.

```
GET /v1/permissions
```

```json
{
  "grants": ["*"],
  "exp": 1787532733,
  "max_exp": 1790121133,
  "user": "alice"
}
```

`exp`, `max_exp` and `user` are null when unset — a `--no-expiry` token
reports `"exp": null`. This is the route a dashboard uses to decide which
buttons to draw, and a client uses to check what a pairing actually
granted.

## `GET /v1/server`

Counts and bounds, for a dashboard or a health check with more to say than
`ok`.

**Permission:** `meta:server:read`. It is deliberately not covered by
`*:read`: how much is in the store and how loaded the process is are
facts about the deployment, not about any facet's data.

```json
{
  "version": "0.1.0",
  "feed_head": 5,
  "facets": 3,
  "items": 5,
  "changes": 5,
  "limits": {
    "streams": 32,
    "streams_free": 32,
    "iroh_connections": 64,
    "client_name": 128
  }
}
```

`version` is the core's crate version. `feed_head` is the highest `seq` in
`changes`, which is what a client compares its cursor against to know how
far behind it is. `streams_free` is this replica's remaining stream slots,
so it moves; the other limits are constants compiled in.

## Pairing

Seven routes and one state machine. The client half (`/v1/pair/*`) is
reached with the pairing code; the operator half (`/v1/pairings*`) with an
ordinary token holding the `meta:pairing:*` permissions.

```
pending  ── redeem ──▶  requested  ── approve ──▶  approved
                                   ── deny ─────▶  denied
```

A session is an ordinary item in the `pair` facet, so it survives a
restart, is visible from every replica, and lands on the change feed —
which is how a dashboard sees a request arrive live. [pairing.md](pairing.md) is
the authority on the ticket format and on why the flow has two phases.

### `POST /v1/pairings`

Cut a code.

**Permission:** `meta:pairing:create`.

The body is optional. `ttl_secs` defaults to 600 and is clamped to
`60..=3600` — asking for 5 gets 60, silently.

```json
{ "ttl_secs": 600 }
```

**201 Created**:

```json
{
  "id": "6e10cf80-8142-488b-afc5-9ecab365247e",
  "expires": 1787529757,
  "secret": "bz1.eyJncmFudHMiOlsibWV0YTpwYWlyaW5nOnJlZGVlbSJdLCJleHAiOjE3ODc1Mjk3NTcsIm1heF9leHAiOjE3ODc1Mjk3NTcsInBhaXIiOiI2ZTEwY2Y4MC04MTQyLTQ4OGItYWZjNS05ZWNhYjM2NTI0N2UifQ.Gci71EteM_kVnSHgXFy1tx5xdG1wif--30q3_xgKOAk"
}
```

`secret` is the code that goes in the QR. Decoded, it is
`{"grants":["meta:pairing:redeem"],"exp":…,"max_exp":…,"pair":"6e10cf80-…"}`
— a token that can do exactly one thing to exactly one session. It reads
no data and writes none; presenting it to `GET /v1/items` is a 403.
`expires` is the session's deadline in unix seconds, and the code's too.

### `POST /v1/pair/redeem`

The native identity is taken from the authenticated Iroh connection. HTTP clients
must include `challenge` (an unpadded base64url SHA-256 commitment to a fresh
installation secret) in the JSON body. The returned session contains `identity`
and `fingerprint`. HTTP collection and renewal prove the secret in
`X-ErisDB-Client-Proof`; redemption never sends the secret itself. See
[registered installations](clients.md) for transport and credential details.

Say who you are and what you want.

**Permission:** `meta:pairing:redeem`, on a token that names a session.

| field | required | meaning |
|-------|----------|---------|
| `client` | yes | 1 to 128 characters. Shown to the human, trusted for nothing. |
| `requested` | yes | The permissions the client would like, validated as grants. Asking is free. |

```json
{
  "client": "Tasks (Android) v0.3",
  "requested": ["tasks:read", "tasks:create", "tasks:update"]
}
```

**200** with the session, token field removed:

```json
{
  "id": "f17df84d-fde1-4d38-8797-d83afd574a4c",
  "revision": 2,
  "created_at": "2026-08-23T23:50:38.694082Z",
  "body": {
    "status": "requested",
    "client": "Tasks (Android) v0.3",
    "requested": ["tasks:read", "tasks:create", "tasks:update"],
    "expires": 1787529638
  }
}
```

**403** if the token does not hold `meta:pairing:redeem`. **401** if it
holds it but names no session — an ordinary `*` token cannot redeem
anything, which is what keeps the code the only key to its own session.
**400** if the session has expired. **409** if it is not `pending`: a code
is redeemed once, and a denied one cannot be retried for a better answer.

### `GET /v1/pair/status`

Requires the pairing capability and the same installation proof used at redemption:
the authenticated Iroh key, or `X-ErisDB-Client-Proof` for HTTP clients. A different
key or missing/wrong HTTP proof returns 401, even if the QR ticket is valid.

Before approval: `200 {status: "requested", fingerprint: "12AB-34CD-56EF"}`.
After approval: `200 {status: "approved", granted: [...], token: "bz1.…",
client_id: "UUID", exp: 1234567890, fingerprint: "12AB-34CD-56EF"}`.

Collection creates the client and marks the pairing collected in one transaction.
It is repeatable by that same installation while the ticket remains live, so a
lost response does not lose the enrollment. No token is persisted in the pairing
or audit log. A revoked registration cannot collect again. Requested/denied
states contain no access credential. Responses have `Cache-Control: no-store`.

### `GET /v1/pairings`

Every session, newest first, capped at 100.

**Permission:** `meta:pairing:read`.

```json
{
  "pairings": [
    {
      "id": "f17df84d-fde1-4d38-8797-d83afd574a4c",
      "revision": 2,
      "created_at": "2026-08-23T23:50:38.694082Z",
      "body": {
        "status": "requested",
        "client": "Tasks (Android) v0.3",
        "requested": ["tasks:read", "tasks:create", "tasks:update"],
        "expires": 1787529638
      }
    },
    {
      "id": "499616f3-7a27-46d6-b7aa-381314fcc1de",
      "revision": 1,
      "created_at": "2026-08-23T23:50:38.686887Z",
      "body": { "status": "pending", "expires": 1787529638 }
    }
  ]
}
```

The `token` field is stripped from every body. It is not stripped by the
generic item routes, which reach the same rows under the same permission —
see [Permissions by route](#permissions-by-route).

### `GET /v1/pairings/{id}`

One session, same shape, same redaction. **404** for an id that is not a
`pair` item.

**Permission:** `meta:pairing:read`.

### `POST /v1/pairings/{id}/approve`

Answer yes, to all of it or some of it.

**Permission:** `meta:pairing:approve`, plus enclosure — approving is
minting, so nobody grants what they do not hold.

The body is optional. Omit it entirely to approve exactly what was asked
for, with default lifetimes.

| field | required | default | meaning |
|-------|----------|---------|---------|
| `granted` | no | whatever was `requested` | What to actually grant. Must be the requested set or a subset, also enclosed by the approver. |
| `ttl_secs` | no | 604800 (7 days) | Access-token lifetime, 1–604800 seconds. |
| `max_ttl_secs` | no | no deadline | Optional installation lifetime, 1–31536000 seconds. |
| `user` | no | — | The signed identity the paired client writes as. |

```json
{ "granted": ["tasks:read", "tasks:create"], "ttl_secs": 604800 }
```

**200** with the session, token still redacted — the token belongs to the
client that redeemed the code, and the approver never sees it:

```json
{
  "id": "f17df84d-fde1-4d38-8797-d83afd574a4c",
  "revision": 3,
  "created_at": "2026-08-23T23:50:38.694082Z",
  "body": {
    "status": "approved",
    "client": "Tasks (Android) v0.3",
    "requested": ["tasks:read", "tasks:create", "tasks:update"],
    "granted": ["tasks:read", "tasks:create"],
    "expires": 1787529638
  }
}
```

**409** unless the session is `requested`: there is nothing to approve
before a client has asked. **400** if the session has expired, or a
lifetime is out of range. **403** if the approval is not enclosed by the
approver's own capability — an operator holding only `tasks:*` cannot
grant `lists:read`, whatever the client asked for, and cannot answer a
request for `*` at all.

### `POST /v1/pairings/{id}/deny`

Answer no. Takes no body.

**Permission:** `meta:pairing:approve`.

**200** with the session at `"status": "denied"`, with `token` and
`granted` removed. Unlike approve, deny does not check the session's state
or its expiry: a session can be denied whenever, including after it was
approved, which cancels an approval the client has not collected yet. It
returns 409 after collection; use `POST /v1/clients/{id}/revoke` to revoke the registration.

## Registered installations

The complete registry API, revision semantics, renewal and revocation contract
are documented in [clients.md](clients.md#administration).

## Limits

| bound | value | why |
|-------|-------|-----|
| Live change streams | 32 | Each holds a Postgres connection outside the pool, so this bounds the store as much as the process. Over it is 503. |
| Iroh connections | 64 | Bounded before authentication: the endpoint id is public by design, and a capability check costs more than a refusal. Over it, the connection is dropped. |
| Iroh streams per connection | 32 | Over it, the stream is dropped. |
| Capability endpoints | burst of 10, then 1 per 5 seconds | A token bucket per caller, keyed by observed address. Minting is rare for an honest client and attractive to grind on. Soft state: a restart forgives everyone, and replicas do not share buckets. |
| `X-Bezel-Client` | 128 characters, printable ASCII | It is copied into every change row this caller writes, so an unbounded one is a way to grow the table. |
| Grants per token | 64, each ≤ 128 chars | Attacker-controlled strings baked into a payload that is HMAC'd on every request. |
| `user` per token | 128 characters | Same reason. |
| Refresh chain | 31536000 seconds over HTTP | Applies to manual/delegated refresh chains; registered installation renewal is independently revocable. |
| Pairing code lifetime | 60 to 3600 seconds, default 600 | Long enough to walk to the other device, short enough that a photographed screen goes stale. |
| Request body | 2 MiB | The framework default. Over it is 413. |
| Store connection pool | 16 | Per replica. |

On the plain-TCP path the rate-limit key is the peer's `ip:port`, so a
caller that opens a fresh connection per request gets a fresh bucket and
the limit is close to meaningless there — one more reason that listener
belongs on loopback. Over Iroh the key is the remote endpoint id, a
cryptographic identity, and the bucket means what it says.
