# bezel

Holds your facets.

A stateless personal data core: one Postgres store, N interchangeable core
replicas, and everything else — apps, bridges, agents, the poker — is a
client holding a capability token.

## Vocabulary

- **Store** — Postgres. The only stateful thing. Two tables: `items`
  (current truth) and `changes` (a durable, totally-ordered change feed that
  doubles as the event bus).
- **Core** — this process. Verifies capabilities, validates writes against
  facet schemas, serves the API. Holds nothing a restart would lose; run as
  many replicas as you like.
- **Facet** — a named contract over the store (`tasks/v1`). Facets are
  themselves items in the meta-facet `facet`: registering one is a `POST
  /v1/items`, no deploy. Writes to a strict facet are validated against its
  JSON Schema.
- **Client** — anything with a capability token. A bridge is a client that
  represents an external system and keeps its config as items in its own
  facet.
- **Capability** — a signed, self-describing token scoping facets × verbs
  (`read`, `write`, `admin`) with an expiry, and optionally a `user`
  identity. The core verifies a signature and looks nothing up.
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
POST   /v1/items                    create (schema-validated)
GET    /v1/items/{id}
GET    /v1/items?facet=&updated_since=&limit=
PUT    /v1/items/{id}               {body, revision} — optimistic concurrency
DELETE /v1/items/{id}
GET    /v1/items/{id}/history       every state the item has been in
POST   /v1/items/{id}/revert        {seq, revision} — old snapshot as a NEW revision
GET    /v1/changes?since=&facet=    cursor-paged change feed (bodies included)
GET    /v1/changes/stream           SSE, live via Postgres NOTIFY
POST   /v1/tick
POST   /v1/capabilities             mint a narrower token (admin verb)
```

The same router is served over plain TCP and over Iroh (ALPN `bezel/0`,
HTTP/1.1 per QUIC bi-stream), so a bezel is dialable from anywhere without
exposing a port. The iroh identity is derived from `BEZEL_SECRET`, so the
endpoint id survives restarts: clients hold one address forever. Iroh
authenticates the pipe; the capability token authorizes the request —
anyone may connect, nobody reads or writes without a token.

## Run

```sh
export DATABASE_URL=postgres://…
export BEZEL_SECRET=…
bezel serve                  # migrates, serves TCP + Iroh
bezel mint --facets 'tasks/v1' --verbs read,write --ttl 86400 --user alice
```

## Tests

`cargo test` runs the e2e suite: real Postgres via testcontainers (Docker
required), real HTTP over real sockets, real Iroh QUIC. No mocks.
