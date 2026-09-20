# Permissions

A capability token carries a set of **grants**. Every request names one
**required permission**. The request succeeds when some grant covers it.

```
grant                required permission        covered
tasks:*              tasks:create               yes
tasks:read           tasks:create               no
*                    meta:pairing:approve       yes
*:read               meta:facets:read           no
```

## The grammar

A permission is segments joined by `:`.

A required permission is always concrete — the core computes it from the
request and it never contains a wildcard. A grant may contain `*`:

- `*` as a segment matches exactly one segment.
- `*` as the **final** segment of a grant matches all remaining segments.

That second rule is what makes `*` alone mean everything, and `meta:*` reach
three-segment meta permissions. It is also why `*:read` does **not** reach
`meta:facets:read`: its final segment is the literal `read`, so it matches
exactly two segments. Read-everything and administer-everything are
different grants, and neither implies the other.

Segments are lowercase `[a-z0-9][a-z0-9._-]*`, or `*`.

## Facet permissions

For a facet named `tasks`, the namespace is `tasks` and the actions are:

| permission      | what it allows |
|-----------------|----------------|
| `tasks:read`    | read items, list them, read their history, read the facet's change feed |
| `tasks:create`  | create items |
| `tasks:update`  | update items, and revert one to an earlier snapshot |
| `tasks:delete`  | delete items |

Four actions rather than read/write because the useful grants live in
between. An append-only logger holds `sensors:create` and nothing else: it
can add readings and can neither edit nor erase one. That grant cannot be
expressed with a write verb.

**The facet namespace is open.** The core keeps no list of valid
permissions and never checks that a namespace exists. `newthing:read` is a
legal grant the moment it is typed — it simply matches nothing until a facet
called `newthing` is registered, and then it matches. This is what makes a
new client cost zero backend changes: register a facet with
`meta:facets:write`, pair the client, approve the grants it asks for. There
is no deploy, no allowlist, and no core release in that path.

**The core carries grants it does not understand.** A permission whose
namespace is not a facet, or whose action is not one of the four, is stored,
delegated and enclosed like any other — the core just never requires it. A
bridge can define `imap:sync` and enforce it itself, and the token, the
approval screen and the enclosure rules all handle it correctly without the
core knowing what IMAP is.

One-shot plugins make that enforcement concrete. Each operation manifest
names one required permission under the plugin namespace: the shipped
OpenAI operation requires `openai:chat`; a future IMAP executable might
declare `imap:search` and `imap:sync`. `POST /v1/call` reads the manifest,
requires that permission, validates the operation input, and only then
starts the executable. No plugin permission implies any facet permission,
or vice versa, unless a wildcard grant explicitly covers both.

## Meta permissions

`meta` is the one closed namespace: these are core operations, so the core
defines them all.

| permission               | what it allows |
|--------------------------|----------------|
| `meta:facets:read`       | read facet registrations |
| `meta:facets:write`      | register, change and remove facets |
| `meta:server:read`       | server state: version, uptime, limits, live streams, feed head |
| `meta:feed:read`         | the change feed across every facet |
| `meta:pairing:read`      | list pairing requests |
| `meta:pairing:create`    | cut a pairing code |
| `meta:pairing:approve`   | approve or deny a pairing request |
| `meta:pairing:redeem`    | held only by a pairing code itself; redeems it once |
| `meta:capabilities:mint` | mint tokens directly |
| `meta:system:read`       | read tick and lapse rows |
| `meta:system:tick`       | `POST /v1/tick` — the poker's whole job |

The `facet` and `system` meta-facets are reachable *only* through
`meta:facets:*` and `meta:system:*`. They are not ordinary facets with
ordinary namespaces, so there is exactly one name for each thing.

Because `meta` is reserved, a facet may not be named `meta`, and facet names
may not contain `:` or `*`. Registration refuses them. Without that rule a
facet name would be a way to mint meta permissions.

## Enclosure

A token mints another only if the child grants nothing the parent lacks, in
scope **and** in time. Time is unchanged: a child's deadline never exceeds
its parent's.

Scope is subsumption between patterns, not a match against a concrete
permission — the child is itself a pattern. A parent grant subsumes a child
grant when every concrete permission the child could match, the parent
matches too. Comparing segment by segment, with `*` wild on the parent side
and literal on the child side, gives exactly that.

It is deliberately conservative. A parent holding all four of `tasks:read`,
`tasks:create`, `tasks:update` and `tasks:delete` may **not** mint
`tasks:*`, even though the two are equivalent today — because they stop
being equivalent the moment a fifth action exists. Enclosure that depends on
the current action list would silently widen on upgrade.

## Pairing

Pairing is where grants are decided, so an app asks and a human answers.

1. **Cut a code.** `erisdb pair`, or `POST /v1/pairings` with
   `meta:pairing:create`. This creates a pairing session and a one-time
   secret. The secret is an ordinary token holding exactly
   `meta:pairing:redeem`, naming its session, expiring in minutes. It is
   what the QR carries — **not** a capability over your data.
2. **Redeem it.** The client `POST`s `/v1/pair/redeem` with its manifest:
   who it is, and the grants it wants.
   ```json
   { "client": "Tasks (Android) v0.3",
     "requested": ["tasks:read", "tasks:create", "tasks:update"] }
   ```
3. **Approve.** The operator sees the request — in the waiting `erisdb pair`
   terminal, or any app holding `meta:pairing:approve` — and answers. They
   may approve exactly what was asked, select a subset, grant `*`, or deny.
4. **Collect.** The client polls `GET /v1/pair/status` with the same secret
   and receives its token. The session is spent.

A photographed QR is therefore worth nothing on its own: redeeming it
raises a prompt on the operator's screen naming the client, and grants
nothing until someone approves. The approval is the security boundary,
which is where it belongs.

A facet registration may carry human descriptions so the prompt reads in
sentences rather than permission strings:

```json
{ "name": "tasks", "version": 1, "schema": { … },
  "permissions": { "read": "read your tasks",
                   "create": "add tasks",
                   "update": "change and complete tasks",
                   "delete": "delete tasks" } }
```

Descriptions are presentation only. A missing one shows the raw permission,
and no grant depends on them.

## Sessions are items

A pairing session is an item in the `pair` facet, reachable through
`meta:pairing:*`. The core stays stateless: pairing needs no new storage,
survives a restart, replicates across cores, and lands on the change feed
like everything else — so a dashboard watching the feed sees a pairing
request arrive live.
