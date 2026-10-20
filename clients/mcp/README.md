# ErisDB MCP client

This is a separate client application. An MCP host launches the `erisdb-mcp`
binary over stdio. The application calls ErisDB over Iroh, through
[erisdb-client](../../erisdb-client/README.md), as an app its owner's mesh let
in. The ErisDB server does not import or launch this program.

Build it from the repository root with:

```sh
cargo build --release --manifest-path clients/mcp/Cargo.toml
```

## Sign in

Where ErisAuth runs:

```sh
erisdb-mcp sign-in --grant tasks:read,tasks:create,meta:facets:read
# Approve it on the device and type ErisDB's passphrase there.
erisdb-mcp
```

`sign-in` asks this machine's ErisAuth to let the app into ErisDB with those
permissions, under the name `erisdb-mcp (PROFILE)` unless `--name` says
otherwise. The person approves all of them or fewer on the device and types
ErisDB's passphrase there; ErisAuth enrolls the app and answers. `sign-in` then
keeps the app's key and where ErisDB is in a profile. Starting the client needs
no token, URL or file-path setup.

A profile is one app: its own key, grants and revocation. To give two
applications independent permissions, use `--profile coding` and `--profile chat`
respectively, both when signing in and in each application's MCP launch command.
Without that option, the `default` profile is used. Signing in to the same
profile again keeps its key, so the app stays the same installation with what
the person approved this time. The profile is replaced atomically only once
ErisDB has taken the app; a denial or a cancel leaves it as it was.

On Linux, storage is `$XDG_CONFIG_HOME/erisdb/mcp/PROFILE.json`, defaulting to
`~/.config/erisdb/mcp/PROFILE.json`; other systems use their user configuration
directory. The file holds the app's key, so it has Unix mode 0600, the client
refuses to use one others can read, and nothing in it appears in tool results.
`--profile-file PATH` / `ERISDB_PROFILE_FILE` is an advanced storage override;
`--profile NAME` also accepts `ERISDB_PROFILE`.

The installation lasts until the operator revokes it, the device that approved it
leaves the mesh, or its note is revoked; ErisDB checks on every call, and a tool
call then answers 401. Sign in again to get back in.

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

my_permissions                what this app holds. Needs no permission.
server_state                  version, feed head, counts, limits
```

Success returns the API's JSON as text content; failures (401, 403, 404, 409,
422…) are `isError` tool results carrying the status and body — the model
sees exactly what went wrong and the session keeps going. A transport failure is
an `isError` result too.

`update_item` and `revert_item` require the current revision, as the server does.
A stale revision returns 409. `update_item` replaces the entire body: callers must
read it, preserve fields they want to keep, and overlay their intended changes.
A correct revision does not prevent omitted optional fields from being erased.

`search_items` clamps `limit` to 200 and scans at most 1,000 items in each of
25 facets. It returns `{items, scanned_facets, truncated}` and marks the result
truncated whenever a scan or result limit is reached. Facets outside the app's
grants are skipped; authentication and server failures are returned as errors.

## Permissions

A grant is `namespace:action`. A facet's name *is* its namespace, and the
actions are `read`, `create`, `update`, `delete`:

```
tasks:read          read tasks, their history, their change feed
tasks:create        add tasks, and initialize the missing tasks schema
tasks:*             every action on tasks, including ones added later
meta:facets:read    read facet registrations
meta:facets:write   register or change any facet
meta:feed:read      the change feed across every facet
meta:server:read    server_state
meta:capabilities:mint  mint_capability
*                   everything. A master key.
```

A facet's name carries no schema version, so a grant survives a schema
change. The version lives in the registration body, next to the schema
it describes.

Which tool needs which is the whole access story: sign in with `tasks:read` and
`read_items` works and `create_item` comes back 403. `my_permissions` needs
nothing at all, so an agent can always ask what it holds before trying anything.

## Run

After signing in, launch `erisdb-mcp`. For example, configure Claude Code to
launch the default profile:

```sh
claude mcp add erisdb -- erisdb-mcp
```

For an independent named profile, sign in with `--profile coding`, then include
`--profile coding` in the MCP launch command:

```sh
claude mcp add erisdb -- erisdb-mcp --profile coding
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

# A dashboard: watch the store, touch no data.
--grant meta:server:read,meta:feed:read
```

Two grants deserve a pause before they are typed. `*` is a master key over the
whole store — every facet, every action, plus minting.
`meta:capabilities:mint` lets a conversation cut tokens. Neither is needed to
read and write your data, and the person approving on the device sees exactly
what was asked.

### Environment

```
ERISDB_PROFILE             the app this is. Default: default.
ERISDB_PROFILE_FILE        advanced override for the profile's location.
ERIS_NETWORK               how to reach ErisDB: loopback, a relay URL, or
                          the internet when it names neither.
ERISDB_MCP_ALLOW_MINT      "1" enables mint_capability. Off by default.
ERISDB_MCP_MAX_MINT_TTL    ceiling on the lifetime of any token this server
                          mints, in seconds. Default 86400.
```

### Minting

`mint_capability` returns a working token as tool text. That means it
lands in the transcript, in the client's logs, and in any export of the
conversation — so it is off until the operator turns it on:

```sh
ERISDB_MCP_ALLOW_MINT=1 ERISDB_MCP_MAX_MINT_TTL=3600 erisdb-mcp
```

It also needs `meta:capabilities:mint` among the app's grants. `ttl_secs` is
required and capped at `ERISDB_MCP_MAX_MINT_TTL`, so nothing minted through this
tool lives forever, whatever it asked for. The response reports the lifetime
actually granted. A minted token names this app's installation: it holds no more
than the app does, and stops working when the app is cut off.

## Tests

`cargo test` runs the e2e suite: real Postgres via testcontainers (Docker
required), a real `erisdb` binary joined to a mesh and serving over Iroh, a
device node built from the mesh library in the test process, and the erisdb-mcp
binary as a real subprocess driven over its stdio. ErisAuth is a stand-in on
its agent socket that signs the note and enrolls it from the device's endpoint,
as ErisAuth does once the person says yes. Every endpoint is on loopback. No
mocks.

Nothing in the suite links the erisdb crate — the core is a process, not a
dependency. The tests find it as `ERISDB_BIN`, if that names an executable, or
build `../../erisdb`, the server crate in this repository, on demand. Missing
prerequisites fail the suite; real-core tests are never silently skipped.

## License

MIT. See [LICENSE](LICENSE). The core server is AGPL-3.0; the clients are
deliberately not.
