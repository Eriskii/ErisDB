# Operations

Running an ErisDB. The [root README](../README.md#install) has the
install commands; this page is what to know while it is running.

`erisdb serve` stores items, history, installations, its place in the mesh and
the passphrase in Postgres. The poker calls the tick endpoint on a timer.
In-memory caches, rate limits, active connections and each replica's copy of the
device list in memory belong to each running server process.

## Local launcher

From the checkout, run `./scripts/run-local.py` where `eon` runs. It builds ErisDB
exactly as its lockfile names, whatever Cargo configuration surrounds the checkout, starts a
dedicated Postgres 17 Docker container, starts the server, and opens an `erisdb>` prompt. The
server loads plugin manifests from `plugins/` in the data directory, or from
`ERISDB_PLUGIN_DIR` when that is set. Python 3, Docker access, Cargo, and a C build toolchain
are required on Linux.

The first time, the server is in no mesh yet: the launcher runs `erisdb join`, which asks
this machine's `eon` to make it the mesh's database — approve it on the device — and then
`erisdb passphrase set`, which asks for the passphrase devices type to let apps in. Later
launches find both in the database.

Type the existing CLI subcommands directly, for example:

```text
erisdb> endpoint-id
erisdb> clients list
erisdb> passphrase set
erisdb> exit
```

The prompt accepts quoted arguments and passes them to the ErisDB binary; it does
not interpret shell pipes or variable expansion. `help` displays the CLI help.
`passphrase set` asks twice without echo.

The default data directory is `${XDG_DATA_HOME:-~/.local/share}/erisdb/local/`.
`--data-dir PATH` selects another directory, and `--port PORT` changes the HTTP
port from 7700. The launcher refuses an occupied port or a data directory already
in use by another launcher. Both HTTP and Postgres TCP listeners bind to localhost;
the server also starts its normal encrypted Iroh endpoint.

The directory contains `postgres/` with the database files, `config.json` with the
server key and database password, `server.log`, `postgres.log`, and a lock file.
The directory is private to your user and the configuration file has mode 0600.
Postgres owns its data files inside the bind mount. Data is written continuously;
shutdown stops Postgres cleanly, then removes the container while retaining those
files. Keep the configuration and database together so future sessions retain
their server identity and credentials.

`exit`, EOF/Ctrl-D, Ctrl-C, SIGHUP, and SIGTERM close the session and stop its
processes. Startup failures also clean up services already started. Logs remain
in the data directory for diagnosis. To copy the directory as a filesystem backup,
stop the launcher first; normal live backups use `pg_dump` as described below.

## Systemd service (Debian)

After building the server and creating the database using the README, join the
mesh and set the passphrase as yourself, where `eon` runs, with the server
configuration loaded:

```sh
set -a; . "$HOME/.config/erisdb/server.env"; set +a
erisdb join
erisdb passphrase set
```

Then install the binary and the supplied unit from the repository root:

```sh
sudo install -m 0755 "$HOME/.cargo/bin/erisdb" /usr/local/bin/erisdb
sudo useradd --system --no-create-home --shell /usr/sbin/nologin erisdb
sudo install -d -m 0755 /etc/erisdb
sudo install -m 0600 "$HOME/.config/erisdb/server.env" /etc/erisdb/erisdb.env
sudo install -m 0644 deploy/erisdb.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now erisdb
curl --fail http://127.0.0.1:7700/v1/health
```

Stop any foreground `erisdb serve` process before starting the service. Keep the
same database URL and secret. Edit service configuration with
`sudoedit /etc/erisdb/erisdb.env`, then `sudo systemctl restart erisdb`.
Read logs with `sudo journalctl -u erisdb`.

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
| `ERIS_NETWORK` | `--network` | `internet` | How the core reaches the mesh: `internet`, a relay URL, or `loopback` (tests). |
| `ERISDB_LOCKOUT_SECS` | `--lockout-secs` | `60` | The wait after a device's third wrong passphrase in a row; each further wrong guess doubles it, up to a day. |
| `RUST_LOG` | — | `erisdb=info` | Standard `tracing` filter syntax. |

On start it loads plugin manifests when configured, connects the pool (16
connections per replica), runs the migrations, and reads its note: without one
it refuses to start, saying to run `erisdb join`, and with one for another key
it refuses too. It binds the iroh
endpoint under that key and starts its mesh node, which syncs with the mesh at
once; binds the TCP listener; and serves both until one of them stops, with
`eris/database/1`, `eris-mesh/0` and `eris-enroll/0` on the one endpoint.
Plugin secrets such as `OPENAI_API_KEY` are ordinary service environment
variables; each manifest explicitly allowlists which ones its fresh process
receives.

`deploy/erisdb.service` passes `--listen ${ERISDB_LISTEN}` explicitly, so
that variable must be set in `/etc/erisdb/erisdb.env`: unset, systemd
expands it to nothing and the binary is handed a `--listen` with no value
and refuses to start.

Both secrets are marked so that `--help` and error output never echo them.

### The core is replaceable

Nothing in the process is durable. Restart it, run three of them behind a
load balancer, kill one mid-request — the store is the truth and every
replica reads it. The following state is per replica:

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
- **The device list in memory.** Each replica's mesh node holds the list and
  saves it to the `mesh` row; on restart it loads the row and syncs with the
  mesh, which brings it whatever it missed.

The e2e suite checks this: two replicas over one store, an item written
through one and read through the other immediately.

## Database initialization

`erisdb serve`, `erisdb join` and `erisdb passphrase` run the embedded migrations
before anything else. `0001_init.sql` creates `items` and `changes`, the facet
definition and the `safe_ts` function; `0002_mesh.sql` creates `clients`, `mesh`,
`passphrase` and `passphrase_guesses`. SQLx records each migration's checksum and
verifies it on subsequent starts. Concurrent starts are serialized by SQLx's
database lock. A schema error stops startup.

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

## Letting apps in

Apps get in through a device, not the server: an app asks a device to use
`database/1`, the person approves it there and types the passphrase, and the
device enrolls it here over `eris-enroll/0`. The operator's part is the
passphrase and the registry:

- **`erisdb passphrase set`** sets the passphrase, or resets a forgotten one.
  Devices can change it themselves by knowing it (`eon passphrase`).
- **`erisdb passphrase unlock [DEVICE]`** clears a device's wrong guesses
  (by endpoint id), or every device's, when someone fat-fingered their way into
  a long wait.
- **`erisdb clients`** lists, narrows and revokes installations.

A device removed from the mesh, or an app's note revoked, ends that app here at
once, with nothing for the operator to do. See [the mesh](mesh.md).

## Backup and restore

With `DATABASE_URL` loaded from the server environment file, back up the database:

```sh
pg_dump --format=custom --file="erisdb-$(date +%F).dump" "$DATABASE_URL"
```

To restore, stop ErisDB and point `DATABASE_URL` at an empty database owned by
the ErisDB database role, then run:

```sh
pg_restore --dbname="$DATABASE_URL" --no-owner erisdb-YYYY-MM-DD.dump
```

Restore the deployment configuration and secrets before starting ErisDB again.
Keep these properties in mind:

1. **Restore the whole database together.** `items`, the `changes` feed, the
   `clients` registry, `mesh` and `passphrase` must agree. Partial restores can
   break sync or restore inconsistent authority. Restoring an older backup can
   also restore installations the operator revoked since that backup; the
   mesh's removals and revocations come back as soon as the core syncs.
2. **A dump contains everything ever written**, including every prior
   version and everything since deleted, because the feed *is* the
   history and nothing prunes it.
3. **A dump is half the system.** Without the same secrets, every token
   fails and the server comes back under another key, which its note does not
   approve. Back the secrets up separately, and not inside the dump.

The server stores its durable data in Postgres. Its Iroh key is derived from the
configured secret. Deployment manifests and secrets must be backed up separately
from the database.

## Secrets and what rotation costs

`ERISDB_SECRET` signs every capability token. `ERISDB_IROH_SECRET` seeds the
iroh endpoint identity — and when it is unset, `ERISDB_SECRET` does that
job too.

```sh
openssl rand -hex 32
```

Both are pure derivations, so `erisdb endpoint-id` answers the same whether
or not the server is running, and the same after every restart. The key a
device approved stays the core's key.

**Set them separately, at install time.** `deploy/erisdb.env.example` ships
with only `ERISDB_SECRET`, which is the simple case and also the case where
the two rotations are welded together. Adding a second line now costs
nothing and is the difference between a future rotation that re-mints
tokens and one that also takes the core out of its mesh.

Rotating `ERISDB_SECRET` does exactly one irreversible thing, or two:

- **Outstanding token signatures become invalid.** Operator and minted tokens
  must be re-minted. Apps hold no tokens and carry on.
- **The key moves too, unless `ERISDB_IROH_SECRET` is set separately.** The
  core's note approves the old key, so it refuses to serve until `erisdb join`
  has a device approve the new one, and every app then finds it anew as the
  mesh's database.

Revoke one installation with `erisdb clients revoke CLIENT_UUID`. This
immediately blocks the app's requests and active change feeds. Tokens without an
installation still require expiry or signing-key rotation.
[Installations](clients.md) explains administration and revocation.

`/etc/erisdb/*.env` stays mode 0600 and root-owned: systemd reads it as the
manager, before dropping to the `erisdb` user, so the service account never
needs to see the secret on disk.

## Transport posture

The HTTP and Iroh listeners serve the same API, with different transport security:

**Iroh is the reachable path.** ALPN `eris/database/1`, HTTP/1.1 per QUIC
bi-stream, authenticated and encrypted end to end by the transport, with
no port to forward and no certificate to manage. The endpoint id is public
and stable by design, so anyone may dial — and nobody reads or writes
without an installation for their key, or a token. This is what apps use. The
same endpoint serves `eris-mesh/0`, where devices keep the core's device list
current, and `eris-enroll/0`, where devices let apps in and change the
passphrase.

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

`GET /v1/health` needs no credential. Every other route requires an enrolled
key over Iroh or a bearer capability. Enrollment and passphrase changes are not
routes: they take a device of the mesh, over `eris-enroll/0`. Health answers from the process without touching the
store, so it is liveness, not readiness — a core with a dead database still
answers `{"ok": true}`.

## Limits

Bounds the core enforces, and what hitting one looks like:

| bound | value | at the limit |
|-------|-------|--------------|
| Live change streams | 32 | **503** `unavailable`. Each stream holds a Postgres connection outside the pool, so this bounds the store as much as the process. |
| One-shot plugin processes | 32 | **503** `unavailable`. Per replica; a process occupies one slot until it exits or its response is dropped. |
| Iroh connections | 64 | The connection is dropped, with a warning logged. Bounded *before* authentication, because the endpoint id is public and a capability check costs more than a refusal. |
| Iroh streams per connection | 32 | The stream is dropped, with a warning naming the peer. |
| Capability endpoints | burst 10, then 1 per 5s | **429** `too_many_requests`, per caller, per replica. Checked before authorization, so a caller that cannot mint at all still sees 429 rather than 403. |
| `X-ErisDB-Client` | 128 chars, printable ASCII | **400**. It lands in every change row this caller writes. |
| Grants per token / grant length | 64 / 128 chars | **400** at mint. |
| Refresh chain over HTTP | 365 days | **400**. The CLI has no ceiling; it holds the secret. |
| Passphrase guesses | 3 free per device, then 1 minute doubling to a day | The device hears `locked` with `retry_after`, and nothing it sends is checked. In Postgres, so a restart forgives no one. |
| Request body | 2 MiB; `/v1/call` 16 MiB | **413**. |
| Plugin invocation timeout | 1 hour maximum; 10 minutes in the OpenAI manifest | Before response: **502** `plugin_failed`; during response: the stream ends with an error and the child is killed. |
| Store pool | 16 connections | Requests queue. |

The rate-limit key is the authenticated `iroh:<endpoint id>` over QUIC and
the actual peer IP over TCP. Opening new TCP connections does not reset a bucket.
Clients behind the same local TLS proxy share its bucket; forwarded headers do
not override the observed peer. Buckets are per replica and cover minting and
token refresh.

## Watching it

`RUST_LOG` takes standard `tracing` filter syntax and defaults to
`erisdb=info`. Under systemd it all goes to the journal:

```sh
journalctl -u erisdb -f
journalctl -u erisdb-poker.service --since -1h
```

The server logs these events:

- `rejected capability token`, `rejected an unenrolled key`, `rejected an app
  the mesh no longer counts` — warn, with the path and the peer. A refused
  credential is the only trace of somebody probing, and also what an app cut
  off by the mesh looks like.
- `enrolled` and `revoked by the mesh` — info, naming the installation and app.
  `wrong passphrase` — warn, naming the device and its count.
- `this ErisDB's approval no longer counts` — error. The mesh approved another
  database, or the device that approved this one left. Apps find the other one,
  and this one lets no more apps in and takes no passphrase change; `erisdb join`
  makes it the mesh's database again. `this ErisDB's approval counts again` —
  info, when it does.
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
