# Capabilities

A capability is a signed upper bound on a request's authority. Registered
installation tokens also carry a `client` UUID: the core checks that registration
in Postgres on every request, verifies its transport binding and intersects its
current grants with the token's grants. See [clients.md](clients.md).

Manual/operator tokens have no registration. Their signature, scope and two
clocks remain sufficient for authorization. The bounded refresh and delegation
rules below describe those tokens; paired installations have independent renewal
credentials and renew until revoked.

## The token

```
erisdb1.<b64url(payload)>.<b64url(hmac_sha256(secret, payload))>
```

Three dot-separated parts, base64url without padding. The payload is JSON:

```json
{
  "grants": ["tasks:read", "tasks:create"],
  "exp": 1788133957,
  "user": "agent-1",
  "max_exp": 1790121133
}
```

| field | meaning |
|-------|---------|
| `grants` | The permission patterns this token holds. Always present, never empty. |
| `exp` | Unix seconds. When *this* token stops working. Null on a `--no-expiry` token. |
| `max_exp` | Unix seconds. The end of the refresh chain. Omitted when unset. |
| `user` | A signed identity, stamped into `source.user`. Attribution, not privilege. Omitted when unset. |
| `client` | Registered installation UUID, inherited by delegated tokens; enables immediate revocation. |
| `pair` | The pairing session this token redeems. Only a pairing code carries one. Omitted otherwise. |

Signed, not encrypted. Anyone holding a token can read its scope, and
clients are expected to — that is how an app knows when to refresh.
`GET /v1/permissions` answers the same question without any base64, for
a client that would rather ask than parse.

Verification is: three parts, prefix `erisdb1`, both segments decode, the HMAC
matches in constant time, the payload parses as a capability, `exp` is in
the future, and the chain end is in the future. Anything else is a flat 401
with no hint as to which. A refused token is logged with the path and the
peer — it is the one thing an operator most wants a record of, and the
only trace of somebody probing.

## Grants, briefly

A token holds patterns; a request requires one concrete permission; the
request succeeds when some pattern covers it. `tasks:read`, `tasks:*`,
`*:read`, `meta:facets:write`, `*`. Segment by segment, `*` matching one
segment, and a trailing `*` matching all remaining ones.

Three properties of that design matter to everything below.

**The namespace is open.** The core keeps no registry of valid
permissions and never checks that a namespace exists. `sensors:create` is
a legal grant the moment it is typed. It permits initializing the missing
`sensors` schema and creating readings, without authority to change existing
schemas or initialize other namespaces. A new client needs no deploy,
allowlist entry, or core release. The app E2E tests exercise this from a fresh
installation with no app schemas pre-registered.

**Operation permissions use the same grant syntax.** For example, the shipped
OpenAI operation requires `openai:chat`. It is delegated and bounded by the same
rules as facet permissions.

**`meta` is closed.** Core operations live there and only there, facet
names may not reach into it, and `*:read` — read everything — deliberately
does not cover `meta:facets:read`. Reading all your data and administering
the deployment are different grants, and neither implies the other.

The grammar, the four facet actions, the full meta list and the naming
rules are all in [permissions.md](permissions.md).

## The two clocks

Every expiring token carries two deadlines.

- **`exp`** — when *this* token stops working.
- **`max_exp`** — when its *line* stops working. The end of the refresh
  chain.

`exp` moves. `max_exp` does not. Refresh trades a live token for one with
the same grants and a later `exp`, clamped to `max_exp`. Once `max_exp`
passes, every token descended from that line is dead and a human mints a
new one.

The core's deadline for a token is `max_exp` if it has one, else `exp`.
Verification refuses a token past either clock, so a payload with a live
`exp` and a dead `max_exp` is a 401.

A token with no `exp` never expires and has no chain. Only the CLI mints
one, because only the CLI holds the secret.

## What refresh does and does not do

`POST /v1/capabilities/refresh` takes `{ttl_secs}` and needs nothing but a
valid token — no special permission, no `meta:capabilities:mint`. It is how
an app running on a phone or in a browser outlives the lifetime an operator
chose without that operator being present.

It moves time, and only time:

- Grants and the signed `user` carry over byte for byte. A read-only token
  refreshes into a read-only token.
- The new `exp` is `min(now + ttl_secs, max_exp)`. Asking for a day when
  four hours remain returns four hours and a 201, not a refusal — the
  response reports the `exp` and `chain_ends` you actually got.
- `max_exp` is copied, never recomputed. Refreshing a token twenty times
  does not walk the ceiling forward by an inch.
- An expired token cannot be refreshed. Verification refuses it before the
  handler runs. Refresh keeps a session alive; it does not resurrect one.

The consequence worth internalising: **a leaked token buys the holder the
rest of its chain and no more.** That is the entire revocation story for
one token, and it is why the chain length is the number to be careful
with. The HTTP default is 30 days and the hard ceiling is a year.

A `--no-expiry` token is the exception. It has no chain to run out, so
refreshing it returns a *bounded* token — the trade only ever narrows.
Daemons holding one should not call the route.

## Enclosure covers scope and time

Minting over HTTP is delegation, never issuance. The core requires that
the caller's own capability **encloses** the one it is asking for: every
grant in the child is subsumed by some grant in the parent, and the child's
deadline is at or before the parent's.

The time clause is the one people forget, and it is the one that keeps
delegation honest. A ten-minute token is ten minutes of authority
*including everything it delegates*. Without it, any short-lived token
holding `meta:capabilities:mint` could launder itself into a permanent one
by minting a child and throwing the parent away.

### Subsumption, not matching

The scope clause compares two *patterns*. The child is not a concrete
permission the parent might cover; it is itself a pattern, and the question
is whether every concrete permission the child could ever match is one the
parent matches too. Comparing segment by segment with `*` wild on the
parent side and literal on the child side gives exactly that.

So `tasks:read` cannot mint `tasks:*`. The child could match
`tasks:delete`; the parent cannot. That is not a special case — it falls
straight out of treating the child as the pattern it is — but it is the
trap a naive check walks into, because `tasks:read` does "cover"
`tasks:*` if you squint at it as a string.

It is also **deliberately conservative about the wildcard**. A parent
holding all four of `tasks:read`, `tasks:create`, `tasks:update` and
`tasks:delete` may not mint `tasks:*`, even though the two grant the same
thing today. They stop granting the same thing the moment a fifth action
exists, and an enclosure rule that consulted the current action list would
silently widen every such token on upgrade. Four grants is what that parent
holds, so four grants is what it can hand on.

### Choosing the chain

Two details of how the minted chain is decided:

- Omit `max_ttl_secs` and the minted chain silently takes whatever is left
  of the parent's — an unasked-for chain never costs a refusal.
- Name `max_ttl_secs` explicitly and it is taken at face value, so asking
  for more than the parent has is a 403 rather than a quiet clamp. If you
  said a number, you get that number or an error.

There is no HTTP spelling for a token that never expires. `ttl_secs` is
required and must be positive, because a stateless core cannot take back
something it never recorded.

### Approving an installation

`POST /v1/pairings/{id}/approve` runs the same enclosure check against the
approver's token. An operator holding only `tasks:*` cannot grant
`lists:read` however loudly the client asks, and cannot answer a request
for `*` at all — the approval is refused, not trimmed. Pairing is where
grants are decided. Approval also cannot exceed the original app request; its durable authority is independent of the administrator token lifetime.

## Revocation

Use `erisdb clients revoke CLIENT_UUID` for registered installations. It blocks
access, renewal, delegated tokens and active change subscriptions across replicas.
Signing-key rotation alone does not revoke a registration: its proof can renew
against the new signing key. See [clients.md](clients.md#administration).

Manual tokens without a `client` claim have no individual registry
entry. Their expiry or signing-key rotation ends them. Keep the Iroh seed separate
if rotating the signing key must preserve the core's address.

## Rate limits

`POST /v1/capabilities` and `POST /v1/capabilities/refresh` share a token
bucket: a burst of 10, refilling at one every five seconds, per caller.
Minting is rare for an honest client and attractive to grind on, so the
shape is a handful of bursts and then a trickle.

The bucket is soft state, held in the process. A restart forgives
everyone, and replicas do not share buckets — the limit exists to blunt a
flood, not to keep books.

It is checked **before** authorization, so a caller with no
`meta:capabilities:mint` that hammers the mint route sees 429 rather than
403. Eleven requests on one connection is enough to watch it happen.

The key is the observed Iroh identity or TCP IP (not source port). HTTP clients
behind the same local reverse proxy share that IP bucket. Installation renewal
uses the same bucket. Limits are process-local; protect the public TLS edge
against distributed floods when deploying publicly.

## Three ways to get a token

### `erisdb mint` — the root

Holds the secret, so it can cut anything. This is where authority enters
the system.

```sh
erisdb mint --grant tasks:read --grant tasks:create --ttl 86400 --user alice
erisdb mint --grant 'tasks:read,tasks:create' --ttl 86400 --user alice   # same thing
```

| flag | default | notes |
|------|---------|-------|
| `--grant` | none, required | One permission. Repeatable, and comma-separated values are split. A wildcard is a master key, so it has to be typed. |
| `--ttl` | required unless `--no-expiry` | Seconds. Must be positive. |
| `--max-ttl` | 30 days | Chain length. Conflicts with `--no-expiry`. |
| `--no-expiry` | off | A token that never expires. Conflicts with `--ttl` and `--max-ttl`. |
| `--user` | none | Signed identity for attribution. |
| `--secret` | `$ERISDB_SECRET` | The HMAC key. |

`--ttl 0` is an error rather than a spelling of "forever": it produces a
token whose `exp` is now, which is refused on first use. The CLI says so
and points at `--no-expiry`.

`--no-expiry` is for daemons that must not fail at 3am — the poker is the
canonical one, and it is scoped to `--grant meta:system:tick`, which is
authority to do exactly one thing. Know that the only way to revoke one is
rotating the secret.

Unlike the HTTP route, the CLI applies no ceiling to `--max-ttl`: it holds
the secret, so it is already the trusted path.

### `erisdb pair` — ask a human

`erisdb pair` takes no scope flags at all. It cuts a pairing code against a
running core, shows it as a QR, and waits: the client says what it wants
and the operator answers. Approval is bounded by the requested grants and the operator’s own authority.
The terminal offers approve, select a subset, or deny, after fingerprint comparison.

```sh
erisdb pair --name my-laptop
```

| flag | default | notes |
|------|---------|-------|
| `--url` | `$ERISDB_URL`, else `http://127.0.0.1:7700` | The core to drive pairing against. Pairing is stateful, so a core must be running. |
| `--name` | none | A label for the human, carried in the ticket. Trusted for nothing. |
| `--ttl` | 600 | How long the code stays open, in seconds. |
| `--token-ttl` | 604800 | Lifetime of the token an approval issues. |
| `--client-url` | `--url` when it is not loopback | The url to put in the ticket, for clients that cannot speak QUIC. |
| `--no-iroh` | off | Leave the endpoint id out of the ticket. With a loopback `--url` and no `--client-url`, that leaves no dialable address and the CLI refuses. |

See [pairing.md](pairing.md) for the ticket format and
[permissions.md](permissions.md#pairing) for the flow.

### `POST /v1/capabilities` — delegation

Needs a token holding `meta:capabilities:mint`, and can only ever narrow.
This is how an agent gets a short-lived, single-facet token from a
longer-lived one without anybody touching the secret. Full parameters in
[api.md](api.md#post-v1capabilities).

## Attribution is not authority

`user` is a string signed into the token and stamped into `source.user` on
every write it makes. It grants nothing, and enclosure ignores it
entirely: a token can mint a child naming any user at all, including one
that is not its own.

That is on purpose. `user` is how agents get names in the audit log, not
how anything gets permission. If you are reading `source.user` to decide
whether a write was allowed, you are reading the wrong field — the write
already happened, and the token is what allowed it. What `source` gives
you is a trust gradient for reading history: `addr` observed and
unforgeable, `user` signed by whoever held the secret, `client` claimed by
the caller and worth exactly nothing on its own.
