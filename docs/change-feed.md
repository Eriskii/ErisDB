# The change feed

One table does two jobs. `changes` is the event bus every client syncs
against, and it is the audit log of everything that has ever happened.
Those are the same rows. Nothing is derived from anything else, nothing
can drift, and there is no second system to keep in step.

Every mutation appends a change row **in the mutation's own transaction**.
A write that commits has a change row; a write that does not, does not.
There is no window in which the store is true and the feed is behind.

## `seq` is the cursor

`seq` is a `BIGSERIAL`. A client's entire sync state is one integer: the
highest `seq` it has processed. Ask for everything after it, apply what
comes back, keep the last `seq`, repeat.

For that to work, `seq` order has to be commit order — and by default it
is not. A sequence hands out numbers at INSERT, but rows only become
visible at COMMIT, so two writers can take 5 and 6 and commit in the other
order. A reader polling in that window sees 6, moves its cursor past it,
and never sees 5. The row is in the table forever and that client will
never read it.

So every writer takes one transaction-scoped advisory lock, from the
append through to the commit. Serialising the append makes seq order and
commit order the same order, which is exactly what lets a cursor be a
single number. Writers take it after whatever item lock they need, so the
order is always item-then-feed and there is no cycle to deadlock on.

The e2e suite pins the property — forty concurrent writers, one cursor
walking the feed while they commit, and every write observed.

`seq` is unique and increasing. It is not dense: an aborted transaction
burns its number. Never treat a gap as loss.

## Reading it

Two routes, one shape. [api.md](api.md#get-v1changes) has the exact
parameters; the semantics are:

- `since` is **exclusive**. `since=0` is the beginning of time.
- `next` in a `GET /v1/changes` response is the last `seq` on that page,
  or `since` unchanged when the page is empty. Feed it back as `since`.
  It is a page cursor, not a high-water mark of the feed — an empty page
  means caught up *for this filter*.
- `facet` narrows to one contract. Reading one facet's feed needs
  `{facet}:read`; reading the feed unfiltered needs `meta:feed:read`,
  which is its own permission rather than the sum of the ones it would
  reveal. Without that rule any token could read every write in the system
  by dropping a query parameter — and `*:read`, which is every facet, still
  does not cover it.
- The SSE stream drains from `since` first and *then* goes live, so
  nothing falls between catching up and subscribing.

The stream is an optimisation. The poll is the source of correctness. The
web apps run both: a `setInterval` catch-up every ten seconds while the
stream is down and every minute while it holds, plus the stream for
latency. A dropped stream costs nothing because the cursor is client-held
and the next connection opens at `?since=<cursor>`.

## What each `op` means

| `op` | `item_id` | `body` | `revision` | what happened |
|------|-----------|--------|------------|---------------|
| `created` | the item | the new body | 1 | `POST /v1/items`. |
| `updated` | the item | the new body | the new revision | `PUT /v1/items/{id}`, or a `revert`, which is an ordinary update. |
| `deleted` | the item | null | null | `DELETE /v1/items/{id}`. The state after a delete is absence; the prior snapshot lives one row up. |
| `tick` | null | null | null | `POST /v1/tick`. Facet is `system`, so a facet-filtered reader never sees one. |
| `lapsed` | the item | its current body | its current revision | The tick sweep found an overdue, un-done item. |

A `created`, `updated` or `lapsed` row carries the **full body** the change
produced, not a patch and not just an id. That is what lets a client apply
the feed straight to its cache with no per-item refetch: the feed alone is
enough to keep a mirror true. The revision comes with it, so the mirror
also knows what to send on the next write.

`lapsed` is a notification, not a state change — nothing in `items` moved.
It is the one op a client can ignore entirely and still hold a correct
cache. See [facets.md](facets.md#lapse-rules-and-the-tick-sweep) for the
rule that fires it and the once-per-edit re-arm.

A revert produces `updated`, not some third thing. History rolls forward.

## History and revert

`GET /v1/items/{id}/history` is the feed for one item: every state it has
been in, oldest first, each with the source that produced it. It reads
`changes` directly, which has two consequences worth knowing.

It works on deleted items. History outlives its item — the rows were never
touched by the delete — so a `history` call on an id that no longer exists
returns everything up to and including the `deleted` row. The
authorization check reads the facet from the item's *first* change row, so
it works even after the facet's definition is gone.

And it returns **404 for an item with no change rows**, which is not quite
the same as "no such item". The rows this actually catches are the two
bootstrap registrations, `facet` and `pair`: both were inserted by
migrations, never written through the API, and so have no history at all
despite being perfectly real items.

`POST /v1/items/{id}/revert` takes a `seq` from that history and writes
that body back as a **new** revision. It is git-revert, not time travel:

- The old state lands as an `updated` row with its own source and its own
  place at the head of the feed. Nothing is rewritten and no `seq` is
  reused.
- It takes a `revision`, so a revert that races an edit is a 409 like any
  other write.
- The snapshot is re-validated against the facet's schema **as it is now**,
  so a body from before a tightening is a 422 rather than a way around it.
- Reverting a deleted item is a 404. Undelete is not a revert: read the
  history, take the last non-null body, and create a new item. It gets a
  new id, and the old id's history stays where it is.

## The feed grows without bound

Nothing prunes it. Say that plainly, because it is the one operational
property of this design that surprises people:

- Every write snapshots the **entire body** it produced. Ten edits to a
  large item store that item ten times.
- A delete keeps every body that came before it. Deleting an item removes
  it from `items` and removes nothing from `changes`.
- The poker adds a `tick` row a minute. That is about half a million rows
  a year on its own, before any real writes.

The feed *is* the history, so there is no version of this that both keeps
the audit log and trims the table. A dump therefore contains every version
of everything ever written, including things since deleted, including
anything that was ever briefly in a body by mistake. Size for it, and
handle it accordingly — see
[the root README's backup notes](../README.md#backup-and-restore).

The other half of the same fact: **restoring `items` without `changes` is
not a restore.** Every sync client holds a cursor into the feed, and a feed
that has lost its tail leaves those cursors pointing at a history that no
longer exists. Back up and restore the whole database, including the `clients`
registry, together; see [migration and recovery](clients.md#migration-and-recovery).

If a body should never be in the store, it must never be written. There is
no route that removes a row from `changes` — deleting the item does not,
and reverting past it does not.

## Everything lands on it, pairing included

A pairing session is an ordinary item in the `pair` facet, which is what
lets a dashboard watch a request arrive live: `created` when a code is cut,
`updated` when a client redeems it, `updated` again when a human answers.
That is the point of holding the conversation in the store rather than in
the process — it survives a restart and it replicates.

It also means the feed keeps whatever those bodies held, permanently — so
a session never holds a token. An approval records the *shape* of the token
it will issue: the grants, the two deadlines, the signed user. The token
itself is minted at the moment the client collects it, and the same
revision-checked write spends the session. Nothing readable through
`?facet=pair` or the global feed is a credential, and nothing has to be
scrubbed later, because a change row cannot be edited and a token written
here would outlive every clock it carries.
