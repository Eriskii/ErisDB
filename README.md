# persserver

The personal server, in three pieces that match the architecture:

- **`bezel/`** — the core. Stateless Rust process over Postgres; facets,
  capabilities, change feed, tick sweep. See its README.
- **`poker/`** — the clock. A curl in a systemd timer. Knows one URL, one
  token, nothing else.
- **`apps/tasks/`** — the first client. One HTML file: local cache in
  localStorage, cursor-based sync against the change feed, revision-safe
  writes, and due notifications fed by both the poker's lapse sweep and a
  local due-check between ticks.
- **`apps/lists/`** — lists of stuff. Same one-file shape as tasks. Each
  entry is `list` + `name`, with optional description, link, and a flat
  frontmatter-style attributes map; lists are implicit — a list is the set
  of entries naming it. Added/modified timestamps ride the item envelope.
- **`bezel-client/`** — a Rust client that dials a bezel over Iroh by
  endpoint id; async core, blocking facade for FFI, JNI bindings for
  Android.
- **`apps/lists-android/`** — the lists client as an Android app over
  `bezel-client`: no IP, no port, just the server's iroh endpoint id and
  a token. See its README.

## Wiring it up

```sh
# core
export DATABASE_URL=postgres://…  BEZEL_SECRET=…
bezel serve

# tokens
bezel mint --facets '*' --verbs read,write,admin --ttl 86400   # for the app
bezel mint --facets system --verbs write --ttl 0               # for the poker

# poker (or just run poker/poke.sh from any cron)
cp poker/poke.sh /usr/local/bin/ && cp poker/bezel-poker.* /etc/systemd/system/
systemctl enable --now bezel-poker.timer

# apps: serve apps/tasks/ or apps/lists/ statically (or open index.html),
# paste url + token
```
