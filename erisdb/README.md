# erisdb

Holds your facets.

A stateless personal data core: one Postgres store, N interchangeable core
replicas, apps its owner's mesh let in for durable data, and one-shot
executables for non-durable calls into external systems.

## Vocabulary

- **Store** — Postgres. `items` holds current truth, `changes` is a durable,
  totally-ordered change feed that doubles as the event bus, `clients` holds
  installations, `mesh` the core's note and its copy of the device list, and
  `passphrase` the enrollment passphrase's hash.
- **Core** — this process. Verifies capabilities, validates writes against
  facet schemas, and serves the API. Durable state is stored in Postgres.
- **Facet** — a named schema over the store (`tasks`), and the namespace
  its permissions live in. Facets are themselves items in the meta-facet
  `facet`: registering one is a `POST /v1/items`, no deploy. Writes to a
  strict facet are validated against its JSON Schema, and the schema
  version lives in the body so a grant survives it moving.
- **Mesh** — the owner's devices, each running ErisAuth, keeping one signed
  list of who is in. ErisDB is an app in it, the one approved to serve
  `erisdb`: a mesh has one, and approving another revokes this one's approval.
  Its node keeps the list current. See [the mesh](../docs/mesh.md).
- **Client** — a separate application that calls the server.
- **Installation** — an app a device let in: its Iroh key, the device, and
  the note the device approved it with. The person approves on the device and
  types ErisDB's passphrase; the device enrolls the app here. Its key is its
  only credential. Postgres stores its grants and revocation; the mesh decides
  whether its device and note still count.
- **Plugin** — an operator-installed executable described by a manifest. One
  call starts one fresh process and streams its stdout; it never listens,
  persists, retries, or touches Postgres. An operation declares its own JSON
  Schema and permission, such as `openai:chat`.
- **Capability** — what a request holds: **grants**, permission patterns like
  `tasks:read`, `tasks:*`, `*:read` or `meta:facets:write`. An installation's
  come from the registry. Operators, daemons and delegates carry theirs as a
  signed, self-describing bearer token with an expiry and optionally a `user`
  identity; a token an installation minted names it, and holds no more than it
  does.
- **Permission** — what a request requires, computed from the request:
  `tasks:create`, `meta:clients:revoke`. Four actions per facet — `read`,
  `create`, `update`, `delete` — so an append-only logger can hold
  `sensors:create` and nothing else. The core keeps **no registry** of
  valid permissions: a grant for a facet that does not exist yet is legal
  and inert until it does.
- **Chain** — the second clock on a token. `exp` is when this token stops
  working; `max_exp` is when its line does. Refresh moves `exp` forward and
  never past `max_exp`, so a token renews itself for a bounded stretch and
  then a human mints a new one. Installations have no clock: they last until
  revoked, or until the mesh ends them.
- **Source** — server-stamped attribution on every write:
  `{addr, user, client, installation}` with a trust gradient. `addr` is observed
  from the connection (peer IP over TCP, `iroh:<endpoint id>` over QUIC),
  `user` is signed into the capability, `client` is whatever the caller
  claims via the `X-ErisDB-Client` header, and `installation` is the
  installation the caller's key or token belongs to. Items carry their last writer's
  source; every change row carries the source that produced it.
- **History** — every change row snapshots the body and revision it
  produced. The feed is a full, append-only audit log: any past state can
  be read back and rolled forward, deleted items keep their history, and
  sync clients apply the feed directly without refetching items.
- **Poker** — an external clock hitting `POST /v1/tick`; the tick lands on
  the change feed and subscribers do their own due-checks.

## API

```
GET    /v1/health
POST   /v1/items                    create (schema-validated)          {facet}:create
GET    /v1/items/{id}                                                  {facet}:read
GET    /v1/items?facet=&updated_since=&limit=                          {facet}:read
PUT    /v1/items/{id}               {body, revision}                   {facet}:update
DELETE /v1/items/{id}?revision=     revision optional                  {facet}:delete
GET    /v1/items/{id}/history       every state it has been in         {facet}:read
POST   /v1/items/{id}/revert        {seq, revision} — as a NEW revision {facet}:update
GET    /v1/changes?since=&facet=    cursor-paged feed, bodies included {facet}:read / meta:feed:read
GET    /v1/changes/stream           SSE, live via Postgres NOTIFY      as above
POST   /v1/tick                                                        meta:system:tick
POST   /v1/capabilities             {grants, ttl_secs, max_ttl_secs?, user?}  meta:capabilities:mint
POST   /v1/capabilities/refresh     {ttl_secs} — same grants, fresh exp  a valid bearer token
GET    /v1/permissions              what this caller holds             none
GET    /v1/plugins                  callable operation schemas         any caller
POST   /v1/call                     one fresh executable, streamed     manifest permission
GET    /v1/server                   version, counts, live limits       meta:server:read
POST   /v1/enroll                   {note, passphrase} — let an app in a device of the mesh
POST   /v1/passphrase               {current, new}                     a device of the mesh
GET    /v1/clients, /v1/clients/{id}                                  meta:clients:read
PUT    /v1/clients/{id}             {grants, revision}                 meta:clients:write
POST   /v1/clients/{id}/revoke                                        meta:clients:revoke
```

The core's own facets answer only to `meta:`: `facet` to
`meta:facets:read` and `meta:facets:write`, `system` to `meta:system:*`.
So `*:read` reads every facet of yours and reaches none of them.

## Capability lifetime

An app calling by its key needs an active installation, its device in the mesh
and its note unrevoked, checked on every request. Revoke one installation with
`erisdb clients revoke ID`, or remove its device or revoke its note from any
device; either blocks subsequent access, including delegated tokens, and closes
its change streams across replicas.

Tokens carry expiry and refresh-chain limits. Two rules apply to token
delegation and bounded refresh:

- **Enclosure covers scope and time.** A minted token never grants what its
  minter lacks, and never outlives it, in expiry or in chain. A ten-minute
  token is ten minutes of authority including everything it delegates. Scope
  is subsumption between patterns, so `tasks:read` cannot mint `tasks:*` —
  and holding all four actions is deliberately not the same as holding the
  wildcard, because a fifth action would change what the wildcard means.
- **Refresh moves `exp`, never `max_exp`.** A leaked token buys the holder
  the rest of its chain and no more. `ttl_secs` is required when minting
  over HTTP, and the chain is capped at a year.

There is one way to cut a token that never expires, and it needs the
secret: `erisdb mint --no-expiry`. Do it for daemons that must not fail at
3am, and know that the only way to revoke one is rotating `ERISDB_SECRET`.
`--ttl 0` is an error rather than a spelling of "forever", because it
produces a token that is already expired.

Rotating `ERISDB_SECRET` invalidates outstanding token signatures; apps hold no
tokens and carry on. Rotation also changes the core's Iroh key unless
`ERISDB_IROH_SECRET` is set separately, and the mesh approved the old one: the
core joins again. See [clients.md](../docs/clients.md).

## Limits

The core bounds what a caller can spend: 32 concurrent change streams
(each holds a Postgres connection of its own), 64 iroh connections with 32
streams each, a token bucket over both capability endpoints, and a 128-char
cap on `X-ErisDB-Client`. A facet schema may only `$ref` into itself — an
external ref would ask the core to fetch a URL or read a file on every
write to that facet, so it is refused at registration.

Authorization on the by-id routes answers 403, not 404, when a token does
not cover the item's facet. That tells a caller holding the wrong token
something true about an id it already has; item ids are v4 UUIDs, so it is
not a way to find one.

The same router is served over plain TCP and over Iroh (ALPN `erisdb/0`,
HTTP/1.1 per QUIC bi-stream), so an ErisDB is dialable from anywhere without
exposing a port; the mesh's protocol, `eris-mesh/0`, shares the endpoint. The
iroh key is derived from `ERISDB_IROH_SECRET`, or from `ERISDB_SECRET` when that
is unset, so it survives restarts and stays the key the mesh approved. Iroh
authenticates the pipe, and the caller's key is an app's whole credential —
anyone may connect, nobody reads or writes without an installation or a token.

The TCP path carries bearer tokens only. It defaults to `127.0.0.1:7700`, and
binding it anywhere else publishes every bearer token that crosses it — put TLS
in front, or use iroh.

## Run

```sh
export DATABASE_URL=postgres://…
export ERISDB_SECRET=$(openssl rand -hex 32)
erisdb join                   # where ErisAuth runs: a device approves the core
erisdb passphrase set         # what devices type to let apps in
erisdb serve                  # migrates, joins the mesh's node, serves TCP + Iroh
erisdb mint --grant tasks:read,tasks:create --ttl 86400 --user alice
erisdb mint --grant meta:system:tick --no-expiry     # the poker
erisdb endpoint-id            # the key a device approves
```

## Letting apps in

Apps are let in on a device, not on the server. An app asks ErisAuth on its
device to approve its key for `erisdb` with the grants it wants; the person approves all
of them or fewer, and types ErisDB's passphrase; ErisAuth enrolls the app here
from the device's own endpoint (`POST /v1/enroll`), and ErisDB checks the device
is in the mesh, the note is the device's own and counts, and the passphrase is
right. Wrong guesses are counted per device: three free, then a minute's wait,
doubling to a day.

From then on the app dials ErisDB as its key and nothing else. A device removed
from the mesh, or a note revoked, ends the app here the moment the list reaches
the core. The operator narrows or revokes installations with `erisdb clients
list`, `erisdb clients permissions ID --grant tasks:read` and `erisdb clients
revoke ID`, and clears a device's lockout with `erisdb passphrase unlock`. See
[the mesh](../docs/mesh.md) and [installations](../docs/clients.md).

## Tests

`cargo test` runs the suites: real Postgres via testcontainers (Docker
required), real HTTP over real sockets, the real erisdb binary, and real Iroh
QUIC. `tests/mesh.rs` builds device nodes from the mesh library in the test
process, joins the binary to their mesh through a stand-in for ErisAuth's agent
socket, and enrolls, cuts off and administers apps, all on loopback endpoints.
No mocks.
