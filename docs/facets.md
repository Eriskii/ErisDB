# Facets

A facet is a named schema over the store — `tasks`, `lists`,
`exercise`. It is the unit of three things at once: the schema a body must
satisfy, the namespace a permission lives in, and the filter a change-feed
cursor runs under. One name does all three jobs, which is why they never
drift apart.

Facets are not code. There is no table per facet, no migration, no deploy.
A facet is an item in a facet, and registering one is a `POST /v1/items`.

## A facet name is a permission namespace

For a facet named `tasks`, the permissions are `tasks:read`,
`tasks:create`, `tasks:update` and `tasks:delete`. Nothing declares them
and nothing registers them: the core computes the permission a request
requires from the facet the request names, and asks whether the token
covers it. A grant naming a facet nobody has registered is legal and inert
until somebody does.

That has consequences for what a name may be:

- One segment of `[a-z0-9][a-z0-9._-]*`. No uppercase, no spaces, no `:`
  and no `*` — a name containing either would be a way to write a
  permission by registering something.
- Not `meta`, `facet`, `system` or `pair`. The first is the core's own
  namespace; the other three are its own facets.
- Unique across the store, enforced by an index rather than a check.

A registration that breaks the first two is **400 `bad_request`** naming
the rule; a duplicate is **409 `conflict`**. And a name carries no version
suffix: `tasks`, not `tasks/v1`. The version lives in the body, so a grant
survives the schema moving. See [Versioning](#version-field).

## The `facet` meta-facet

`facet` is the facet whose items are facet definitions. It is bootstrapped
by the first migration and it validates registrations against its own
schema — but it is not an ordinary facet, because it answers to `meta:`
and nothing else:

| operation | permission |
|-----------|------------|
| list or read registrations | `meta:facets:read` |
| register, change or remove one | `meta:facets:write` |

Two permissions rather than four: reading a schema and changing one are
different things, and there is no useful grant between them. `*:read` does
**not** reach `meta:facets:read` — read-everything is about your data, not
about the deployment.

A definition is a body with six fields:

| field | required | default | meaning |
|-------|----------|---------|---------|
| `name` | yes | — | The facet name, and its permission namespace. Rules above. |
| `schema` | yes | — | A JSON Schema object. `{}` accepts anything. |
| `version` | no | — | An integer ≥ 1. Documentation: the core stores it and never reads it. |
| `strict` | no | `true` | When false, `schema` is stored but never run. |
| `lapse` | no | — | `{"due": field, "done": field}`. Makes the tick sweep watch this facet. |
| `permissions` | no | — | Human sentences for the pairing prompt, keyed by action. Presentation only. |

`additionalProperties` is false: a definition carrying anything else is a
422. So is one missing `name` or `schema`. The schema runs before the name
is checked, so a registration with no name at all is a schema violation
rather than a naming complaint.

Registering:

```sh
curl -fsS http://127.0.0.1:7700/v1/items \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{
    "facet": "facet",
    "body": {
      "name": "exercise",
      "version": 1,
      "strict": true,
      "schema": {
        "type": "object",
        "required": ["kind"],
        "properties": {
          "kind": {"type": "string", "minLength": 1},
          "km":   {"type": "number", "minimum": 0}
        },
        "additionalProperties": false
      },
      "permissions": {
        "read": "read your workouts",
        "create": "log a workout"
      }
    }
  }'
```

**201** and the facet exists. Every subsequent `POST /v1/items` naming
`exercise` is validated against that schema, and `exercise:create` starts
meaning something to every token that already held it.

**That is the whole path to a new client.** One token holding
`meta:facets:write` registers the facet; the client pairs and asks for
`exercise:read` and `exercise:create`; a human approves. No deploy, no
allowlist, no core release, nothing restarted. The e2e suite pins exactly
that sequence, including that a grant issued *before* the facet existed
starts working the moment it does.

Names are unique, so a second registration of the same name is **409
`conflict`** with `that value already exists`. The apps use exactly that:
they always `POST` first, and treat 409 as "someone already registered it"
and 403 as "this token cannot register facets" — both fine, neither fatal.

Editing a schema is `PUT /v1/items/{id}` on the definition, with its
revision. Widening is free. Tightening is not: existing items are not
re-validated, so a body that no longer conforms sits in the store
untouched until something tries to write it back and gets a 422. Reverting
an item to a snapshot older than the tightening fails the same way, which
is deliberate — the revert route re-validates against the schema as it is
now, not as it was.

Deleting a definition is `DELETE` on the item. The facet's items stay
where they are and remain readable; writes to that facet become **422
`unknown_facet`**, because the write path looks the registration up every
time.

Writing to a facet nobody has registered is the same 422. There is no
implicit creation — a typo in a facet name is an error, not a new facet.

## The other two meta-facets

`system` and `pair` are the core's own, and each has exactly one name:

| facet | holds | permission |
|-------|-------|------------|
| `system` | nothing; the tick's change rows are filed under it | `meta:system:read` for every action; `meta:system:tick` fires the tick |
| `pair` | pairing sessions | `meta:pairing:read` to look, `meta:pairing:create` to cut, `meta:pairing:approve` to change |

Neither has a registration in the store — `system` has none at all, so a
write naming it is a 422 whatever the token holds. `pair` has one, and its
schema is what the pairing routes write through.

Neither is writable through the item routes at all. `POST`, `PUT` and
`DELETE` naming `system` or `pair` are refused with a 400 pointing at the
endpoints that own them, whatever the token holds — a hand-written session
could otherwise claim an approval nobody gave, and the state machine is the
only thing that should be able to produce one. Reading them as items is
ordinary and still works.

## Validation and `strict`

Bodies are validated with JSON Schema — draft 2020-12 unless the schema's
own `$schema` names another. A failure is **422 `schema_violation`** and
`detail` carries the validator's own message.

`format` is an annotation, not an assertion. The validator is built with
no default features, so it resolves nothing over the network or off disk,
and `"format": "date-time"` documents a field without checking it. A due
date of `"not a timestamp"` is stored happily and then simply never lapses,
because `safe_ts` returns NULL on it.

`strict: false` stores the schema and skips the check. It is the escape
hatch for a facet whose shape is still moving — application settings, a
scratch namespace — and the honest description of it is "documentation
that does not run". Permissions and the change feed work exactly the same
either way; only validation is off.

Compiled validators are cached per replica, keyed by facet name and
matched on the schema itself, so editing a registration recompiles on the
next write and no restart is needed. Each replica caches independently.

The meta-facet only requires that `schema` be an object, so a schema that
is well-formed JSON but not a valid JSON Schema registers successfully and
fails on the **first write to that facet**, as `400 bad_request` with
`facet X carries an invalid schema`. Registration is not a compile step.
Write one item to a new facet before you believe in it.

## Why `$ref` may not leave the document

A facet schema is caller-supplied data that the core later *runs*, on
every write to that facet. A `$ref` in it is a request for the core to go
and resolve something.

So registration walks the whole schema and refuses any `$ref`,
`$recursiveRef` or `$dynamicRef` whose target does not begin with `#`:

```json
{"$ref": "http://169.254.169.254/latest/meta-data/"}
{"$ref": "file:///etc/passwd"}
{"type": "object", "properties": {"x": {"$ref": "https://example.com/s.json"}}}
```

All three are **400 `bad_request`**. The first is a cloud metadata
endpoint and the second is a local file; the third is a slower version of
both, plus an outbound request on every single write. The check is
recursive, so burying one inside `$defs` does not help, and it happens at
registration — where there is a human to tell — rather than at write time,
where there would only be a log line.

Local refs are the normal way to factor a schema and work fine:

```json
{
  "type": "object",
  "properties": { "who": { "$ref": "#/$defs/name" } },
  "$defs": { "name": { "type": "string", "minLength": 1 } }
}
```

A `$ref` whose value is not a string is also a 400: the guard will not
guess.

## Permission descriptions

A registration may carry human sentences for the approval prompt:

```json
"permissions": {
  "read": "read your tasks",
  "create": "add tasks",
  "update": "change and complete tasks",
  "delete": "delete tasks"
}
```

Keys are actions, values are strings; the meta-facet schema requires
nothing more, so a facet defining `sync` may describe `sync` too. They are
presentation only: no grant depends on them, a missing one shows the raw
permission, and nothing in the core reads them. They exist so that the
person deciding whether to approve `tasks:delete` reads "delete tasks"
instead.

## Lapse rules and the tick sweep

The core has no idea what a task is or what "due" means. What it can do is
compare a field to the clock, if a facet tells it which field.

```json
"lapse": { "due": "due", "done": "done" }
```

`due` names a body field holding a timestamp. `done` names an optional
body field holding a boolean. Both are field *names*, not values — this is
the entire extent of the core's knowledge of a facet's semantics.

On `POST /v1/tick`, after appending the tick itself, the core finds every
facet definition carrying a `lapse` key and, for each, appends a `lapsed`
change row for every item where:

- `safe_ts(body ->> due) <= now()` — the due field parses as a timestamp
  and that timestamp has passed. `safe_ts` returns NULL rather than
  erroring on garbage, so one malformed item can never wedge the sweep;
  an unparseable or missing due date simply never lapses.
- the done field is not the text `true`. A missing `done` field, a missing
  `done` *name* in the rule, `false`, or null all count as not done.
- no `lapsed` row already exists for this item with `at >= updated_at`.

That last clause is the whole re-arm rule: **an item lapses at most once
per edit.** Complete an overdue task and the sweep goes quiet. Edit it
back to undone and `updated_at` moves past the old lapse row, so the next
tick fires again. Nothing is remembered outside the feed itself.

Overlapping pokes are the normal case — a timer fires while the last one
is still running — and they are safe by construction rather than by luck.
The tick's own change row takes the feed's append lock first and holds it
for the whole transaction, so a second poke waits, then sees the first
one's rows and finds nothing to do. `lapsed` in the tick's response counts
what that call actually appended.

A `lapsed` row carries the item's facet, its current body and its current
revision. So a client tailing `?facet=tasks` sees lapses on its own feed,
with everything it needs to render a notification, and never sees `tick` —
which is filed under `system`.

Lapse is a notification, not a state change. The item's body and
`updated_at` are untouched; nothing in `items` records that it happened.
If you want a lapse to change something, a client has to write that
change.

## The shipped schemas

Two facets ship with clients in this tree. Neither is special to the core
— they are ordinary registrations that several apps happen to agree on,
pinned by the e2e suite so a change to either breaks a test rather than a
phone.

### `tasks`

One-off and recurring tasks with optional due dates.

```json
{
  "name": "tasks",
  "version": 1,
  "strict": true,
  "permissions": {
    "read": "read your tasks",
    "create": "add tasks",
    "update": "change and complete tasks",
    "delete": "delete tasks"
  },
  "schema": {
    "type": "object",
    "required": ["title", "done"],
    "properties": {
      "title": { "type": "string", "minLength": 1 },
      "done":  { "type": "boolean" },
      "due":   { "type": "string", "format": "date-time" },
      "notes": { "type": "string" },
      "repeat": {
        "type": "object",
        "required": ["n", "unit"],
        "properties": {
          "n":    { "type": "integer", "minimum": 1 },
          "unit": { "enum": ["day", "week", "month", "year"] }
        },
        "additionalProperties": false
      }
    },
    "additionalProperties": false
  },
  "lapse": { "due": "due", "done": "done" }
}
```

`title` and `done` are the whole of a task. `due` is what `lapse` watches;
without the `lapse` rule there are no due notifications at all, which is
the one part of this registration that is not merely documentation.

`repeat` is a client-side convention the core knows nothing about:
completing a repeating task advances `due` by `n` units instead of setting
`done`. The core sees an ordinary update with a later due date, and — since
`updated_at` moved — re-arms the lapse for the next occurrence. The
recurrence rule needs no server support because the lapse rule already
does the only server-side work involved.

`additionalProperties: false` means a client removing an optional field
must remove the key, not set it to null. The web apps do this: a patch
value of `null` deletes the key before the body is sent.

### `lists`

Lists of things — books, films, parts, anything.

```json
{
  "name": "lists",
  "version": 1,
  "strict": true,
  "schema": {
    "type": "object",
    "required": ["list", "name"],
    "properties": {
      "list":        { "type": "string", "minLength": 1 },
      "name":        { "type": "string", "minLength": 1 },
      "description": { "type": "string" },
      "link":        { "type": "string" },
      "attributes": {
        "type": "object",
        "additionalProperties": {
          "anyOf": [
            { "type": ["string", "number", "boolean", "null"] },
            { "type": "array" }
          ]
        }
      }
    },
    "additionalProperties": false
  }
}
```

**Lists are implicit.** There is no list object anywhere: a list is the set
of entries naming it in `list`. Creating a list means creating an entry;
deleting the last entry deletes the list. Nothing has to keep the two in
step because there is only one of them.

`attributes` is a flat frontmatter-style map. Values may be scalars or
arrays; a nested object is refused. That bound is what keeps it a map of
facts about the entry rather than a second schema smuggled in past the
first.

There is no `lapse` rule and no timestamp in the body. Added and modified
times ride the item envelope as `created_at` and `updated_at`, where the
core maintains them — a body field would be a second copy that a client
could get wrong.

## Version field

`version` is optional metadata in a facet definition. The core validates it as
a positive integer but does not use it to select schemas or convert items.
Updating a facet changes validation for subsequent writes. It does not rewrite
or revalidate existing items.
