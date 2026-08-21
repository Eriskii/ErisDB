# bezel-mcp

The core's API as MCP tools — everything a clod needs to read tables,
search, write, delete, and audit a bezel. Built on rmcp (the official MCP
Rust SDK), speaking MCP over stdio; the server is just another client
holding a capability token, so what the token grants is what the tools can
do. Hand it a wildcard token and it can do whatever; hand it a read-only
one and every write comes back as a tool error.

## Tools

```
list_facets                   the store's tables: names, strictness, schemas
read_items                    read one facet, optionally updated_since
get_item                      one item by id
search_items                  substring search, one facet or all of them
create_item                   create (registering a facet = create in "facet")
update_item                   replace body; revision optional (fetched if omitted)
delete_item                   delete; history survives on the feed
item_history                  every state the item has been in
revert_item                   old snapshot as a NEW revision
read_changes                  the change feed, cursor-paged by seq
mint_capability               narrower token (needs admin verb)
```

Success returns the API's JSON as text content; failures (403, 404, 409,
422…) are `isError` tool results carrying the status and body — the model
sees exactly what went wrong and the session keeps going.

## Run

```sh
export BEZEL_URL=http://127.0.0.1:8080
export BEZEL_TOKEN=$(bezel mint --facets '*' --verbs read,write,admin --ttl 86400)
bezel-mcp    # speaks MCP on stdio
```

Claude Code:

```sh
claude mcp add bezel \
  --env BEZEL_URL=http://127.0.0.1:8080 \
  --env BEZEL_TOKEN=bz1.… \
  -- bezel-mcp
```

## Tests

`cargo test` runs the e2e suite: real Postgres via testcontainers (Docker
required), a real bezel over a real socket, and the bezel-mcp binary as a
real subprocess driven over its stdio. No mocks.
