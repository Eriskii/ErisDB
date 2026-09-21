# ErisDB MCP client

This is a separate client application. An MCP host launches the `erisdb-mcp`
binary over stdio. The application calls ErisDB's HTTP API using its own paired
installation credential. The ErisDB server does not import or launch this program.

Build it from the repository root with:

```sh
cargo build --release --manifest-path clients/mcp/Cargo.toml
```

## Pair a connection

```sh
erisdb-mcp pair 'erisdb://pair/…' \
  --grant tasks:read,tasks:create,meta:facets:read
# Compare the fingerprint and approve on the core's terminal.
erisdb-mcp
```

The client remembers its pairing automatically in your user configuration
directory. Restarting it requires no token, URL or file-path setup. To give two
applications independent permissions and revocation, use `--profile coding` and
`--profile chat` respectively, both when pairing and in each application's MCP
launch command. Without that option, the `default` profile is used.

Pairing the same profile with the same core again updates its existing registration.
Its credential is replaced atomically only after approval; denial or cancellation
preserves the old saved login. A profile cannot silently switch to a different core.

On Linux, storage is `$XDG_CONFIG_HOME/erisdb/mcp/PROFILE.json`, defaulting to
`~/.config/erisdb/mcp/PROFILE.json`; other systems use their user configuration
directory. Credential files have Unix mode 0600 and are never returned through
MCP tools. `--session-file PATH` / `ERISDB_SESSION_FILE` remains an advanced storage
override. `--profile NAME` also accepts `ERISDB_PROFILE`. An HTTPS URL is required
remotely; localhost HTTP is accepted. An Iroh-only ticket needs `--url` naming its
HTTPS endpoint because this client uses HTTP.

Access renews after expiry, including after process restart, using the installation
secret in that file. Revocation is checked by the core on each call. Only an explicit
401 triggers renewal and one retry; ambiguous transport failures never repeat a
write. A failed/interrupted pairing requires a fresh ticket, with no manual file
cleanup needed. An existing saved login remains usable with its current permissions.

`ERISDB_URL` plus `ERISDB_TOKEN_FILE`/`ERISDB_TOKEN` configures a manual token with
its original lifetime. For individually revocable,
automatically renewing access, pair instead. See [client administration](../../docs/clients.md).

## Tools

```
list_facets                   the store's tables: names, strictness, schemas
read_items                    read one facet, optionally updated_since
get_item                      one item by id
search_items                  substring search or id lookup, one facet or all
create_item                   create (registering a facet = create in "facet")
update_item                   replace body WHOLE; revision required
delete_item                   confirm=false previews, confirm=true deletes
item_history                  every state the item has been in
revert_item                   old snapshot as a NEW revision; revision required
read_changes                  the change feed, cursor-paged by seq
mint_capability               narrower token; off unless ERISDB_MCP_ALLOW_MINT=1

my_permissions                what this token holds. Needs no permission.
server_state                  version, feed head, counts, limits
list_pairings                 who is asking to pair, and for what
get_pairing                   one request in full
approve_pairing               grant a subset; off unless ERISDB_MCP_ALLOW_APPROVE=1
deny_pairing                  refuse it. No switch: denying only takes away.
```

Success returns the API's JSON as text content; failures (403, 404, 409,
422…) are `isError` tool results carrying the status and body — the model
sees exactly what went wrong and the session keeps going.

`update_item` and `revert_item` require the current revision, as the server does.
A stale revision returns 409. `update_item` replaces the entire body: callers must
read it, preserve fields they want to keep, and overlay their intended changes.
A correct revision does not prevent omitted optional fields from being erased.

`search_items` clamps `limit` to 200 and scans at most 1,000 items in each of
25 facets. It returns `{items, scanned_facets, truncated}` and marks the result
truncated whenever a scan or result limit is reached. Facets outside the token's
permissions are skipped; authentication and server failures are returned as errors.

## Permissions

A grant is `namespace:action`. A facet's name *is* its namespace, and the
actions are `read`, `create`, `update`, `delete`:

```
tasks:read          read tasks, their history, their change feed
tasks:create        add tasks
tasks:*             every action on tasks, including ones added later
meta:facets:read    read facet registrations
meta:facets:write   register or change a facet
meta:feed:read      the change feed across every facet
meta:server:read    server_state
meta:pairing:read   list_pairings, get_pairing
meta:pairing:approve  approve_pairing, deny_pairing
meta:capabilities:mint  mint_capability
*                   everything. A master key.
```

A facet's name carries no schema version, so a grant survives a schema
change. The version lives in the registration body, next to the schema
it describes.

Which tool needs which is the whole access story: hand this server
`tasks:read` and `read_items` works and `create_item` comes back 403.
`my_permissions` needs nothing at all, so an agent can always ask what it
holds before trying anything.

## Run

After pairing, launch `erisdb-mcp`. For example, configure Claude Code to launch
the already-paired default profile:

```sh
claude mcp add erisdb -- erisdb-mcp
```

For an independent named profile, pair with `--profile coding`, then include
`--profile coding` in the MCP launch command:

```sh
claude mcp add erisdb -- erisdb-mcp --profile coding
```

### Manual-token configuration

A manually minted token can be supplied through the environment. Its expiry and
refresh-chain limits apply; the pairing flow above enables renewal until the
installation is revoked.

The core's default listen address is `127.0.0.1:7700`.

```sh
erisdb mint --grant tasks:read,tasks:create,tasks:update,notes:* \
  --ttl 86400 --secret … > ~/.config/erisdb/mcp.token
chmod 600 ~/.config/erisdb/mcp.token

export ERISDB_URL=http://127.0.0.1:7700
export ERISDB_TOKEN_FILE=~/.config/erisdb/mcp.token
erisdb-mcp    # speaks MCP on stdio
```

Claude Code:

```sh
claude mcp add erisdb \
  --env ERISDB_URL=http://127.0.0.1:7700 \
  --env ERISDB_TOKEN_FILE=$HOME/.config/erisdb/mcp.token \
  -- erisdb-mcp
```

### Least privilege, concretely

Name the facets the assistant is actually for and the actions it actually
needs. The four actions exist so the useful grants live between read and
write: a note-taker that must never lose anything holds
`notes:read,notes:create,notes:update` and simply cannot delete.

```sh
# A tasks assistant.
--grant tasks:read,tasks:create,tasks:update

# Read-only across two facets, and the feed for each.
--grant tasks:read,notes:read

# Append-only: it can add and it can neither edit nor erase.
--grant sensors:create

# A dashboard: watch the store, answer pairing requests, touch no data.
--grant meta:server:read,meta:pairing:read,meta:pairing:approve
```

Three grants deserve a pause before they are typed. `*` is a master key
over the whole store — every facet, every action, plus minting and
approving. `meta:capabilities:mint` lets a conversation cut more tokens.
`meta:pairing:approve` lets it admit new clients. None of them is needed
to read and write your data.

### Environment

```
ERISDB_PROFILE             remembered installation profile. Default: default.
ERISDB_SESSION_FILE        advanced override for the saved pairing location.
ERISDB_URL                 the core's base URL for manual-token configuration.
ERISDB_TOKEN_FILE          path to a manually supplied capability.
ERISDB_TOKEN               the capability itself. Used when no file is named.
ERISDB_MCP_ALLOW_MINT      "1" enables mint_capability. Off by default.
ERISDB_MCP_ALLOW_APPROVE   "1" enables approve_pairing. Off by default.
ERISDB_MCP_MAX_MINT_TTL    ceiling on the lifetime of any token this server
                          causes to exist, minted or approved, in seconds.
                          Default 86400.
```

For manual tokens, prefer `ERISDB_TOKEN_FILE`. A token in `--env ERISDB_TOKEN=erisdb1.…` is written
in cleartext into the MCP client's config, ends up in this process's
environment where anything else running as you can read it, and tends to
be copied into shell history and screenshots on the way. A path is a
path; the secret stays in a file whose permissions you chose.

### Minting

`mint_capability` returns a working token as tool text. That means it
lands in the transcript, in the client's logs, and in any export of the
conversation — so it is off until the operator turns it on:

```sh
ERISDB_MCP_ALLOW_MINT=1 ERISDB_MCP_MAX_MINT_TTL=3600 erisdb-mcp
```

`ttl_secs` is required and capped at `ERISDB_MCP_MAX_MINT_TTL`, so nothing
minted through this tool lives forever, whatever it asked for. The
response reports the lifetime actually granted.

### Pairing

A pairing code grants nothing. A client redeems it, says who it is and
which permissions it wants, and **a human answers** — that approval is
the entire reason a photographed QR code is worth nothing. An agent that
approves silently deletes exactly that step, so `approve_pairing` is off
until the operator turns it on:

```sh
ERISDB_MCP_ALLOW_APPROVE=1 erisdb-mcp
```

Reading pairings is not gated: `meta:pairing:read` on the token is the
bound, and the core never puts the issued token in that view — it belongs
to the client that redeemed the code and to nobody else.

Denying is not gated either, and does not need to be. Approving adds
authority; denying only removes it. The worst an agent can do with
`deny_pairing` is cost you the ten seconds it takes to cut another code,
and a session it can always refuse is a session it never has to be
allowed to accept.

Switched on, three things still hold:

- **`granted` is required.** Approving what was asked for is not a
  default that arrives by omission; the permissions get typed out, which
  means they get read.
- **`"*"` is refused outright.** A master key is a keystroke a person
  makes using `erisdb mint`, after reviewing the required authority.
- **The lifetime is capped** by `ERISDB_MCP_MAX_MINT_TTL`, including when
  `ttl_secs` is omitted. An approved token is a credential like a minted
  one, and the core's own default is a week.

## Tests

`cargo test` runs the e2e suite: real Postgres via testcontainers (Docker
required), a real `erisdb` binary serving on a real socket, and the
erisdb-mcp binary as a real subprocess driven over its stdio. No mocks.

Nothing in the suite links the erisdb crate — the core is a process, not a
dependency — so a bare clone of this repo builds and tests on its own. The
tests that need a core find one in this order:

1. `ERISDB_BIN`, if it names an executable.
2. `../../erisdb`, the server crate in this repository, built on demand.
3. `erisdb` on `PATH`.

Missing prerequisites fail the suite; real-core tests are never silently skipped.

## License

MIT. See [LICENSE](LICENSE). The core server is AGPL-3.0; the clients are
deliberately not.
