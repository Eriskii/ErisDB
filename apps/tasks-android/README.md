# tasks-android

The tasks client as an Android app, speaking erisdb's native transport:
Iroh QUIC, HTTP/1.1 per bi-stream, no IP or port configured anywhere —
just the server's endpoint id and a capability token.

## Shape

- **`../../erisdb-client/`** — the Rust core: dials by endpoint id (or a
  full `EndpointAddr` JSON), holds one connection, one bi-stream per
  request. Compiled as `liberisdb_client.so` via `cargo ndk` into
  `app/src/main/jniLibs/`. Its schema is pinned by host-side tests
  against a real core over real QUIC.
- **Kotlin shell** — `ErisDB.kt` is the JNI surface (configure + request,
  JSON in/out) and `Core.kt` wraps it in the `CoreApi` interface the rest
  of the app is written against. `Sync.kt` is the read path, `Outbox.kt`
  the write path, `Store.kt` the persistence, `Secrets.kt` the sealed
  token and identity, `Notifications.kt` the due notices, `Capability.kt`
  the token's own arithmetic, `Pairing.kt` the ticket a core cuts,
  `Redeem.kt` the two-phase pairing conversation, `Grants.kt` the
  permission grammar and what this app asks for; `TasksApp.kt` wires them
  to the screens.
  `Recurrence.kt` is the repeat rule as pure `java.time` and `Task.kt` is
  the thin JSON skin over it.

The application ID is `dev.erisdb.tasks`.
The source namespace is `dev.erisdb.tasks`. The phone mints a random 32-byte
iroh identity on first launch and keeps it, so `source.addr` names this
device stably across sessions. The client string is
`Tasks (Android) v0.4` — what the operator reads on the approval prompt.

## The facet

`tasks`, shared with the web client at `../tasks/`. The name is also the
permission namespace, so the schema version lives in the registration
body: `tasks:read` survives a move to schema v2, which is what a
permission should do. A task body is:

```json
{"title": "water the plants", "done": false,
 "due": "2026-08-22T07:00:00Z", "notes": "the big one too",
 "repeat": {"n": 2, "unit": "day"}}
```

The schema is strict — `additionalProperties: false` — so `due`, `notes`
and `repeat` are absent rather than empty when unset. The facet body
carries `lapse: {"due": "due", "done": "done"}`, which is how the core
knows a task has gone overdue and emits a `lapsed` change. It also
carries `version: 1` and a line of prose per permission, so the pairing
prompt reads *add tasks* rather than `tasks:create`.

The app initializes its missing schema automatically with `tasks:create`
before sending queued creations. It cannot replace an existing schema or
administer other namespaces. If initialization fails, pending entries remain
on the device and the app retries. No manual registration is needed.

## Recurrence

`repeat` means: completing the task does not close it. Its due date
advances by `n × unit` — anchored on the original due date, so month
steps keep their day across short months (Jan 31 → Feb 28 → Mar 31) —
repeatedly until it lands strictly in the future, and `done` stays
false. The step always happens at least once, so completing early moves
the task on to the next period rather than leaving it due today; and a
task months past due catches up in a single tap rather than replaying
every beat it missed.

A task with `repeat` but no `due` has nothing to roll forward from, so
it simply becomes done, like any one-off. Un-checking anything sets
`done` back to false.

The rule lives in `Recurrence.kt` as pure `java.time` — no `org.json`,
no Android — and is pinned by `app/src/test/java/dev/erisdb/tasks/`:
daily advance, catch-up over many missed periods, month-end clamping in
common and leap years, weekly `n > 1` stepping in whole periods, and
the always-advance-once guarantee.

## Sync

The app first reads the change feed's head, then fetches an item snapshot. It
pages the snapshot with `updated_since` and saves the earlier feed cursor so
changes made during the fetch can be replayed on the next poll.

This implementation has a pagination limitation: the server caps item pages at
1000 and filters timestamps with strict `>`. Records sharing a page-boundary
timestamp can be skipped, and subsequent changes after the saved cursor do not
recover an unchanged skipped record. It does not guarantee a complete initial
snapshot for every dataset. The [client guide](../../docs/client-development.md#2-seed-a-cache-then-hold-a-cursor)
describes complete initialization by replaying the existing change feed from zero.

After initialization, the app polls `GET /v1/changes?since=<cursor>&facet=tasks`
and applies rows to its cache. Created and updated rows carry bodies and
revisions; deleted rows remove items. The saved cursor resumes across restarts;
reinstalling or replacing the configuration reseeds the cache.

The cache is one file, written whole and renamed into place. It is not
SharedPreferences: that reads its entire XML into memory and rewrites all
of it on every commit, which is the wrong shape for a few hundred
kilobytes of items.

## Offline-first

Every mutation — add, edit, delete, and every checkbox tap — is committed
to an on-disk outbox with `commit()`, not `apply()`, **before** the
network is touched: killing the app mid-write loses nothing, and the op
replays on next launch. What the screen shows is the cache with the
queued ops replayed on top, so a change is visible the instant it is
made. The outbox drains in order and stops at the first transport
failure, so ordering holds; permanent rejections — a schema violation, a
403 — drop out with a reason on the status
line instead of retrying forever. Schema initialization failures keep creations
queued. A failed startup connection is retried automatically, and queued entries
remain visible after an offline restart.

A create names its item with a pending id the moment it is queued, so the
screen has something to hand back on the next tap. The core mints the
real id when the create lands, and from then on an alias carries every op
that still names the pending one — the delete queued behind the create,
and the edit made a second later from an editor still holding the pending
item. Without it, offline create-then-delete resurrects the item and
offline create-then-edit loses the edit.

Sync is serialized. The ten-second poll and every user mutation both ask
for one, and two running at once would read the same outbox, send the
same op, and leave the core holding two identical writes.

## Notifications

A due date that passes with the task still open posts a notice on the
`due` channel. The core notices it too — that is what the facet's `lapse`
rule is for — but the phone's own clock is the faster of the two, and
both land in the same place: a task is announced once per revision, so
editing it announces it again and polling ten times does not. Android 13
and up asks for `POST_NOTIFICATIONS` on first launch; a task that came
due before permission was granted is still announced once it is.

## Token refresh

Paired installations renew through `POST /v1/clients/{id}/refresh`, proving
possession of the persistent Iroh key. The app saves the replacement token and
can renew after access-token expiry. Current permissions and revocation are
checked by the server. The app reads permissions again during sync.

For a manually configured token, refresh uses `POST /v1/capabilities/refresh`
and is bounded by the token's expiry and refresh-chain deadline. A token with
no expiry is not refreshed.

## Secrets

The capability token and the iroh private key are the two things on the
phone worth stealing: one is write access to the store, the other is this
device's identity on the network. Both are sealed with an AES-256-GCM key
the Android keystore generates and never hands out; what sits on disk is
ciphertext and the key that opens it cannot leave the device.

`android:allowBackup="false"`. Nothing here belongs in Google's cloud or
on a device-to-device transfer: the secrets would arrive elsewhere as
noise, and everything else is a cache of the core that reseeds on first
sync.

The token is a password and every screen that shows it treats it as one:
masked until asked for, with the window marked `FLAG_SECURE` so it stays
out of screenshots and out of the recents-screen thumbnail.

## Build

```sh
cd ../../erisdb-client
cargo ndk -t arm64-v8a -o ../apps/tasks-android/app/src/main/jniLibs build --release
cd ../apps/tasks-android
./gradlew test                # the sync engine, the outbox, the recurrence rule
./gradlew assembleDebug       # apk lands in app/build/outputs/apk/debug/
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

`java.time` runs back to minSdk 24 through core library desugaring.

On NixOS the NDK's prebuilt toolchain needs its ELF interpreter and rpath
patched (patchelf against nix glibc/zlib/libstdc++), and gradle needs a
real JDK 17 registered by absolute path. Those paths are per-machine, so
they live in `~/.gradle/gradle.properties` rather than in this repo:

```properties
org.gradle.java.installations.paths=/nix/store/<hash>-openjdk-17.../lib/openjdk
android.aapt2FromMavenOverride=/path/to/patched/aapt2
```

## Pairing

Pairing is the front door, and it takes two steps and a person.

```sh
erisdb pair --name my-laptop
```

No scope flags: the client says what it wants and you answer. That prints
a QR code holding `erisdb://pair/<base64url-nopad(JSON)>`, which carries
where the core is and a **pairing code** — a token holding exactly
`meta:pairing:redeem`, naming one session, good for minutes. It is not a
capability over any data.

Point the phone's camera at it and tap the link it offers: Android routes
the `erisdb` scheme to this app. There is no scanner in here. The camera
app is already one, so this app asks for no camera permission and trusts
no QR library — and a scan while the app is running lands on the same
activity rather than a second copy of it.

Then the app talks:

1. `POST /v1/pair/redeem` with its name and its manifest. The request
   appears in the waiting `erisdb pair` terminal.
2. A screen that says *waiting for approval on my-laptop…*, listing
   exactly what was asked for, with Cancel. It polls
   `GET /v1/pair/status` every 1.5 seconds.
3. Compare the fingerprint with the terminal. Collection is bound to this
   installation’s persistent Iroh key and is repeatable while the ticket lives.
   Save the access token in the sealed store before proceeding. Denied and
   expired tickets return to pairing.
4. `GET /v1/permissions`, because the approval may be narrower than the
   request. See **Permissions**.

A photographed QR is therefore worth nothing on its own: redeeming it
raises a prompt on the operator's screen naming a client they did not
install, asking for permissions they can refuse.

A ticket can also be pasted as text, and an endpoint id with a token from
`erisdb mint` can be typed in by hand — that route skips the conversation,
because a minted token is already a capability. All three sit on one
screen, the first thing an unpaired phone sees, repeated in Settings for
re-pairing. Settings names the core it is paired with, from the ticket's
`name`.

A ticket is read all-or-nothing. No `erisdb://pair/` prefix, a payload
that is not base64url, a `v` this app does not know, neither `eid` nor
`url`, an `eid` that is not 64 hex characters, no token: each is refused
by name rather than stored as half a config. A ticket carrying only `url`
is refused too — that core answers plain HTTP on its LAN, which is a
browser's transport, and this app dials over iroh. One carrying both is
fine; the endpoint id wins. A ticket whose token is an ordinary
capability rather than a pairing code is refused at redeem, where the
core answers 403.

A ticket arriving over a working config asks before replacing it —
replacing drops the tasks cached from the old core — and the pairing
screen is marked `FLAG_SECURE` like the token field on it.

## Permissions

A grant is `<namespace>:<action>`; a required permission is concrete and
a grant may wildcard. `Grants.kt` carries the same matcher the core runs,
so a button this app shows is a request the core will take and a button
it hides is one the core would refuse. `*` matches one segment and, as a
grant's final segment, all remaining ones — which is why `*` means
everything, `meta:*` reaches `meta:pairing:approve`, and `*:read` does
**not** reach `meta:facets:read`.

The manifest is what this app asks for at pairing time:

| grant          | what it buys |
|----------------|--------------|
| `tasks:read`   | the tasks, their history, and the change feed |
| `tasks:create` | add an item and initialize its missing schema |
| `tasks:update` | edit one, tick one off, roll a repeating one forward |
| `tasks:delete` | delete one |

Four, not two: `create` and `update` are separate permissions, so *add
tasks but do not touch mine* is a real answer. `meta:facets:write` is
deliberately not asked for — it is authority over every facet on the
core, including another app's schema, and a tasks app has no business
holding it.

An approval narrower than the manifest is honoured, not worked around.
Without `create` there is no Add button; without `update` the checkboxes
and Save are gone; without `delete` so is Delete; with `read` alone the
app says *read-only* under the title and shows every task. Settings lists
each grant ✓ or ✗. A 403 arriving anyway drops the op from the outbox
naming the permission, rather than retrying forever.

Grants are unknown until `/v1/permissions` answers once, and unknown
draws every button: a phone with no signal at launch is not a phone that
lost its permissions. Saved permissions are refreshed on the next connection.

## Licence

MIT. See [LICENSE](LICENSE).

## Registered access and device verification

Paired access renews using the persistent Iroh identity, even after an offline
period outlasts token expiry. Permissions are re-read during sync. Revocation
stops renewal and clears available write controls. See [installation authority](../../docs/clients.md).

Build both apps and their native libraries with `scripts/build-android.sh` from
the repository root. `tests/android/run.sh` drives both actual APKs on an isolated
emulator through ADB, using a real ErisDB/Iroh endpoint and disposable Postgres.
Unit fixtures do not establish transport or security guarantees; the device and
Rust integration suites exercise those boundaries.
