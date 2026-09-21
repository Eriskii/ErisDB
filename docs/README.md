# Documentation

The [root README](../README.md) is the tour: what the pieces are, how to
install them, and the operational facts that matter before you have read
anything else. These pages are the detail behind it.

| page | for |
|------|-----|
| [api.md](api.md) | The complete v1 HTTP reference. Every route, parameter, status code, required permission and error. The reference, and the place to check a claim. |
| [permissions.md](permissions.md) | What a grant is and what it covers. The grammar, the four facet actions, the closed `meta` namespace, enclosure, and why the facet namespace is open. |
| [capabilities.md](capabilities.md) | The token that carries a grant: the two clocks, refresh, enclosure over scope and time, manual refresh chains and registered revocation. |
| [facets.md](facets.md) | What a facet is, how to register one, why its name is a permission namespace, schema validation, lapse rules, and the shipped contracts in full. |
| [plugins.md](plugins.md) | One-shot executable plugins: manifests, operation schemas and permissions, the stdin/stdout protocol, streaming, and the OpenAI Chat Completions plugin. |
| [change-feed.md](change-feed.md) | The feed as bus and audit log. Cursors, ops, history, revert, and the fact that it grows forever. |
| [clients.md](clients.md) | Registered installation identity, independent renewal, revocation, administration and migration. |
| [client-development.md](client-development.md) | How to build one. Pair, sync, write, refresh — and the worked examples in this tree. |
| [pairing.md](pairing.md) | The ticket format: one QR code carrying an address and a code a human turns into a token. |
| [operations.md](operations.md) | Running it. Configuration, migrations, the poker, pairing, secrets, transport, limits, logs. |

## Shortest paths

**I want to run this.** The root README's
[Wiring it up](../README.md#wiring-it-up) has the commands, start to
finish: build, database, service, tokens, poker, apps. Then
[operations.md](operations.md) for what to know once it is up — especially
[what secret rotation costs](operations.md#secrets-and-what-rotation-costs),
which is cheaper to get right at install time than later.

**I want to build a client.** [client-development.md](client-development.md) first: it is the
shape, and it links out to everything it depends on.
[permissions.md](permissions.md) to decide what to ask a human for,
[api.md](api.md) as you go, [pairing.md](pairing.md) if the thing has a
camera. `apps/tasks/index.html` and `apps/shared/auth.js` are a working example.

**I want to understand the design.** [permissions.md](permissions.md) and
[change-feed.md](change-feed.md) are the two ideas the rest follows from —
authority as namespaced grants bounded by revocable installations, and an append-only feed
that is both the bus and the history. [capabilities.md](capabilities.md)
is the lifetime and delegation half of the first;
[facets.md](facets.md) is how the store gets structure without a deploy.
[plugins.md](plugins.md) is the separate, deliberately non-durable path for
computed requests against external systems.
`erisdb/README.md` is the one-page version of all of it.

## The shape, in a paragraph

One Postgres store holds `items` as current truth, `changes` as an append-only,
totally-ordered feed, and `clients` as the installation authority registry. The
core is a stateless process over that store — it verifies a signed capability,
checks installation identity and current grants for registered clients, checks
the request's required permission, validates a body against a facet's JSON
Schema, and writes the item and its history in one transaction. A facet
is a contract registered as an item, and its name is the namespace its
permissions live in, so new structure and the authority over it are both a
`POST` rather than a deploy. Apps, bridges, the poker and Android builds are
clients with tokens. One-shot plugins use those same tokens for authority
but execute as a fresh process per call, outside the store.
