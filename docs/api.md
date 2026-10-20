# ErisDB server API

The server exposes the HTTP routes below over TCP and authenticated Iroh.
Routes are implemented in [api.rs](../erisdb/src/api.rs), with installation
authorization in [installation.rs](../erisdb/src/installation.rs) and the
passphrase in [passphrase.rs](../erisdb/src/passphrase.rs).
Examples use illustrative IDs, timestamps and credentials; obtain those values
from your running server.

## Contents

- [Conventions and authentication](#conventions-and-authentication)
- [Example setup](#example-setup)
- [Objects](#objects)
- [Permissions by route](#permissions-by-route)
- [Errors](#errors)
- [Health and server state](#health-and-server-state)
- [Items and facets](#items-and-facets)
- [Change feed and ticks](#change-feed-and-ticks)
- [Capabilities and permissions](#capabilities-and-permissions)
- [Enrollment](#enrollment)
- [Installations](#installations)
- [Plugins](#plugins)
- [Operator CLI](#operator-cli)
- [Limits](#limits)

## Conventions and authentication

The core serves `/v1` on plain HTTP, default `127.0.0.1:7700`, and on HTTP/1.1
streams inside authenticated Iroh QUIC using ALPN `erisdb/0`. The routes and
payloads are shared; the transport decides which credentials a request can carry.

| Credential | Used for | How it is sent |
|---|---|---|
| An enrolled app's key | Routes its installation's grants allow | Nothing in the request: over Iroh, with no `Authorization` header, the peer's key the QUIC handshake proved |
| A device of the mesh | `POST /v1/enroll` and `POST /v1/passphrase` | The device calling over Iroh as itself |
| Operator, minted or delegated token | Routes its grants allow | `Authorization: Bearer erisdb1.…`, over TCP or Iroh |

Only the health route is unauthenticated. A request with a bearer token is
authorized by the token; one without, over Iroh, by the installation of its key;
one without, over TCP, by nothing (401).

For an app calling by its key, Postgres supplies its installation's status and
grants, and the core's copy of the mesh's device list supplies whether its device
is still a member and its note still unrevoked. A token delegated by an
installation names it, and its effective grants are the intersection of its own
and the installation's current grants. Missing, expired or revoked authority
yields 401; insufficient grants yield 403. Operator and minted tokens have no
installation and keep their bounded refresh-chain semantics.

Use HTTPS remotely, with the core behind a **local** TLS reverse proxy; HTTP is
supported on localhost. Bearer tokens on TCP are readable by anything on the
path.

Bodies use `Content-Type: application/json`. Responses are JSON except 204,
SSE, framework rejections, and plugin-defined responses. Core responses carry
`Cache-Control: no-store`. Do not follow redirects with credentials. CORS is
permissive; possession of authority, not the browser origin, authorizes requests.
Extra fields in top-level request envelopes are ignored except for
`POST /v1/call`, `POST /v1/enroll` and `POST /v1/passphrase`, which reject them.
Item bodies follow their own facet schema. The router also supplies HEAD for GET
routes and unauthenticated CORS OPTIONS preflights. These framework methods are
implicit, not additional application operations in the route table.

`X-ErisDB-Client` is an optional printable-ASCII attribution label, at most 128
bytes. It authenticates nothing. There are no cookies, query-string tokens,
HTTP PATCH operations, item upsert routes, bulk routes, or server-side search
route.

## Example setup

Shell examples use Bash, curl and jq. Point these variables at a
running development core. The creation examples write real data; use a facet
whose schema matches the example body.

```bash
BASE=http://127.0.0.1:7700
# On the operator's machine, with ERISDB_SECRET already supplied privately:
TOKEN=$(erisdb mint --grant '*' --ttl 3600)

api() {
  local method=$1 path=$2
  shift 2
  curl --fail-with-body --silent --show-error \
    --request "$method" "$BASE$path" \
    --header "Authorization: Bearer $TOKEN" \
    --header 'X-ErisDB-Client: API examples' \
    --header 'Content-Type: application/json' "$@"
}
```

`TOKEN` is the acting caller's token, not the server's signing secret. Apps
call by their enrolled keys over Iroh instead, through
[erisdb-client](../erisdb-client/README.md); the shell examples use an operator
token over TCP. Examples below assign `ITEM_ID`, `ITEM_REV`, `SNAPSHOT_SEQ`, and
`CLIENT_ID` from previous responses. If using a section independently, supply
its variables.
The delete example is a separate final operation; keep the item alive while
trying update, history and revert.

## Objects

### Item

An item is the durable envelope around a facet-defined JSON body. Registrations
in `clients` are separate objects, not generic items.

```json
{
  "id": "a871b3bb-a77b-4fbd-a289-6f7d04e03c3a",
  "facet": "notes",
  "body": {"title": "First note", "done": false},
  "revision": 1,
  "created_at": "2026-09-21T12:00:00Z",
  "updated_at": "2026-09-21T12:00:00Z",
  "source": {"addr": "127.0.0.1:54321", "user": null, "client": "API examples", "installation": null}
}
```

| Field | Type and meaning |
|---|---|
| `id` | Server-assigned UUID v4; immutable |
| `facet` | Registered namespace; immutable through item updates |
| `body` | JSON value checked against the facet's schema when strict; commonly an object, but the API itself accepts any JSON value |
| `revision` | Signed 64-bit integer, initially 1; increments on body updates |
| `created_at`, `updated_at` | RFC 3339 timestamps |
| `source` | JSON attribution or null for initialized facet definitions |

Normal request attribution has `addr` (observed TCP peer or `iroh:ENDPOINT_ID`),
`user` (signed token label), `client` (self-declared header), and `installation`
(the installation the caller's key or token belongs to). Individual values may
be null. An installation the mesh revoked records `{"mesh": "device removed"}` or
`{"mesh": "note revoked"}` as the source of its audit event.

Postgres stores bodies as JSONB: key ordering is not preserved and duplicate
object keys collapse. Timestamps and revisions are server-owned.

### Change and history row

```json
{
  "seq": 42,
  "item_id": "a871b3bb-a77b-4fbd-a289-6f7d04e03c3a",
  "facet": "notes",
  "op": "created",
  "at": "2026-09-21T12:00:00Z",
  "body": {"title": "First note", "done": false},
  "revision": 1,
  "source": {"addr": "127.0.0.1:54321", "user": null, "client": "API examples", "installation": null}
}
```

| Field | Meaning |
|---|---|
| `seq` | Monotonically increasing committed-feed cursor; gaps are allowed |
| `item_id` | Item UUID, or null for ticks and installation audit events |
| `facet` | Item namespace, or `system` for ticks/installation events |
| `op` | `created`, `updated`, `deleted`, `tick`, or `lapsed` |
| `at` | Database transaction timestamp, not a substitute for `seq` ordering |
| `body` | Snapshot after this change; null for deletes and ticks |
| `revision` | Snapshot revision, or null when no snapshot exists |
| `source` | Attribution as above |

History rows contain `seq`, `op`, `at`, `body`, `revision`, and `source`; their
item ID/facet are implicit in the history request. Installation audit events use
`facet: "system"`, `item_id: null`, and body `{"event":"client","client":…}`.
Treat them as registry events, not item updates. Deletion leaves prior snapshots
in history; it does not erase audit records.

### Installation

Installation objects have their own complete schema under
[Installations](#installations).

## Permissions by route

A grant is a colon-separated pattern, for example `notes:read`, `notes:*`,
`*:read`, or `*`. Up to 64 nonempty grants are accepted, each at most 128 bytes.
Segments are `*` or lowercase `[a-z0-9][a-z0-9._-]*`. The `meta` namespace is
closed: `*:read` covers user facets, not `meta:clients:read` or other administration.
See [permission matching](permissions.md) for enclosure and wildcard rules.

| Method | Route | Required authority |
|---|---|---|
| GET | `/v1/health` | None |
| GET | `/v1/server` | `meta:server:read` |
| GET | `/v1/permissions` | Any caller with authority: an enrolled key or a valid token |
| POST | `/v1/items` | `{facet}:create` |
| GET | `/v1/items` | `{facet}:read` |
| GET | `/v1/items/{id}` | `{facet}:read` |
| PUT | `/v1/items/{id}` | `{facet}:update` |
| DELETE | `/v1/items/{id}` | `{facet}:delete` |
| GET | `/v1/items/{id}/history` | `{facet}:read` |
| POST | `/v1/items/{id}/revert` | `{facet}:update` |
| GET | `/v1/changes` | `{facet}:read` when filtered; otherwise `meta:feed:read` |
| GET | `/v1/changes/stream` | Same feed authority |
| POST | `/v1/tick` | `meta:system:tick` |
| POST | `/v1/capabilities` | `meta:capabilities:mint`, plus grant/lifetime enclosure |
| POST | `/v1/capabilities/refresh` | A valid bearer token |
| POST | `/v1/enroll` | A device of the mesh over Iroh, its own note, and the passphrase |
| POST | `/v1/passphrase` | A device of the mesh over Iroh, and the current passphrase |
| GET | `/v1/clients` | `meta:clients:read` |
| GET | `/v1/clients/{id}` | `meta:clients:read` |
| PUT | `/v1/clients/{id}` | `meta:clients:write`, plus approved/acting grant enclosure |
| POST | `/v1/clients/{id}/revoke` | `meta:clients:revoke` |
| GET | `/v1/plugins` | Any caller with authority; filters operations by effective grants |
| POST | `/v1/call` | The selected operation's concrete permission |

For the core's facets, `facet` reads require `meta:facets:read`. Creating a new
registration named `NAME` requires `NAME:create` or `meta:facets:write`; changing,
reverting, or deleting a registration requires `meta:facets:write`. `system`
reads require `meta:system:read`, and generic writes to `system` are refused: the
core writes its own events. A client registry UUID is not an item ID.

## Errors

Core-handler errors use the HTTP status plus this JSON envelope:

```json
{"error":"forbidden","detail":"capability does not grant notes:create"}
```

Branch on the status and `error`; `detail` is human-readable and may change.

| HTTP | `error` | Cause |
|---|---|---|
| 400 | `bad_request` | Invalid grants, unsupported lifetime, invalid client label, short new passphrase, refresh without a bearer token, forbidden schema reference, or invalid snapshot |
| 400 | `bad_note` | An enrollment's note does not read, does not count in the mesh, or does not approve an app to call `erisdb` |
| 401 | `unauthorized` | Missing/invalid/expired token, an unenrolled key, or an installation that is revoked or no longer counts in the mesh |
| 403 | `forbidden` | Missing permission or grants/lifetime outside the caller's authority |
| 403 | `not_a_device` | Enrollment or a passphrase change from anything but a device of the mesh calling as itself over Iroh, or a device presenting another device's note |
| 403 | `wrong_passphrase` | The passphrase was wrong; counted against the calling device. `retry_after` (seconds) is present when this guess locked the device out |
| 404 | `not_found` | Missing item, history, or administratively requested installation |
| 404 | `plugin_not_found` | Unknown plugin or operation |
| 409 | `revision_conflict` | Stale item or installation revision |
| 409 | `conflict` | Duplicate facet name, or a change to a revoked installation |
| 422 | `unknown_facet` | Write to an unregistered facet |
| 422 | `schema_violation` | Item body violates the facet schema |
| 422 | `plugin_schema_violation` | Plugin input violates its manifest schema |
| 429 | `too_many_requests` | Shared mint/refresh bucket exhausted |
| 429 | `locked` | The calling device is locked out by wrong passphrases; `retry_after` and a `Retry-After` header say for how long |
| 503 | `unavailable` | Change-stream or plugin-process capacity reached |
| 503 | `no_passphrase` | Enrollment or a passphrase change before the operator set a passphrase |
| 503 | `not_joined` | Enrollment on a core running without a mesh |
| 503 | `not_the_erisdb` | Enrollment or a passphrase change at a core whose own approval no longer counts: the mesh approved another ErisDB, or this one's device left |
| 503 | `plugin_unavailable` | Required allowlisted plugin environment variable is absent |
| 502 | `plugin_failed` | Plugin startup, header protocol, or timeout failure before headers are committed |
| 500 | `internal` | Database/internal failure; details remain in operator logs |

Framework rejections can be plain text: malformed JSON (400), wrong/missing JSON
content type (415), missing/wrongly typed JSON fields (422), invalid UUID/query
(400), oversized body (413), unknown route (404), or unsupported method (405).
Authorization normally precedes handler body validation; the enrollment routes,
which authorize the calling device, read their body first. Plugin responses can
have their own body and status, including non-JSON errors.

There is no general idempotency-key API. Retry reads as appropriate; do not
blindly retry a write after a transport failure with an unknown outcome. An
explicit core authorization 401 occurs before the mutation. Enrollment is safe to
repeat: the same note gives the same installation the same grants. Revocation is
idempotent.

## Health and server state

### `GET /v1/health`

No authentication or parameters. **200** `{"ok":true}`. This is process
liveness, not a database connectivity probe.

```bash
curl --fail-with-body --silent --show-error "$BASE/v1/health"
```

### `GET /v1/server`

Requires `meta:server:read`. No parameters. **200**:

```json
{
  "version":"0.1.0","feed_head":42,"facets":4,"items":8,"changes":42,
  "limits":{"streams":32,"streams_free":32,"iroh_connections":64,"client_name":128}
}
```

```bash
api GET /v1/server
```

Counts include core items/history. `feed_head` is zero for an empty feed and is
not necessarily equal to `changes` (the row count). `streams_free` is per replica.

## Items and facets

### `POST /v1/items`

Body: required `facet: string` and `body: JSON`. Requires create authority on that
facet. **201** returns an [Item](#item). There is no client-chosen ID or upsert.

Initialize a missing namespace through the same route. This example needs
`notes:create` (or the broader `meta:facets:write`). A namespace grant permits
only its own initialization, never replacing or deleting an existing schema:

```bash
api POST /v1/items --data '{
  "facet":"facet",
  "body":{"name":"notes","version":1,"strict":true,
    "schema":{"type":"object","required":["title"],
      "properties":{"title":{"type":"string"},"done":{"type":"boolean"}},
      "additionalProperties":false}}
}'
```

A facet name is one lowercase permission segment, not `notes/v1` or `*`.
The built-in meta-facet accepts these registration body fields and rejects others:

| Field | Required / default | Meaning |
|---|---|---|
| `name` | Required | Unique namespace; reserved core names cannot be registered by an app |
| `schema` | Required object | JSON Schema for item bodies; `{}` accepts any JSON value |
| `strict` | Default true | Enforce the schema on writes |
| `version` | Optional integer ≥ 1 | Schema metadata; does not change the namespace or migrate existing items |
| `permissions` | Optional object | Action → human-readable description, for example `{"create":"Add notes"}` |
| `lapse` | Optional object | Required `due` field name, optional `done` field name; the tick sweep uses them |

Schemas must compile before a definition is saved; invalid schemas return 400
`bad_request`. Local `$ref` values are supported; external references are refused. Changing a
schema affects subsequent writes, not a retroactive rewrite of existing items.
See [facet schemas](facets.md) for schema and lapse examples.
If `notes` already exists, registration returns 409 and leaves its schema intact.
An app can proceed using that schema; 409 does not guarantee schema compatibility.
Reading definitions separately requires `meta:facets:read`. Schema validation
failures must be handled when writing data. A failed initialization should leave
pending user data queued for retry.

```bash
ITEM_JSON=$(api POST /v1/items --data \
  '{"facet":"notes","body":{"title":"First note","done":false}}')
ITEM_ID=$(jq -r '.id' <<<"$ITEM_JSON")
ITEM_REV=$(jq -r '.revision' <<<"$ITEM_JSON")
printf '%s\n' "$ITEM_JSON"
```

Missing registration yields 422 `unknown_facet`; schema mismatch yields 422
`schema_violation`. Creating `system` items through this route is refused.

### `GET /v1/items`

Requires read authority for the named facet. **200** `{"items":[ITEM,…]}`.

| Query | Required | Default / behavior |
|---|---|---|
| `facet` | Yes | Exactly one namespace |
| `updated_since` | No | RFC 3339; `updated_at` strictly greater than this timestamp |
| `limit` | No | Integer, default 100, clamped to 1–1000 |

```bash
api GET '/v1/items?facet=notes&limit=100'
api GET '/v1/items?facet=facet&limit=1000'
```

Rows are ordered by `(updated_at, id)`, ascending. An empty or unknown facet
returns an empty list if authorized. There is no offset or item-list cursor.
A timestamp-only continuation can miss ties; use the sequence-based change feed
from `since=0` for complete initialization of application data, as shown in the
[client guide](client-development.md#2-seed-a-cache-then-hold-a-cursor).
URL-encode query values; in
particular encode `+` in timezone offsets or use `Z` timestamps.

### `GET /v1/items/{id}`

Requires read authority on the item's facet. **200** Item; **404** if missing;
**403** if the item exists but the caller lacks its facet permission.

```bash
api GET "/v1/items/$ITEM_ID"
```

### `PUT /v1/items/{id}`

Requires update authority. Body: required `body: JSON`, `revision: integer`.
This replaces the **whole body**; omitted fields disappear. **200** returns the
updated Item with an incremented revision. A stale revision yields **409**.

```bash
CURRENT_ITEM=$(api GET "/v1/items/$ITEM_ID")
ITEM_JSON=$(api PUT "/v1/items/$ITEM_ID" --data \
  "$(jq -c '{revision,body:(.body + {title:"Edited note"})}' <<<"$CURRENT_ITEM")")
ITEM_REV=$(jq -r '.revision' <<<"$ITEM_JSON")
```

The facet stays fixed. A matching revision does not preserve omitted fields.
Read first, preserve the fields you intend to keep, and
handle 409 by reading the current value and resolving the concurrent edit.

### `DELETE /v1/items/{id}`

Requires delete authority. Optional integer query `revision` makes deletion
conditional. **204** has no body. Missing item yields **404**; a supplied stale
revision yields **409**. Without `revision`, the current item is deleted.

```bash
# Run after the history/revert examples if trying the full item lifecycle.
api DELETE "/v1/items/$ITEM_ID?revision=$ITEM_REV"
```

Prior bodies remain in history. Generic deletion cannot remove system records,
and it does not revoke an installation.

### `GET /v1/items/{id}/history`

Requires read authority on the original facet. No query parameters or pagination.
**200** `{"history":[HISTORY_ROW,…]}`, ordered by `seq`, oldest first. Works after
deletion; **404** means no history exists for that ID.

```bash
HISTORY_JSON=$(api GET "/v1/items/$ITEM_ID/history")
SNAPSHOT_SEQ=$(jq -r '.history | map(select(.body != null)) | first | .seq' <<<"$HISTORY_JSON")
printf '%s\n' "$HISTORY_JSON"
```

### `POST /v1/items/{id}/revert`

Requires update authority. Body: required `seq: integer`, `revision: integer`.
Writes the chosen historical body as a new revision; history never rewinds.
**200** Item. The item must still exist (404 otherwise). A sequence belonging
to another item or a null snapshot is **400**; stale revision is **409**. The
snapshot must satisfy the facet's **current** schema (422 otherwise).

```bash
ITEM_JSON=$(api POST "/v1/items/$ITEM_ID/revert" --data \
  "$(jq -nc --argjson seq "$SNAPSHOT_SEQ" --argjson revision "$ITEM_REV" \
    '{seq:$seq,revision:$revision}')")
ITEM_REV=$(jq -r '.revision' <<<"$ITEM_JSON")
```

## Change feed and ticks

### `GET /v1/changes`

With `facet`, requires read authority on that facet; without it requires
`meta:feed:read`. **200** `{"changes":[CHANGE,…],"next":42}`.

| Query | Default / behavior |
|---|---|
| `since` | Integer, default 0; only `seq > since` |
| `facet` | Optional single namespace; absent means global feed |
| `limit` | Integer, default 500, clamped to 1–5000 |

```bash
PAGE=$(api GET '/v1/changes?facet=notes&since=0&limit=500')
CURSOR=$(jq -r '.next' <<<"$PAGE")
api GET "/v1/changes?facet=notes&since=$CURSOR&limit=500"
```

`next` is the last returned sequence, or the supplied `since` when empty. Apply
rows in sequence order and persist the last processed cursor. Filtered feeds
have gaps because other facets share the sequence. Item deletion is a tombstone;
`lapsed` carries a snapshot without changing the item's revision.

### `GET /v1/changes/stream`

Same authorization and `since`/`facet` semantics. The query parser accepts
`limit`, but streaming ignores it and drains internal batches of 500.
**200** `Content-Type: text/event-stream`:

```text
event: change
data: {"seq":42,"item_id":"a871b3bb-a77b-4fbd-a289-6f7d04e03c3a","facet":"notes","op":"created","at":"2026-09-21T12:00:00Z","body":{"title":"First note","done":false},"revision":1,"source":null}

```

```bash
api GET '/v1/changes/stream?facet=notes&since=0' --no-buffer
```

The stream catches up, then waits for new changes. Keepalive comments can appear.
It sends no SSE `id` or `retry` field and does not consume `Last-Event-ID`; reconnect
with the last processed `seq` in `since`. Browser clients need a streaming fetch
that can set the bearer header (native EventSource cannot set it).

Expiry, loss of feed permission, revocation, the mesh ending the app, or backend
failure closes the stream; an already-started response cannot become a new HTTP
error. An installation's authority is rechecked before each event and at least
every second while idle, and registry notifications wake idle streams across
replicas. Capacity exhaustion before stream creation yields **503**. Already
delivered events are not retracted.

### `POST /v1/tick`

Requires `meta:system:tick`. Takes no body. **200** `{"seq":43,"lapsed":0}`.

```bash
api POST /v1/tick
```

`seq` identifies the tick row, not necessarily the final feed head. In one
transaction the core appends a system tick and emits `lapsed` changes for overdue,
unfinished items in facets declaring lapse rules. An item lapses at most once
per edit. Ticks do not themselves rewrite item bodies.

## Capabilities and permissions

### `POST /v1/capabilities`

Requires `meta:capabilities:mint`. Issues a token enclosed by the caller's
**effective** grants and token lifetime. **201** `{"token":"erisdb1.…"}`.

| JSON field | Required | Behavior |
|---|---|---|
| `grants` | Yes | 1–64 valid permission patterns; cannot exceed the caller |
| `ttl_secs` | Yes | Positive integer access lifetime |
| `max_ttl_secs` | No | Refresh-chain lifetime; defaults to 2592000 seconds (30 days), raised to at least `ttl_secs`; resulting chain cannot exceed 31536000 seconds |
| `user` | No | Signed attribution label, at most 128 bytes; does not add permissions |

```bash
CHILD_JSON=$(api POST /v1/capabilities --data \
  '{"grants":["notes:read"],"ttl_secs":300,"user":"report-reader"}')
CHILD_TOKEN=$(jq -r '.token' <<<"$CHILD_JSON")
```

An omitted chain is bounded to the parent's remaining authority; an explicitly
requested chain outside it is refused. Invalid input yields 400, failed enclosure
403, and rate limiting 429. The bearer token is authenticated before the rate
check; the mint permission is checked after it.

An app calling by its key mints too, when its grants include
`meta:capabilities:mint`: the child names its installation, so the
installation's current grants, its revocation and the mesh's removal of its
device or note still bound the child. An installation has no deadline of its
own, so the child's chain is bounded only by `max_ttl_secs` and its default.

Token format is `erisdb1.BASE64URL(JSON).BASE64URL(HMAC_SHA256)`, without base64
padding. The signed payload contains `grants` and optional `exp`, `max_exp`,
`user`, and `client` (installation UUID). Unix deadlines are seconds. Payloads
are signed, not encrypted; decoding is not verification.
See [capability semantics](capabilities.md) for the signing format.

### `POST /v1/capabilities/refresh`

Requires a still-valid bearer token, no extra grant; an app calling by its key
carries none and is refused with 400. Body: required positive integer
`ttl_secs`. **201**:

```json
{"token":"erisdb1.…","exp":1790000300,"chain_ends":1790003600}
```

```bash
curl --fail-with-body --silent --show-error \
  --request POST "$BASE/v1/capabilities/refresh" \
  --header "Authorization: Bearer $CHILD_TOKEN" \
  --header 'Content-Type: application/json' --data '{"ttl_secs":300}'
```

This is **bounded token refresh**. The new expiry is clamped to the existing
chain end; without `max_exp`, the old token's expiry is the ceiling. Effective
grants, signed user and installation carry forward. An expired token is 401.
Refreshing a non-expiring manual token produces a bounded token, not another
non-expiring one. The endpoint shares the mint rate limit.

### `GET /v1/permissions`

Requires an enrolled key or a valid token, no additional grant. No parameters.
**200**, for an app calling by its key:

```json
{
  "grants":["notes:read","notes:create"],
  "exp":null,"max_exp":null,"user":null,
  "client_id":"ad6caf6b-ece2-442a-a178-a93cc5aa0802"
}
```

```bash
api GET /v1/permissions
```

`exp`, `max_exp`, `user`, and `client_id` can be null; a key has no expiry. For a
token delegated by an installation, `grants` reflects live registry reductions
and may be narrower than the signed token's payload. Use this result to render
available actions. A revoked app, or one whose device left the mesh, gets 401
rather than a successful empty grant list.

## Enrollment

Apps get into ErisDB through a device of its owner's mesh; [the mesh](mesh.md)
describes the whole path. Both routes here are called by a **device**, over Iroh,
from its own endpoint: the caller's key must be a member of the mesh as ErisDB's
copy of the device list stands. Over TCP, or from any other key, they answer
403 `not_a_device`. The device syncs with ErisDB before calling, so ErisDB's list
knows it (`Node::connect` does so on its own). Both answer only while this core is
the mesh's ErisDB: once the mesh approves another, revoking this one's note, they
answer 503 `not_the_erisdb`.

Both carry the passphrase, and share its refusals: 403 `wrong_passphrase`,
counted against the calling device, with `retry_after` when that guess locked it
out; 429 `locked` with `retry_after` and `Retry-After` while it waits, checking
nothing; and 503 `no_passphrase` before the operator set one. The first three
wrong guesses in a row cost nothing; the third locks the device out for a minute,
and each one after doubles the wait, up to a day. A right guess clears the
device's count; one device's guesses never lock out another.

### `POST /v1/enroll`

Required JSON: `note`, the signed note a device of the mesh issued the app, and
`passphrase`. Unknown fields are refused.

```json
{
  "note": {"by":"9c1e…","body":"eyJtZXNo…","sig":"Qm3t…"},
  "passphrase":"correct horse battery staple"
}
```

ErisDB checks, in order: it is itself the mesh's ErisDB, its own note counting
(503 `not_the_erisdb`); the caller is a device of the mesh (403
`not_a_device`); the note reads and its signer is the caller (400 `bad_note`, 403
`not_a_device`); the note counts in ErisDB's list — the same mesh, its device a
member, not revoked — and approves an app for service `erisdb` that does not serve
it (400 `bad_note`); the passphrase (above). None of the refusals before the
passphrase costs the device a guess. Then it creates the installation for the
note's app, or, when that app has an active one, gives it the note's device, id
and grants. **200**:

```json
{"client":"ad6caf6b-ece2-442a-a178-a93cc5aa0802","grants":["notes:read","notes:create"]}
```

The app now calls by its key. Each enrollment appends an installation audit
event, `created` or `updated`, whose source `addr` is the device. Enrolling the
same note twice gives the same result. A revoked installation is never updated:
enrolling its app again creates a new one.

### `POST /v1/passphrase`

Required JSON: `current` and `new`, strings; unknown fields are refused. `new` is
at least 8 characters, at most 1024 bytes (400 otherwise, checked before
`current`, so a short one costs no guess). A wrong `current` counts against the
device like any wrong guess. **204** with no body; the new passphrase applies to
the next enrollment. The operator sets the first passphrase, and resets one, on
the server with `erisdb passphrase set`.

## Installations

An installation is an app a device let in: its Iroh key, the device that
approved it, and the note it approved it with. The registry is separate from
items and is shared by all replicas. One active installation exists per app key.
Enrolling that app again updates it; revoked installations remain revoked, and a
fresh enrollment creates a new UUID.

### Installation object

```json
{
  "id":"ad6caf6b-ece2-442a-a178-a93cc5aa0802",
  "name":"Notes",
  "app":"5f2d…64 hex",
  "device":"9c1e…64 hex",
  "note":"e04b…64 hex",
  "approved":["notes:read","notes:create"],
  "grants":["notes:read","notes:create"],
  "revoked_at":null,"revision":1,"created_at":"2026-09-21T12:00:00Z"
}
```

| Field | Meaning |
|---|---|
| `id` | Installation UUID |
| `name` | The app's name for itself, as the device approved it; trusted for nothing |
| `app` | The app's Iroh endpoint id: its credential |
| `device` | Endpoint id of the device that approved it |
| `note` | Id of the note it was approved with |
| `approved` | The note's grants: the most this installation can hold |
| `grants` | What it holds now |
| `revoked_at` | RFC 3339 timestamp or null; revocation is irreversible |
| `revision` | Optimistic concurrency version, initially 1 |
| `created_at` | RFC 3339 creation time |

An installation lets its app in while it is not revoked, its device is a member
of the mesh and its note is not revoked. When the mesh removes the device or
revokes the note, the app is refused at once, and ErisDB sets `revoked_at` with
an audit event.

### `GET /v1/clients`

Requires `meta:clients:read`. No query parameters. **200** `{"clients":[INSTALLATION,…]}`.
Returns all installations, including revoked ones, ordered by creation time then
ID. There is no filter or pagination.

```bash
api GET /v1/clients
```

### `GET /v1/clients/{id}`

Requires `meta:clients:read`. **200** Installation; **404** if absent.

```bash
CLIENT_JSON=$(api GET "/v1/clients/$CLIENT_ID")
CLIENT_REV=$(jq -r '.revision' <<<"$CLIENT_JSON")
```

### `PUT /v1/clients/{id}`

Requires `meta:clients:write`. Body: required `grants: string[]` and
`revision: integer`. Replaces the grant set; this route does not change the
name, key, device, note or approved ceiling. **200** Installation with its
revision incremented.

```bash
CLIENT_JSON=$(api PUT "/v1/clients/$CLIENT_ID" --data \
  "$(jq -nc --argjson revision "$CLIENT_REV" \
    '{revision:$revision,grants:["notes:read"]}')")
CLIENT_REV=$(jq -r '.revision' <<<"$CLIENT_JSON")
```

Grants must fit both `approved` and the acting administrator's effective grants
(403 otherwise). Empty grants are invalid (400); use revocation to remove all
access. Missing installation gives 404, a revoked one 409, stale revision 409.
Changes apply to the app's next request, and to tokens it delegated on their next
authorization check. Registry writes append system audit events and notify
subscribers.

### `POST /v1/clients/{id}/revoke`

Requires `meta:clients:revoke`. No body or revision parameter. **200** Installation
with `revoked_at` populated and revision incremented. Repeated revocation returns
the same revoked record without another increment. Missing ID gives **404**.

```bash
api POST "/v1/clients/$CLIENT_ID/revoke"
```

Blocks the app's subsequent requests on every replica, and its delegated tokens.
Idle subscriptions wake and close; each event rechecks authority. It does not
undo already delivered data, already-authorized transactions, or plugin calls
already launched. The API has no un-revoke or registry-delete route; the app
gets back in only by enrolling again through a device.

## Plugins

### `GET /v1/plugins`

Requires an enrolled key or a valid token, no additional grant. **200** `{"plugins":[…]}`. Each
plugin has `name`, `description`, and an `operations` array. Each operation has
`name`, `description`, `permission`, and `request_schema` (JSON Schema).
Only operations the caller's effective grants allow are visible; plugins with
no visible operations are omitted. No configured plugins means an empty array.

```bash
api GET /v1/plugins
```

Example with the shipped OpenAI plugin installed and `openai:chat` granted:

```json
{
  "plugins":[{
    "name":"openai","description":"OpenAI API calls through a fresh process per request",
    "operations":[{
      "name":"chat.completions","description":"Create or stream an OpenAI Chat Completion",
      "permission":"openai:chat",
      "request_schema":{"type":"object","required":["model","messages"]}
    }]
  }]
}
```

Descriptions and schemas are deployment-defined; this example abbreviates the
schema. Discovery does not expose executable paths or secret environment values.

### `POST /v1/call`

Body: required `plugin: string`, `operation: string`, and `input: JSON`.
Unknown envelope fields are refused (422). The selected manifest determines
permission and input schema. The core authorizes, validates, starts one process,
and streams its response. It creates no item, history row, job or conversation.

```bash
# Requires the shipped plugin to be installed/configured and a provider model
# name supplied in MODEL. This invokes the external provider.
MODEL='<provider-model-name>'
api POST /v1/call --no-buffer --data \
  "$(jq -nc --arg model "$MODEL" \
    '{plugin:"openai",operation:"chat.completions",input:{model:$model,
      messages:[{role:"user",content:"Hello"}],stream:true}}')"
```

The response status, content type and bytes come from the executable. They may
be JSON, SSE or another media type. Check status/content type before decoding;
stream with curl `--no-buffer` or equivalent. The shipped plugin forwards its
`input` to the provider's Chat Completions endpoint and returns the provider's
response; model options and tool execution belong to the caller/provider.

Core-level failures include unknown plugin/operation (404), insufficient grants
(403), schema violation (422), unavailable required environment (503), process
capacity (503), and process/protocol failure (502). A failure after headers are
sent truncates/errors the stream instead of changing its status. Disconnect or
timeout kills the child; there is no automatic replay of an ambiguous invocation.

### Plugin process protocol v1

Plugins are operator-installed executables, not network registrations. A manifest
loaded with `erisdb serve --plugin-dir DIR` has these fields:

| Field | Required / default | Meaning |
|---|---|---|
| `protocol` | Required | `1` |
| `name` | Required | Lowercase permission-segment name |
| `description` | Default empty string | Discovery text |
| `executable` | Required | Absolute executable path |
| `args` | Default empty array | String arguments passed to the executable |
| `environment` | Default empty map | Environment-name → boolean; true required, false optional; values come from the service environment |
| `timeout_secs` | Default 600 | Integer 1–3600 |
| `grants` | Default empty array | What the plugin may do in ErisDB on every call; see below |
| `operations` | Required nonempty object | Operation name → `{description?, permission, request_schema}` |

Operation permissions must be concrete, under the plugin's namespace, with no
wildcards. Unknown manifest, operation-definition, and response-header JSON fields
are rejected. Schemas reject external references. The child environment is cleared
and rebuilt from the allowlist; bearer tokens and undeclared core secrets are
not forwarded. The process runs as the core service user, not in a hostile-code
sandbox. Complete installation examples are in [plugins.md](plugins.md).

Stdin receives one JSON line then EOF:

```json
{"protocol":1,"plugin":"example","operation":"echo","input":{"message":"hello"},"context":{"user":"alice","installation":"ad6caf6b-ece2-442a-a178-a93cc5aa0802"}}
```

`context.user` is omitted when the caller has no signed user label.
`context.installation` names the installation that called, by its key or by a
token it delegated, and is omitted for manual tokens. For a manifest with `grants`,
`context.erisdb` is `{"url": …, "token": …}`: the core's address and a
capability holding exactly those grants, signed for user `plugin:<name>`,
belonging to no installation, and expiring with the call's timeout, with no
renewal past it. Stdout starts
with one JSON header line, followed immediately by raw response bytes:

```text
{"protocol":1,"status":200,"content_type":"application/json","headers":{"x-request-id":"example-1"}}
{"message":"hello"}
```

`headers` defaults to an empty map. Hop-by-hop, credential, cookie,
content-length and content-type entries are filtered; `content_type` controls
the content type. The core supplies `Cache-Control: no-store`. Status must be
200–599. Flush streaming output. Use stderr for diagnostics;
the core captures only a bounded amount. Header limit: 16 KiB; retained stderr:
64 KiB. Full process behavior and the shipped manifest are in
[the plugin guide](plugins.md) and [deploy/plugins/openai.json](../deploy/plugins/openai.json).

## Operator CLI

These commands wrap or host the API; they do not add HTTP routes. All secret
flags accept protected environment variables so secrets need not appear in
command history. Built-in `--help` lists parser details.

| Command | Main arguments | Example |
|---|---|---|
| `erisdb join` | `--database-url`/`DATABASE_URL`, signing/optional Iroh secret; optional `--name` (`ErisDB`); ErisAuth listening at `$XDG_RUNTIME_DIR/erisauth/agent.sock` | `erisdb join` |
| `erisdb serve` | `--database-url`/`DATABASE_URL`, `--secret`/`ERISDB_SECRET`; optional `--listen`/`ERISDB_LISTEN`, `--iroh-secret`/`ERISDB_IROH_SECRET`, `--network`/`ERIS_NETWORK`, `--plugin-dir`/`ERISDB_PLUGIN_DIR`, `--lockout-secs`/`ERISDB_LOCKOUT_SECS` (60) | `erisdb serve --listen 127.0.0.1:7700` |
| `erisdb passphrase set` | `--database-url`/`DATABASE_URL`; the passphrase typed twice at a terminal, or one line of standard input | `erisdb passphrase set` |
| `erisdb passphrase unlock [DEVICE]` | `--database-url`/`DATABASE_URL`; a device's endpoint id, or none for every device | `erisdb passphrase unlock` |
| `erisdb endpoint-id` | Same signing/optional Iroh secret environment | `erisdb endpoint-id` |
| `erisdb mint` | Required repeatable/comma-separated `--grant`; `--ttl` or `--no-expiry`; optional `--max-ttl`, `--user`; signing secret | `erisdb mint --grant notes:read --ttl 3600` |
| `erisdb clients list` | Parent `--url`/`ERISDB_URL`, `--secret`/`ERISDB_SECRET` | `erisdb clients list` |
| `erisdb clients show ID` | Installation UUID | `erisdb clients show "$CLIENT_ID"` |
| `erisdb clients permissions ID` | Required repeatable/comma-separated `--grant`; fetches current revision first | `erisdb clients permissions "$CLIENT_ID" --grant notes:read` |
| `erisdb clients revoke ID` | Installation UUID | `erisdb clients revoke "$CLIENT_ID"` |

Put parent options before the subcommand, for example
`erisdb clients --url http://127.0.0.1:7700 list` and
`erisdb passphrase --database-url … set`. Administration prints JSON.
`mint --no-expiry` is for manual/operator authority; it has no individually
revocable installation. `join` waits while the person answers on the device;
Ctrl-C withdraws the request. `serve` refuses to start before `join`.

## Limits

| Surface | Limit / behavior |
|---|---|
| Ordinary HTTP JSON body | 2 MiB; 413 beyond it |
| Plugin call body | 16 MiB; 413 beyond it |
| Item list | Default 100, clamp 1–1000; no cursor |
| Change page | Default 500, clamp 1–5000 |
| Item history | No API pagination |
| Installation list | All rows; no pagination |
| Live change streams | 32 per replica; excess 503; each holds a Postgres listener connection |
| Plugin processes | 32 per replica; excess 503 |
| Iroh connections / streams | 64 connections, 32 streams per connection; excess dropped |
| Store pool | 16 connections per core replica |
| Mint and refresh rate | Shared bucket for `/v1/capabilities` and `/v1/capabilities/refresh`: burst 10, replenishes one token per five seconds |
| Rate identity | Observed TCP **IP**, not source port; authenticated Iroh peer key over Iroh; a local proxy's callers share its IP bucket |
| Rate durability | Per replica, reset by restart; no `Retry-After` header |
| Passphrase guesses | Per device, in Postgres: three free, then 1 minute doubling to a day; `Retry-After` on 429 |
| Passphrase length | 8 characters to 1024 bytes |
| Grant count / size | 1–64 grants, each at most 128 bytes |
| User / client label | At most 128 bytes; client header additionally printable ASCII |
| Installation lifetime | Until revoked, or until the mesh removes its device or revokes its note |
| Manual/delegated HTTP mint chain | Resulting chain at most 31536000 seconds (365 days) |
| Plugin timeout | Default 600 seconds, configured 1–3600 |
| Plugin header / retained stderr | 16 KiB / 64 KiB |

For deployment, transport setup and backups, see [operations](operations.md).
