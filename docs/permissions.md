# Permissions

An installation, or a capability token, holds a set of **grants**. Every
request names one **required permission**. The request succeeds when some grant
covers it.

```
grant                required permission        covered
tasks:*              tasks:create               yes
tasks:read           tasks:create               no
*                    meta:clients:revoke        yes
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
| `tasks:create`  | create items; initialize the missing `tasks` schema |
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
new client cost zero backend changes: approve its namespace grants on a device,
and let it initialize its missing schema using `newthing:create`. That grant
cannot replace an existing definition or register another namespace. There is
no deploy, no allowlist, and no core release in that path.

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
| `meta:clients:read` | list or inspect installations |
| `meta:clients:write` | change installation permissions, within approved and acting grants |
| `meta:clients:revoke` | revoke an installation |
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

## Approval

Grants are decided on a device, so an app asks and a person answers.

1. **Ask.** The app asks ErisAuth on its device to approve its key for
   `erisdb`, naming the grants it wants:
   `["tasks:read", "tasks:create", "tasks:update"]`.
2. **Approve.** The person sees the app and its request on the device, and
   approves all of it, some of it, or none. Approving takes ErisDB's
   passphrase as well as the tap.
3. **Enroll.** The device signs a note carrying the approved grants and
   enrolls it at ErisDB, which creates the app's installation with exactly
   those grants.

The note is the ceiling. The operator can narrow an installation's grants and
widen them again, never past its note's: `PUT /v1/clients/{id}` refuses
anything the note does not enclose, as it refuses anything the acting operator
does not hold. More takes a new note, which takes the person again. See
[the mesh](mesh.md) for enrollment and [installations](clients.md) for
administration.

A facet registration may carry human descriptions, which an app can show when
it explains what it asks for, in sentences rather than permission strings:

```json
{ "name": "tasks", "version": 1, "schema": { … },
  "permissions": { "read": "read your tasks",
                   "create": "add tasks",
                   "update": "change and complete tasks",
                   "delete": "delete tasks" } }
```

Descriptions are presentation only. A missing one shows the raw permission,
and no grant depends on them.

Installations live in `clients`, outside the items; generic item permissions
cannot edit or revert them.
