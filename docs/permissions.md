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

Operation manifests name their required permissions. The shipped OpenAI
operation requires `openai:chat`. `POST /v1/call` checks that permission and
validates the input before starting the executable. This grant gives no access
to stored items; facet permissions are checked separately.

## Meta permissions

`meta` is the one closed namespace: these are core operations, so the core
defines them all.

| permission               | what it allows |
|--------------------------|----------------|
| `meta:facets:read`       | read facet registrations |
| `meta:facets:write`      | register, change and remove facets |
| `meta:server:read`       | server state: version, uptime, limits, live streams, feed head |
| `meta:feed:read`         | the change feed across every facet |
| `meta:clients:read` | list or inspect registered installations |
| `meta:clients:write` | change installation permissions, within requested and acting grants |
| `meta:clients:revoke` | revoke an installation |
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
   `meta:pairing:create`. This creates a pairing session and a short-lived
   secret. The secret is an ordinary token holding exactly
   `meta:pairing:redeem`, naming its session, expiring in minutes. It is
   what the QR carries — **not** a capability over your data.
2. **Redeem it.** The client `POST`s `/v1/pair/redeem` with its manifest:
   who it is, and the grants it wants. Over Iroh, the authenticated persistent
   key supplies the installation identity:
   ```json
   { "client": "Tasks (Android) v0.3",
     "requested": ["tasks:read", "tasks:create", "tasks:update"] }
   ```
   HTTP clients must also send an S256 `challenge` derived from their saved
   installation secret. The [redemption example](api.md#post-v1pairredeem) shows
   its exact encoding and the required authorization header.
3. **Approve.** The operator sees the request — in the waiting `erisdb pair`
   terminal, or any app holding `meta:pairing:approve` — and answers. They
   may approve exactly what was asked, select a subset, or deny. Both screens must show the same comparison fingerprint.
4. **Collect.** The client polls `GET /v1/pair/status` with the pairing code and installation proof,
   and receives its token. Only that installation can recover the response.

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
`meta:pairing:*`. Pairing survives a restart, is shared across cores through Postgres, and lands on the change feed
like everything else — so a dashboard watching the feed sees a pairing
request arrive live.

Durable registrations live separately in `clients`; generic item permissions
cannot edit or revert them. See [clients.md](clients.md).
