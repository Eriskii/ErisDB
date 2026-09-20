# Operations

Running an ErisDB. The [root README](../README.md#wiring-it-up) has the
install commands; this page is what to know while it is running.

The system is two moving parts and one stateful one. `erisdb serve` is a
process that holds nothing a restart would lose. The poker is a curl in a
timer. Postgres is everything else.

## The process

```sh
erisdb serve
```

Reads its whole configuration from flags or environment, with the
environment being the normal path:

| variable | flag | default | meaning |
|----------|------|---------|---------|
| `DATABASE_URL` | `--database-url` | required | Postgres connection string. |
| `ERISDB_SECRET` | `--secret` | required | HMAC key for capability tokens; also the iroh seed unless the next one is set. |
| `ERISDB_IROH_SECRET` | `--iroh-secret` | falls back to `ERISDB_SECRET` | Seed for the iroh endpoint identity. |
| `ERISDB_LISTEN` | `--listen` | `127.0.0.1:7700` | The plain-TCP listener. |
| `ERISDB_PLUGIN_DIR` | `--plugin-dir` | unset (no plugins) | Directory of one-shot plugin JSON manifests. |
| — | `--no-iroh` | off | Serve TCP only, with no iroh endpoint at all. |
| `RUST_LOG` | — | `erisdb=info` | Standard `tracing` filter syntax. |

On start it loads plugin manifests when configured, connects the pool (16
connections per replica), runs the migrations, binds the TCP listener, then
binds the iroh endpoint and serves both until one of them stops. Plugin
secrets such as `OPENAI_API_KEY` are ordinary service environment variables;
each manifest explicitly allowlists which ones its fresh process receives.

`deploy/erisdb.service` passes `--listen ${ERISDB_LISTEN}` explicitly, so
that variable must be set in `/etc/erisdb/erisdb.env`: unset, systemd
expands it to nothing and the binary is handed a `--listen` with no value
and refuses to start.

Both secrets are marked so that `--help` and error output never echo them.

### The core is replaceable

Nothing in the process is durable. Restart it, run three of them behind a
load balancer, kill one mid-request — the store is the truth and every
replica reads it. Two things are per-replica and worth knowing:

- **Compiled schema validators.** Cached per process, keyed by facet name
  and matched on the schema itself. Editing a registration recompiles on
  the next write, on each replica independently. No restart needed.
- **Rate-limit buckets.** Also per process, and soft on purpose: a restart
  forgives everyone and replicas do not share buckets. They exist to blunt
  a flood, not to keep books.
- **Plugin manifests and process slots.** Manifests are reloaded from
  deployment configuration on restart. The 32 child-process permits are
  soft capacity, not job state. Calls themselves never touch the store and
  disappear with their request.

The e2e suite pins this: two replicas over one store, an item written
through one and read through the other immediately.

## Migrations

The migrations are compiled into the binary (`sqlx::migrate!`) and run at
the start of every `erisdb serve`. There is no separate migrate step, no
migrations directory to ship, and no way for the binary and the schema it
expects to disagree.

The practical consequences:

- **Deploying is replacing the binary and restarting.** The new one
  migrates on the way up.
- **Rolling back means rolling back the binary,** and only works if the
  migrations in between were additive. Every migration in the tree so far
  is: new columns, new indexes, a widened `CHECK`, a function redefined.
- **Two replicas starting at once are fine.** sqlx takes a lock; the
  second waits and then finds nothing to do.
- **A migration that fails stops the process** with `migrating the store`
  in the message. It does not serve on a half-migrated schema.

Five migrations exist. `0001_init` creates `items` and `changes` and
bootstraps the `facet` meta-facet. `0002_lapse` adds the `lapsed` op and
the `safe_ts` helper. `0003_source_history` adds attribution and the body
snapshots that make the feed a full audit log. `0004_safe_ts_stable`
corrects `safe_ts` to `STABLE`, because `'now'::timestamptz` reads the
clock and an `IMMUTABLE` declaration would let the planner constant-fold a
call — and would silently corrupt any expression index built on it.
`0005_namespaced_permissions` moves the schema version out of facet names
and into their bodies, rewrites `items.facet` and `changes.facet` to
match, grows the meta-facet with `version` and `permissions`, and
bootstraps the `pair` facet. It is the one migration that rewrites
existing rows; the file explains why, at length.

`0005` is also the one to read before rolling a binary backwards, since a
core that predates it looks for `tasks/v1` and finds `tasks`.

## The poker

An external clock. `POST /v1/tick` puts a tick on the change feed and
sweeps every facet declaring a lapse rule; subscribers do their own
due-checks off the feed.

`poker/poke.sh` is a curl with two variables — one URL, one token, no
state — driven by a systemd timer at `OnCalendar=*-*-* *:*:00`, so once a
minute. Any cron that can set `ERISDB_URL` and `ERISDB_POKER_TOKEN` works
just as well.

Its token is deliberately the narrowest in the deployment and the only
one in it that never expires:

```sh
erisdb mint --grant meta:system:tick --no-expiry
```

That is authority to do exactly one thing — `POST /v1/tick` and nothing
else, not even reading the tick rows it produces. `--no-expiry` is right
here
because the alternative is due notifications silently stopping at 3am on
a Sunday, and the blast radius of this particular token is one row a
minute.

Things to know about the cadence:

- **Overlapping pokes are safe.** The tick's change row takes the feed's
  append lock and holds it for the whole transaction, so a second poke
  waits, then finds the first one's work already done. `lapsed` in the
  response counts what that call actually appended.
- **The timer is not `Persistent`.** A machine that was asleep does not
  fire a burst of catch-up pokes on wake; it fires the next one on the
  minute. That is correct — a lapse is about the item being overdue now,
  not about how many minutes were missed.
- **Every tick is a row.** Once a minute is about half a million rows a
  year in a table that nothing prunes. See
  [change-feed.md](change-feed.md#the-feed-grows-without-bound).
- **Not running the poker is a supported configuration.** Nothing else
  depends on the tick; facets without a lapse rule never notice. The two
  web apps also run a local due-check between ticks, so they degrade to
  slightly later notifications rather than none.

## Pairing clients

`erisdb pair` drives the ordinary API — the same routes a dashboard would —
so it needs a running core to talk to, and `--url` (or `$ERISDB_URL`) has
to point at one. Against a core that is not up it fails with
`reaching a core at … — is it running?` rather than printing a code that
could never be redeemed.

It cuts the code, prints the QR, and waits. When a client redeems, the
request appears with its comparison fingerprint and requested permissions.
Compare the fingerprint with the app, then choose `[a] approve as asked`,
`[s] select`, or `[d] deny`. Approval cannot exceed the request or the acting
operator's grants.

Two things to know afterwards:

- **Sessions accumulate.** Each one is an item in the `pair` facet and
  nothing prunes them. They are small, and expired ones are inert — a
  code past its `expires` cannot be redeemed or approved — but they are
  visible to anything holding `meta:pairing:read` forever. None of them
  holds a token.
- **An approved session that was never collected issues nothing.** The
  token is minted on collection, so denying an approved session before the
  client polls genuinely stops it existing. After collection there is a
  live credential and denying is refused with a 409: end that one by
  `erisdb clients revoke CLIENT_UUID`.

## Backup and restore

Covered in full in
[the root README](../README.md#backup-and-restore). The three things that
bite, restated because they are the ones that lose data:

1. **Both tables or neither.** `items` is current truth, `changes` is the
   feed every sync client holds a cursor into. Restoring `items` alone
   leaves those cursors pointing into a history that no longer exists.
2. **A dump contains everything ever written**, including every prior
   version and everything since deleted, because the feed *is* the
   history and nothing prunes it.
3. **A dump is half the system.** Without the same secrets, every token
   fails and the server comes back at a different address. Back the
   secrets up separately, and not inside the dump.

Postgres is the only stateful thing. There is no key file, no cache
directory, no per-replica state: the iroh identity is derived from the
secret rather than stored, and `deploy/erisdb.service` runs with
`ProtectSystem=strict` because the core writes nothing to disk at all.

## Secrets and what rotation costs

`ERISDB_SECRET` signs every capability token. `ERISDB_IROH_SECRET` seeds the
iroh endpoint identity — and when it is unset, `ERISDB_SECRET` does that
job too.

```sh
openssl rand -hex 32
```

Both are pure derivations, so `erisdb endpoint-id` answers the same whether
or not the server is running, and the same after every restart. Clients
hold one address forever.

**Set them separately, at install time.** `deploy/erisdb.env.example` ships
with only `ERISDB_SECRET`, which is the simple case and also the case where
the two rotations are welded together. Adding a second line now costs
nothing and is the difference between a future rotation that re-mints
tokens and one that also re-pins every client.

Rotating `ERISDB_SECRET` does exactly one irreversible thing, or two:

- **Outstanding token signatures become invalid.** Registered installations
  can renew using their installation proof. Manual tokens must be re-minted.
- **The address moves too, unless `ERISDB_IROH_SECRET` is set separately.**
  Every Android client, every MCP config, every `erisdb-client` caller is
  pinned to an endpoint id, and after a rotation they dial an address
  nobody answers. Print the new one with `erisdb endpoint-id` and re-pin
  each of them.

Revoke one paired installation with `erisdb clients revoke CLIENT_UUID`.
This immediately blocks subsequent access, renewal and active change feeds.
Legacy/manual tokens without a registration still require expiry or signing-key
rotation. [Client administration](clients.md) explains migration and recovery.

`/etc/erisdb/*.env` stays mode 0600 and root-owned: systemd reads it as the
manager, before dropping to the `erisdb` user, so the service account never
needs to see the secret on disk.

## Transport posture

Two paths into the same router, with very different properties. The
[root README](../README.md#transport) has the summary; the operational
reading is:

**Iroh is the reachable path.** ALPN `bezel/0`, HTTP/1.1 per QUIC
bi-stream, authenticated and encrypted end to end by the transport, with
no port to forward and no certificate to manage. The endpoint id is public
and stable by design, so anyone may dial — and nobody reads or writes
without a token. This is what the Android clients use.

**Plain TCP is the local path.** It defaults to `127.0.0.1:7700` and
belongs there. Capability tokens are bearer credentials in an
`Authorization` header, CORS is permissive because browser clients are
first-class and auth is the token rather than the origin, and there is no
transport security whatsoever: binding this listener to a public interface
publishes every token that crosses it to anyone on the wire. The core logs
a warning at startup if the bound address is not loopback, which is a
reminder and not a defence.

If it has to be reachable off-host, put a reverse proxy terminating HTTPS
in front of `127.0.0.1:7700`. Otherwise leave it on loopback and dial over
iroh, which needs no open port at all.

`--no-iroh` skips the endpoint entirely, for a deployment that only ever
serves loopback and a proxy.

`GET /v1/health` is the only unauthenticated route. It answers from the
process without touching the store, so it is liveness, not readiness — a
core with a dead database still answers `{"ok": true}`.

## Limits

Bounds the core enforces, and what hitting one looks like:

| bound | value | at the limit |
|-------|-------|--------------|
| Live change streams | 32 | **503** `unavailable`. Each stream holds a Postgres connection outside the pool, so this bounds the store as much as the process. |
| One-shot plugin processes | 32 | **503** `unavailable`. Per replica; a process occupies one slot until it exits or its response is dropped. |
| Iroh connections | 64 | The connection is dropped, with a warning logged. Bounded *before* authentication, because the endpoint id is public and a capability check costs more than a refusal. |
| Iroh streams per connection | 32 | The stream is dropped, with a warning naming the peer. |
| Capability endpoints | burst 10, then 1 per 5s | **429** `too_many_requests`, per caller, per replica. Checked before authorization, so a caller that cannot mint at all still sees 429 rather than 403. |
| `X-Bezel-Client` | 128 chars, printable ASCII | **400**. It lands in every change row this caller writes. |
| Grants per token / grant length | 64 / 128 chars | **400** at mint. |
| Refresh chain over HTTP | 365 days | **400**. The CLI has no ceiling; it holds the secret. |
| Pairing code lifetime | 60s to 1h, default 10m | Clamped silently, not refused. |
| Request body | 2 MiB; `/v1/call` 16 MiB | **413**. |
| Plugin invocation timeout | 1 hour maximum; 10 minutes in the OpenAI manifest | Before response: **502** `plugin_failed`; during response: the stream ends with an error and the child is killed. |
| Store pool | 16 connections | Requests queue. |

The rate-limit key is the observed address: `iroh:<endpoint id>` over
QUIC, `ip:port` over TCP. Over iroh that is a cryptographic identity and
the bucket means what it says. Over TCP the key includes the source port,
so it is per-connection: a caller opening a fresh connection per request
gets a fresh bucket every time and the limit is close to meaningless
there — one more reason that listener belongs on loopback.

## Watching it

`RUST_LOG` takes standard `tracing` filter syntax and defaults to
`erisdb=info`. Under systemd it all goes to the journal:

```sh
journalctl -u erisdb -f
journalctl -u erisdb-poker.service --since -1h
```

Three lines are worth watching for:

- `rejected capability token` — warn, with the path and the peer. A
  refused token is the only trace of somebody probing, and also what a
  client with a stale token looks like.
- `store failure` — error, carrying the real Postgres message. The caller
  got `the store failed; see the server log` and a 500, because the raw
  text carries constraint names, column names and query fragments. This
  log line is the other half of that trade.
- `refusing an iroh connection` / `refusing a stream` — warn. A limit is
  being hit.
- `plugin unavailable` / `plugin failure` — error. The client gets a generic
  503 or 502; the log names the executable/configuration failure. Stderr is
  drained to avoid deadlock and retained only up to 64 KiB.

Request tracing is on the router, so `RUST_LOG=erisdb=debug,tower_http=debug`
gives a line per request when you need one.

`deploy/erisdb.service` is hardened tightly — `ProtectSystem=strict`, an
empty `CapabilityBoundingSet`, a `@system-service` syscall filter. The one
part that is not obvious is `RestrictAddressFamilies`: `AF_INET` and
`AF_INET6` carry both the TCP listener and iroh's QUIC datagrams, so UDP
must stay open on both families, and `AF_NETLINK` is how iroh enumerates
local interfaces to advertise candidate addresses. Removing either breaks
iroh, quietly, in a way that looks like a network problem.
