---
name: erisdb-client-development
description: Build or extend applications that use ErisDB as their backend. Use for new browser, Android, native, CLI, daemon, or MCP clients and their sign-in through the mesh, facet schemas, synchronization, offline writes, or plugin calls. Does not cover implementing server plugins or administering a deployment.
---

# Develop an ErisDB client

Build the user's application against the existing ErisDB API. Keep its chosen
platform, language, and product scope. A new client normally needs no core route,
database migration, or server plugin. Establish the app's data shape, facet
names, needed actions, and transport from the request and surrounding project.
Ask only for missing choices that materially affect implementation.

## Read the relevant contract

Paths below are relative to the ErisDB repository root. Prefer the checkout
matching the target server; the [upstream repository](https://github.com/Eriskii/ErisDB)
provides these files when working in a separate client project. Relative links
resolve from this skill's location inside an ErisDB checkout.

Start with the relevant sections of [Building a client](../../../docs/client-development.md)
and the [API contract](../../../docs/api.md). Read additional references as needed:

| Work | Reference or implementation |
|---|---|
| Getting in and credentials | [The mesh](../../../docs/mesh.md), [installations](../../../docs/clients.md), [permission matching](../../../docs/permissions.md) |
| Browser client | [Browser client (WebAssembly)](../../../clients/web/README.md), [Tasks](../../../apps/tasks/index.html), [Lists](../../../apps/lists/index.html) |
| Rust or native client | [Rust SDK](../../../erisdb-client/README.md), [SDK implementation](../../../erisdb-client/src/lib.rs) |
| Android client | [Shared Kotlin and JNI](../../../apps/android-shared/README.md), [Tasks app](../../../apps/tasks-android/README.md), [Lists app](../../../apps/lists-android/README.md) |
| Schemas and due events | [Facets](../../../docs/facets.md) |
| Calling a plugin | [Plugin contract](../../../docs/plugins.md) |
| MCP client | [Existing MCP client](../../../clients/mcp/README.md) |

Reuse compatible helpers and SDKs. The demos are examples, not a complete sync
or delivery guarantee: their snapshot initialization has timestamp-pagination
limits, and their outboxes can duplicate a create after an ambiguous failure.
Use the protocol rules below when adapting them. If a reference disagrees with
the target server, inspect its handler and existing integration tests.

## Get in through a device, and keep the key

- An app is let in by a device of its owner's mesh, never by the server. It asks
  the device's ErisAuth to approve its Iroh key for service `erisdb`, not serving,
  with the grants it wants: over the agent socket on a desktop
  (`$XDG_RUNTIME_DIR/erisauth/agent.sock`), through ErisAuth's bound service on
  Android, or by redeeming a device's `eris://pair/` ticket where no ErisAuth runs
  beside it. The person approves on the device, possibly fewer grants, and types
  ErisDB's passphrase; ErisAuth enrolls the app and then answers with its note and
  a snapshot of the mesh.
- ErisDB is the app the snapshot shows approved to serve `erisdb` (the most
  recently approved, should there be more than one). Dial its endpoint id with ALPN
  `erisdb/0` — HTTP/1.1 over QUIC streams — as the app's key, with no
  `Authorization` header. Prefer the Rust SDK to recreating the transport:
  `sign_in` does the whole of the above, `Client::dial` reconnects later, and
  Android apps use its JNI surface (`nativeLocate`, `nativeDial`).
- The key is the credential. Generate it once, keep the same 32 bytes across
  restarts in the platform's credential storage, and store ErisDB's address
  beside it. Never ship the server signing secret or an operator-wide token in an
  ordinary app. `X-ErisDB-Client` is an optional display label, not
  authentication.
- A denial is an answer. A 401 means the app is out — revoked, its device removed
  from the mesh, or its note revoked — and only a device can let it back in.

## Initialize a permitted facet

Request only actions the app uses, such as `bookmarks:read`, `bookmarks:create`,
`bookmarks:update`, and `bookmarks:delete`. Namespaces are shared data contracts,
not private tables owned by an app package. Reuse an existing compatible facet
when interoperability is intended. Read effective grants from `/v1/permissions`
and make controls reflect what was granted, including read-only access.

`bookmarks:create` permits registering a missing `bookmarks` definition (sent over
Iroh as the app's key, with no `Authorization` header):

```http
POST /v1/items
Content-Type: application/json

{"facet":"facet","body":{"name":"bookmarks","version":1,"strict":true,"schema":{"type":"object","required":["title","url"],"properties":{"title":{"type":"string"},"url":{"type":"string"}},"additionalProperties":false}}}
```

Adapt this example to the requested domain. A facet name is one lowercase
permission segment. Schemas compile at registration; references must stay local
to the document. JSON Schema `format` is not enforced by the current validator,
so validate application-specific values such as URLs and dates in the client.

Ordinary initialization does not need `meta:facets:write`. It cannot replace an
existing definition. On 409, retain that definition and handle incompatibility;
do not assume the existing schema matches yours. Reading definitions separately
requires `meta:facets:read`. Read-only clients skip initialization. Keep queued
creations if setup fails, and retry setup after reconnecting before sending them.
The definition's `version` is metadata, not an automatic data migration.

## Keep synchronization complete and durable

For a complete application-facet cache, replay `/v1/changes` from `since=0`,
filtering by the URL-encoded facet. Apply each page in sequence order, persist
the cache and returned `next` cursor atomically, and continue until an empty page.
Resume from that saved cursor. Give each core/facet its own cache and cursor;
restart initialization when the saved snapshot is missing or unreadable.

Do not treat one `/v1/items` response as the whole dataset: its limit is 1000.
Paging with `updated_since` can skip records sharing the boundary timestamp
because the filter is strictly greater-than and has no ID continuation.
Built-in facet definitions are not all represented by creation events; use the
item API when reading those definitions rather than reconstructing them by feed.

Apply `created` and `updated` snapshots by `item_id`, and remove `deleted` items
idempotently. A `lapsed` event is not an edit; its event timestamp must not
overwrite an item's modification timestamp. Change rows are not full Item
envelopes; fetch the item if exact current envelope metadata is needed. Ignore
rows without item IDs for the item cache. Keep unsent edits separate from the
server snapshot and suppress historical notifications during initial replay.

Polling is sufficient. For SSE, use `/v1/changes/stream?since=...&facet=...`, parse
`event: change`, and reconnect from the last persisted cursor. There are no SSE
event IDs or `Last-Event-ID` support. Serialize polling and streaming cache
updates. Handle revocation, disconnects, and 503 capacity limits; polling can
continue while waiting to reconnect. The core closes the stream of an app the
mesh or the operator ends.

## Preserve user intent through writes

`PUT /v1/items/{id}` replaces the whole body and requires its current revision.
Preserve untouched fields. On 409, refetch and reconcile the intended edit with
the latest body; changing only the revision can overwrite another client's work.
Delete supports an optional revision when concurrent edits should prevent it.
There is no HTTP PATCH, upsert, bulk-write, or server-side search endpoint.

If offline writes are required, durably save the outbox before transmission and
overlay it on the server cache. Track pending IDs and dependent operations. A
lost create response has an uncertain outcome: the API has no idempotency key or
client-chosen item ID. Do not blindly resend or promise exactly-once delivery;
retain the operation for reconciliation. Check persistence failures and surface
schema/permission rejection rather than retrying permanently invalid writes.

An app calling by its key has nothing to renew. Daemons holding minted tokens
use the bounded `/v1/capabilities/refresh` flow; see the client guide.

For plugin features, discover permitted operations with `/v1/plugins` and invoke
`POST /v1/call` under their operation grants. Check status and content type before
decoding or streaming the response. Plugin calls do not persist app data. Provider
credentials belong in server configuration; the bundled OpenAI operation is
`chat.completions` with `openai:chat`, and clients execute returned tool calls.

## Verify the client at its real boundaries

Exercise the client against a real core and PostgreSQL with isolated test data.
Use the browser, executable, or device runtime the app will actually use; retain
useful pure tests without substituting mocked server responses for integration
coverage. The Rust integration suites (`erisdb/tests/mesh.rs`,
`erisdb-client/tests/client.rs`, `clients/mcp/tests/e2e.rs`) run a real core with
device nodes from the mesh library and a stand-in for ErisAuth's agent socket;
copy that harness. Use a disposable emulator for tests that clear app data, not
the user's phone.

Cover the features implemented: signing in and restart recovery, granted subsets,
CRUD and stale revisions, offline persistence, permission narrowing, revocation,
and removal of the app's device from the mesh. For synchronization, include more than one
page, timestamp ties, deletions across pages, missing/corrupt caches, and concurrent
edits. If writes retry, lose a response after a real commit to verify uncertainty
handling. Exercise external provider calls only within the user's authorization.

Deliver the client with its setup/run instructions, required grants, schema,
verification results, and any untested or unsupported behavior. Keep changes to
the existing core and deployment outside the client task unless requested.
