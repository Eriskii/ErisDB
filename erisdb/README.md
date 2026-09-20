# erisdb

Holds your facets.

A stateless personal data core: one Postgres store, N interchangeable core
replicas, capability-bearing clients for durable data, and one-shot
executables for non-durable calls into external systems.

## Vocabulary

- **Store** — Postgres. The only stateful thing. Two tables: `items`
  (current truth) and `changes` (a durable, totally-ordered change feed that
  doubles as the event bus).
- **Core** — this process. Verifies capabilities, validates writes against
  facet schemas, serves the API. Holds nothing a restart would lose; run as
  many replicas as you like.
- **Facet** — a named contract over the store (`tasks`), and the namespace
  its permissions live in. Facets are themselves items in the meta-facet
  `facet`: registering one is a `POST /v1/items`, no deploy. Writes to a
  strict facet are validated against its JSON Schema, and the schema
  version lives in the body so a grant survives it moving.
- **Client** — anything with a capability token. A bridge is a client that
  represents an external system and keeps its config as items in its own
  facet.
- **Plugin** — an operator-installed executable described by a manifest. One
  call starts one fresh process and streams its stdout; it never listens,
  persists, retries, or touches Postgres. An operation declares its own JSON
  Schema and permission, such as `openai:chat`.
- **Capability** — a signed, self-describing token carrying **grants**:
  permission patterns like `tasks:read`, `tasks:*`, `*:read` or
  `meta:facets:write`, with an expiry and optionally a `user` identity.
  The core verifies a signature and looks nothing up.
- **Permission** — what a request requires, computed from the request:
  `tasks:create`, `meta:pairing:approve`. Four actions per facet — `read`,
  `create`, `update`, `delete` — so an append-only logger can hold
  `sensors:create` and nothing else. The core keeps **no registry** of
  valid permissions: a grant for a facet that does not exist yet is legal
  and inert until it does.
- **Chain** — the second clock on a token. `exp` is when this token stops
  working; `max_exp` is when its line does. Refresh moves `exp` forward and
  never past `max_exp`, so a token renews itself for a bounded stretch and
  then a human mints a new one.
- **Source** — server-stamped attribution on every write:
  `{addr, user, client}` with a trust gradient. `addr` is observed from
  the connection (peer IP over TCP, `iroh:<endpoint id>` over QUIC),
  `user` is signed into the capability, `client` is whatever the caller
  claims via the `X-Bezel-Client` header. Items carry their last writer's
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
POST   /v1/capabilities/refresh     {ttl_secs} — same grants, fresh exp  any valid token
GET    /v1/permissions              what this token holds              none
GET    /v1/plugins                  callable operation schemas         valid token
POST   /v1/call                     one fresh executable, streamed     manifest permission
GET    /v1/server                   version, counts, live limits       meta:server:read
POST   /v1/pairings                 cut a pairing code                 meta:pairing:create
GET    /v1/pairings, /v1/pairings/{id}                                 meta:pairing:read
POST   /v1/pairings/{id}/approve, /deny                                meta:pairing:approve
POST   /v1/pair/redeem              {client, requested}                the code itself
GET    /v1/pair/status              collect with installation proof    the code and identity
```

The core's own facets answer only to `meta:`: `facet` to
`meta:facets:read` and `meta:facets:write`, `system` to `meta:system:*`,
`pair` to `meta:pairing:*`. So `*:read` reads every facet of yours and
reaches none of them.

## Capability lifetime

Nothing is looked up, so nothing can be taken back — a token is only as
bounded as it was minted. Two rules keep that honest:

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

Revoking everything at once means rotating `ERISDB_SECRET`, which also moves
the iroh endpoint id unless `ERISDB_IROH_SECRET` is set separately. Set it
separately if you ever intend to rotate.

## Limits

The core bounds what a caller can spend: 32 concurrent change streams
(each holds a Postgres connection of its own), 64 iroh connections with 32
streams each, a token bucket over both capability endpoints, and a 128-char
cap on `X-Bezel-Client`. A facet schema may only `$ref` into itself — an
external ref would ask the core to fetch a URL or read a file on every
write to that facet, so it is refused at registration.

Authorization on the by-id routes answers 403, not 404, when a token does
not cover the item's facet. That tells a caller holding the wrong token
something true about an id it already has; item ids are v4 UUIDs, so it is
not a way to find one.

The same router is served over plain TCP and over Iroh (ALPN `bezel/0`,
HTTP/1.1 per QUIC bi-stream), so an ErisDB is dialable from anywhere without
exposing a port. The iroh identity is derived from `ERISDB_IROH_SECRET`, or
from `ERISDB_SECRET` when that is unset, so the endpoint id survives
restarts: clients hold one address forever. Iroh authenticates the pipe; the
capability token authorizes the request — anyone may connect, nobody reads
or writes without a token.

The TCP path has neither. It defaults to `127.0.0.1:7700`, and binding it
anywhere else publishes every bearer token that crosses it — put TLS in
front, or use iroh.

## Run

```sh
export DATABASE_URL=postgres://…
export ERISDB_SECRET=$(openssl rand -hex 32)
erisdb serve                  # migrates, serves TCP + Iroh
erisdb pair --name my-laptop  # QR to scan; the client asks, you answer
erisdb mint --grant tasks:read,tasks:create --ttl 86400 --user alice
erisdb mint --grant meta:system:tick --no-expiry     # the poker
erisdb endpoint-id            # the address clients dial
```

## Pairing

`erisdb pair` cuts a **pairing code** against a running core and prints it
as a QR code in block characters, with the endpoint id, url and code
underneath for anyone who would rather type them, and `s` to save the code
as a png. There is no pairing service anywhere: the ticket carries the
address, and the core holds the conversation as an ordinary item.

The code grants exactly `meta:pairing:redeem` on one session and expires in
minutes. It is not a capability over any data. A client redeems it, says
who it is and which permissions it wants, and the terminal shows the
request:

```
Tasks (Android) v0.3 wants:
  1. tasks:read
  2. tasks:create
  3. tasks:update

[a] approve as asked   [s] select   [d] deny
```

So a photographed screen is not a leaked capability — it gets an attacker
as far as a prompt on your screen. Approve the narrowest set that works;
`s` takes a comma-separated list of numbers. Compare the fingerprint with the app. Approval cannot exceed the request or
the approver. Registered clients renew until revoked: `erisdb clients list`,
`erisdb clients permissions ID --grant tasks:read`, `erisdb clients revoke ID`.
See [registered installations](../docs/clients.md). The ticket format is in [docs/pairing.md](../docs/pairing.md) and
the flow is in [docs/permissions.md](../docs/permissions.md#pairing).

## Tests

`cargo test` runs the e2e suite: real Postgres via testcontainers (Docker
required), real HTTP over real sockets, real Iroh QUIC. No mocks.
