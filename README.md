# ErisDB

ErisDB is a personal data server. This repository contains the core, clients,
apps, and deployment files:

- **`erisdb/`** — the core. Stateless Rust process over Postgres; facets,
  capabilities, change feed, tick sweep. See its README.
- **`poker/`** — the clock. A curl in a systemd timer. Knows one URL, one
  token, nothing else.
- **`apps/tasks/`** — the first browser client, with shared authentication in
  `apps/shared/`: local cache in
  localStorage, cursor-based sync against the change feed, revision-safe
  writes, and due notifications fed by both the poker's lapse sweep and a
  local due-check between ticks.
- **`apps/lists/`** — lists of stuff. Same browser structure as tasks. Each
  entry is `list` + `name`, with optional description, link, and a flat
  frontmatter-style attributes map; lists are implicit — a list is the set
  of entries naming it. Added/modified timestamps ride the item envelope.
- **`erisdb-client/`** — a Rust client that dials ErisDB over Iroh by
  endpoint id; async core, blocking facade for FFI, JNI bindings for
  Android.
- **one-shot plugins** — executable operations loaded from deployment
  manifests. One request starts one process and streams its stdout; no call
  touches Postgres and no plugin survives between requests. The shipped
  `erisdb-plugin-openai` forwards Chat Completions, including tools and SSE.
- **`clients/mcp/`** — a separate client application. An MCP host launches it
  over stdio; it makes authenticated HTTP requests to ErisDB. It has its own
  binary and build. See its [README](clients/mcp/README.md).
- **`apps/tasks-android/`** — the tasks client as an Android app over
  `erisdb-client`: one-off and recurring tasks with due dates on
  `tasks`; completing a repeating task advances its due date instead
  of finishing it. See its [README](apps/tasks-android/README.md).
- **`apps/lists-android/`** — the lists client as an Android app over
  `erisdb-client`, using QR/deep-link pairing and its own Iroh installation
  identity. See its [README](apps/lists-android/README.md).
- **`deploy/`** — how the core runs on a machine: a systemd unit and an
  environment file to fill in.

## Documentation

This file is the tour. [`docs/`](docs/) is the detail:
[api.md](docs/api.md) documents the server API with examples,
[permissions.md](docs/permissions.md) what a grant is and what it covers,
[capabilities.md](docs/capabilities.md) the token that carries one,
[facets.md](docs/facets.md) how the store gets structure without a deploy,
[change-feed.md](docs/change-feed.md) the feed that is both bus and audit
log, [clients.md](docs/clients.md) installation identity and administration,
[client-development.md](docs/client-development.md) how to build a client,
[plugins.md](docs/plugins.md) how one-shot external operations are installed
and invoked,
[pairing.md](docs/pairing.md) the ticket format, and
[operations.md](docs/operations.md) what to know while it is running.
[`docs/README.md`](docs/README.md) indexes them and names the shortest
path through for each of those three jobs.

## Wiring it up

The core is one binary against one Postgres database. The poker, apps and MCP
client make authenticated API requests. One-shot plugins are the other
path: operator-installed executables launched once per authorized call, with
no database involvement.

### The core

```sh
# build and install the binary (everything below runs from the repo root)
cargo build --release --manifest-path erisdb/Cargo.toml
sudo install -m 0755 erisdb/target/release/erisdb /usr/local/bin/erisdb

# the store; `erisdb serve` runs the migrations itself on startup
sudo -u postgres createuser erisdb
sudo -u postgres createdb -O erisdb erisdb

# the service account and its secrets
sudo useradd --system --no-create-home --shell /usr/sbin/nologin erisdb
sudo install -d -m 0755 /etc/erisdb
sudo install -m 0600 deploy/erisdb.env.example /etc/erisdb/erisdb.env
sudoedit /etc/erisdb/erisdb.env    # DATABASE_URL, ERISDB_SECRET, ERISDB_LISTEN

# the unit
sudo install -m 0644 deploy/erisdb.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now erisdb
curl -fsS http://127.0.0.1:7700/v1/health
```

`/etc/erisdb/*.env` stays mode 0600 root-owned: systemd reads it as the
manager, before dropping to the `erisdb` user, so the service account never
needs to see the secret on disk.

### Tokens and the address

Both come out of `ERISDB_SECRET`, so run these as root, with the environment
file sourced:

```sh
set -a; . /etc/erisdb/erisdb.env; set +a

erisdb endpoint-id                                    # what clients dial
erisdb mint --grant '*' --ttl 3600                    # an operator's token
erisdb mint --grant meta:system:tick --no-expiry      # for the poker
```

`erisdb endpoint-id` is a pure derivation of the secret — the same answer
whether or not the server is running, and the same answer after every
restart.

### One-shot plugins

The OpenAI plugin is built with the core but installed as a separate
executable. It is never a service:

```sh
sudo install -d -m 0755 /usr/local/libexec/erisdb /etc/erisdb/plugins.d
sudo install -m 0755 erisdb/target/release/erisdb-plugin-openai \
  /usr/local/libexec/erisdb/
sudo install -m 0644 deploy/plugins/openai.json /etc/erisdb/plugins.d/
sudoedit /etc/erisdb/erisdb.env    # ERISDB_PLUGIN_DIR, OPENAI_API_KEY
sudo systemctl restart erisdb
```

Each `POST /v1/call` validates the manifest's permission and JSON Schema,
starts a fresh executable, pipes the input through stdin, and streams stdout
back. Calls, prompts and responses are never written to Postgres. See
[docs/plugins.md](docs/plugins.md) for the manifest and process protocols.

A token carries **grants**: `tasks:read`, `tasks:*`, `*:read`,
`meta:facets:write`, `*`. `--grant` is repeatable and splits on commas.
Apps do not need one of these: pair them instead, with

```sh
erisdb pair --name my-laptop
```

which shows a QR code, lets the client say what it wants, and asks you.
[docs/permissions.md](docs/permissions.md) is the grammar and
[docs/pairing.md](docs/pairing.md) is the ticket.

### The poker

```sh
sudo install -m 0755 poker/poke.sh /usr/local/bin/
sudo install -m 0644 poker/erisdb-poker.service poker/erisdb-poker.timer /etc/systemd/system/
sudo install -m 0600 poker/poker.env.example /etc/erisdb/poker.env
sudoedit /etc/erisdb/poker.env    # ERISDB_URL, ERISDB_POKER_TOKEN
sudo systemctl daemon-reload
sudo systemctl enable --now erisdb-poker.timer
```

`poke.sh` is a curl with two variables; any cron that can set them works
just as well as the timer.

### The apps

Serve the `apps/` directory statically over HTTPS (HTTP is supported on localhost),
preserving `shared/` alongside `tasks/` and `lists/`. Open either app and give it
a ticket: scan the QR `erisdb pair` prints, or
paste the `erisdb://pair/…` string. The app redeems it, asks for the
permissions it needs, and waits while you answer in the terminal. Pasting
a URL and a token still works for a token you already hold. The Android
clients take an iroh endpoint id instead of a URL — no IP, no port.

## Backup and restore

Postgres is the only stateful thing. Back it up and you have backed up the
system: the core holds nothing a restart would lose, and the iroh identity
is derived from `ERISDB_SECRET` rather than stored anywhere.

```sh
# DATABASE_URL comes from the environment file:
#   set -a; . /etc/erisdb/erisdb.env; set +a

# dump: custom format, compressed, restorable selectively
pg_dump --format=custom --file="erisdb-$(date +%F).dump" "$DATABASE_URL"

# restore into an empty database
sudo -u postgres createdb -O erisdb erisdb
pg_restore --dbname="$DATABASE_URL" --no-owner erisdb-YYYY-MM-DD.dump
```

Three things to keep straight:

- **Restore the whole database together.** `items` is current truth; `changes`
  is the append-only feed every sync client reads by cursor; `clients` holds
  installation permissions and revocation. Partial restores can break sync or
  restore inconsistent authority. An older backup can restore registrations
  revoked after that backup.
- **`changes` grows without bound.** Every write snapshots the full body it
  produced, deletes keep the bodies that came before them, and the poker
  adds a `tick` row a minute. Nothing prunes any of it — the feed *is* the
  history. A dump therefore contains every version of everything ever
  written, including things since deleted; size and handle it accordingly.
- **A dump is half the system.** Without the same `ERISDB_SECRET`, every
  token minted against the old deployment fails and the server comes back
  at a different address. Back the secret up separately — and not inside
  the dump.

## Secrets

`ERISDB_SECRET` does two jobs at once:

1. It is the HMAC key that signs and verifies every capability token.
2. It derives the server's iroh endpoint id — its permanent address.

```sh
openssl rand -hex 32
```

Rotating it therefore does two irreversible things to everything already
deployed:

- **Outstanding token signatures become invalid.** Registered installations
  can renew using their installation proof. Manual tokens must be re-minted.
- **The server's address changes.** Each Android client and
  each `erisdb-client` caller is pinned to the endpoint id derived from the
  old secret, and will dial an address nobody answers. Print the new one
  with `erisdb endpoint-id` and re-pin every one of them.

Revoke one paired installation with `erisdb clients revoke CLIENT_UUID`.
This immediately blocks subsequent access, renewal and active change feeds.
Manual tokens without a registration still require expiry or signing-key
rotation. [Client administration](docs/clients.md) explains renewal and revocation.


## Transport

Two paths into the same router, with very different properties.

- **Iroh** (ALPN `erisdb/0`, HTTP/1.1 per QUIC bi-stream) is authenticated
  and encrypted end to end. Anyone may connect; nobody reads or writes
  without a token.
- **Plain TCP is neither.** `--listen` defaults to `127.0.0.1:7700` and
  belongs on loopback. Capability tokens are bearer credentials in an
  `Authorization` header, CORS is permissive, and this path has no
  transport security: binding it to a public interface publishes every
  token that crosses it to anyone on the wire.

If the TCP listener has to be reachable off-host, put TLS in front of it —
a reverse proxy terminating HTTPS into `127.0.0.1:7700`. Otherwise leave it
on loopback and dial over iroh, which is what the Android clients do and
what needs no open port at all.

## Tests

`erisdb/`, `erisdb-client/`, and `clients/mcp/` each carry a suite. They are separate crate
trees with no workspace root, so each runs from its own directory:

```sh
(cd erisdb && cargo test)
(cd erisdb-client && cargo test)
(cd clients/mcp && cargo test)
```

The suites bring up a real Postgres through testcontainers and talk to it over
real sockets, so **Docker must be running**. `erisdb`'s e2e suite also
exercises real Iroh QUIC. No mocks.

CI runs all three suites plus `cargo clippy -- -D warnings` and
an advisory `cargo fmt --check` on every push and pull request.

## License

The core is copyleft; the clients are not.

- **`erisdb/`** — GNU Affero General Public License v3.0, full text in
  [`erisdb/LICENSE`](erisdb/LICENSE) and
  [`LICENSE-AGPL-3.0`](LICENSE-AGPL-3.0). Running a modified core as a
  network service obliges you to offer its source to that service's users.
- **Everything else here** — MIT, full text in
  [`LICENSE-MIT`](LICENSE-MIT): `erisdb-client/`, `poker/`, `deploy/`,
  `apps/tasks/` and `apps/lists/`.
- **The MCP and Android clients** — MIT, carried in their own trees:
  `clients/mcp`, `apps/tasks-android`, `apps/lists-android`.

Copyright (c) 2026 Isolyth.

There is no single `LICENSE` at the repo root on purpose: this tree holds
both licenses, and one file there would say the wrong thing about half of
it.
