# ErisDB

ErisDB is a self-hostable personal data server and universal app backend designed
for personal software ecosystems. It stores app data in a shared postgres database
allowing for easy interop between allowed apps. ErisDB includes and is designed to
be used with the integrated Iroh networking, to allow access by your apps from any
network your client is on. Individual apps can register custom schemas, called 'facets',
to simplify APIs compared to raw database access. Apps can register facets in real time
all without needing to restart the server!

ErisDB also supports plugins, allowing apps to access outside data. Included is a plugin
which forwards an OpenAI endpoint to your apps, allowing you to keep your API key separate
and avoid needing to authenticate every app you want AI features in. Future plans
for bundled plugins include IMAP and CalDAV forwarding, as well as a plugin to
ingest Google Health data.

The [API documentation](docs/api.md) covers every server endpoint, authentication,
pairing, permissions, and CLI commands, with examples. I encourage you to
make your own apps for ErisDB! That's the entire point!

## Install

To start the server and Postgres together and enter an interactive CLI, run:

```sh
./scripts/run-local.py
```

Requires Python 3, Docker running, and a Rust/C build toolchain. The script builds
this checkout and downloads the Postgres image if needed. At `erisdb>`, type
`pair`, `clients list`, `endpoint-id`, or `help` directly. `exit`, Ctrl-D, or Ctrl-C
stops both services. The database and keys stay in `~/.local/share/erisdb/local/`
(under `$XDG_DATA_HOME` when set), ready for the next run. Use `--data-dir PATH`
or `--port 7701` to choose another location or HTTP port.

For a separate server installation, follow the instructions below.
These instructions build the server from source and set up a local PostgreSQL
database. Run commands as your normal user unless they use `sudo`.

### Nix (Linux)

With Nix installed, open a [shell with the required packages](https://nixos.org/manual/nix/stable/command-ref/nix-shell.html):

```sh
nix-shell -p cargo rustc gcc pkg-config postgresql_18 openssl git curl python3
```

Use a current Nixpkgs channel for the Rust toolchain. Run the remaining commands
inside this shell. For a new local PostgreSQL instance:

```sh
export PGDATA="$HOME/.local/share/erisdb/postgres"
initdb --encoding=UTF8 --auth-local=peer --auth-host=scram-sha-256
pg_ctl -l "$PGDATA/server.log" -o "-h 127.0.0.1 -k '$PGDATA'" start
createuser -h "$PGDATA" --pwprompt erisdb
createdb -h "$PGDATA" --owner=erisdb erisdb
```

Choose a database password when prompted. PostgreSQL runs in the background;
use `pg_ctl stop` to stop it. After a reboot, reopen the Nix shell, set `PGDATA`,
and repeat the `pg_ctl ... start` command. Initialize the database only once.

### Debian

Install the build tools and PostgreSQL:

```sh
sudo apt update
sudo apt install -y build-essential pkg-config ca-certificates curl git \
  openssl postgresql python3
sudo systemctl enable --now postgresql
sudo -u postgres createuser --pwprompt erisdb
sudo -u postgres createdb --owner=erisdb erisdb
```

Choose a database password when prompted. Install the current stable Rust
[using rustup](https://doc.rust-lang.org/book/ch01-01-installation.html) if needed:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

### Build and run

From your checkout of this repository, or clone it first:

```sh
git clone git@github.com:Eriskii/ErisDB.git
cd ErisDB
cargo install --locked --path erisdb --bin erisdb
export PATH="$HOME/.cargo/bin:$PATH"
```

Create the configuration once:

```sh
mkdir -p "$HOME/.config/erisdb"
(umask 077; cat > "$HOME/.config/erisdb/server.env" <<EOF_CONFIG
DATABASE_URL=postgres://erisdb:REPLACE_WITH_DATABASE_PASSWORD@127.0.0.1/erisdb
ERISDB_SECRET=$(openssl rand -hex 32)
ERISDB_LISTEN=127.0.0.1:7700
EOF_CONFIG
)
```

Edit `~/.config/erisdb/server.env` and replace the database password placeholder.
URL-encode special characters in the password. Keep this file private and retain
the same secret across restarts. Load it and start the server:

```sh
set -a
. "$HOME/.config/erisdb/server.env"
set +a
erisdb serve
```

The server initializes its tables on first startup. In another terminal, check:

```sh
curl --fail http://127.0.0.1:7700/v1/health
```

For remote HTTP access, put an HTTPS reverse proxy on the same machine in front
of `127.0.0.1:7700`. Android clients connect over Iroh. See
[operations](docs/operations.md) for systemd setup, backups, and configuration.

## Included demo apps

- **Tasks:** one-off and recurring tasks with due dates. Available for
  [browsers](apps/tasks/) and [Android](apps/tasks-android/README.md).
- **Lists:** named lists of entries, with descriptions, links, and custom
  attributes. Available for [browsers](apps/lists/) and
  [Android](apps/lists-android/README.md).

The apps use the same server data across browser and Android installations.
They cache data locally and queue changes while offline.

To try the browser apps locally, run this from the repository root:

```sh
python3 -m http.server 8080 --bind 127.0.0.1 --directory apps
```

Open <http://127.0.0.1:8080/tasks/> or <http://127.0.0.1:8080/lists/>. For remote
hosting, serve `apps/` over HTTPS, keeping `shared/` beside `tasks/` and `lists/`.

In another terminal, load `server.env` as above and create a pairing ticket:

```sh
erisdb pair --name ErisDB --client-url http://127.0.0.1:7700
```

Paste the ticket into the browser app, or scan the QR code/open its deep link
with an Android app. Compare the fingerprint shown in the app and terminal, then
approve the requested permissions or a subset. For a remote browser, replace
`--client-url` with your server's HTTPS URL.

Both browser and Android apps initialize their own schemas automatically with
their `tasks:create` or `lists:create` permission. No manual schema setup or global
facet administration is needed; existing schemas are left unchanged.

Build the Android APKs with `scripts/build-android.sh`; the Android READMEs above
list build requirements. Paired installations renew access automatically until
revoked. Manage them with `erisdb clients list`, `erisdb clients permissions`,
and `erisdb clients revoke` as described in the [API docs](docs/api.md#operator-cli).

A separate [MCP client](clients/mcp/README.md) is also included for MCP hosts.
