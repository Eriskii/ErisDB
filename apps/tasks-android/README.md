# tasks-android

The tasks client as an Android app, speaking erisdb's native transport:
Iroh QUIC, HTTP/1.1 per bi-stream, no IP or port configured anywhere. It
gets into ErisDB through [ErisAuth](https://github.com/Eriskii/ErisAuth) on
the phone, and its own Iroh key is its credential from then on.

## Shape

- **`../../erisdb-client/`** — the Rust core: reads where the mesh's ErisDB
  is from a snapshot of the mesh, dials it as the app's key, holds one
  connection, one bi-stream per request. Compiled as `liberisdb_client.so`
  into `app/src/main/jniLibs/`. Its schema is checked by host-side tests
  against a real core over real QUIC.
- **`../android-shared/`** — what Tasks and Lists share: `ErisDB.kt` is the
  JNI surface (endpoint id, locate, dial, request; JSON in and out),
  `Core.kt` wraps it in the `CoreApi` interface the rest of the app is
  written against, `Sync.kt` is the read path, `Outbox.kt` the write path,
  `Secrets.kt` the sealed key and the ErisDB it was let into, `SignIn.kt`
  the conversation with ErisAuth and `SignInScreen.kt` its screen. ErisAuth's
  client library is vendored there as it is, in `dev.eris.auth.client`.
- **Kotlin shell** — `Store.kt` is the persistence, `Notifications.kt` the
  due notices, `Grants.kt` the permission grammar and what this app asks
  for, `Facet.kt` the registration; `TasksApp.kt` wires them to the
  screens. `Recurrence.kt` is the repeat rule as pure `java.time` and
  `Task.kt` is the thin JSON skin over it.

The application ID is `dev.erisdb.tasks`.
The source namespace is `dev.erisdb.tasks`. The phone mints a random 32-byte
Iroh key on first launch and keeps it: it is the key ErisAuth approves and
ErisDB lets in, and `source.addr` names it. The client string is
`Tasks (Android) v0.4` — the name the person reads on ErisAuth's prompt.

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
carries `version: 1` and a line of prose per permission, so screens that
name a permission read *add tasks* rather than `tasks:create`.

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
no Android — and is checked by `app/src/test/java/dev/erisdb/tasks/`:
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
revisions; deleted rows remove items. The app saves the ErisDB's endpoint id, the
items, and their feed cursor together in one file using Android's `AtomicFile`. Memory also holds
the items and cursor together, so one activity cannot advance another activity's
cursor past records it has not received.

An absent, unreadable, or other ErisDB's snapshot triggers a full fetch instead
of resuming from a cursor whose items are missing. This keeps the app's key, its
ErisDB and pending edits. A valid snapshot displays immediately, including when
the app restarts offline; a failed save reports that the offline copy could not
be saved.

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

## Secrets

The app's Iroh key is the one thing on the phone worth stealing: it is
this app's way into the store. It is sealed, with the address of the
ErisDB it was let into, by an AES-256-GCM key the Android keystore
generates and never hands out; what sits on disk is ciphertext and the
key that opens it cannot leave the device. Should the keystore lose that
key, the app makes a new Iroh key, which no device has approved, and signs
in again.

`android:allowBackup="false"`. Nothing here belongs in Google's cloud or
on a device-to-device transfer: the sealed key would arrive elsewhere as
noise, and everything else is a cache of the core that reseeds on first
sync.

## Build

From the repository root:

```sh
scripts/build-android.sh
adb install -r apps/tasks-android/app/build/outputs/apk/debug/app-debug.apk
```

It builds `erisdb-client` as `liberisdb_client.so` for `arm64-v8a` and
`x86_64` with the NDK's toolchain (through `cargo ndk-env`), copies it into
both apps' `jniLibs/`, runs their unit tests (`./gradlew testDebugUnitTest`:
signing in, the sealed credentials, the sync engine, the outbox, the
recurrence rule) and assembles the debug APKs. It needs the Android SDK and
NDK, `cargo-ndk`, and the `aarch64-linux-android` and `x86_64-linux-android`
Rust targets.

`java.time` runs back to minSdk 24 through core library desugaring.

On NixOS the NDK's prebuilt toolchain needs its ELF interpreter and rpath
patched (patchelf against nix glibc/zlib/libstdc++), and gradle needs a
real JDK 17 registered by absolute path. Those paths are per-machine, so
they live in `~/.gradle/gradle.properties` rather than in this repo:

```properties
org.gradle.java.installations.paths=/nix/store/<hash>-openjdk-17.../lib/openjdk
android.aapt2FromMavenOverride=/path/to/patched/aapt2
```

## Signing in

The app gets into ErisDB through ErisAuth on the phone, and only so. The
phone is a device of its owner's mesh, the mesh has an ErisDB, and ErisAuth
holds the phone's place in it. Without ErisAuth the app's first screen says
to install it and add the phone to the mesh with it; there is no other way
in.

1. The app binds ErisAuth's `dev.eris.auth.SIGN_IN` service and asks it to
   approve its key — its endpoint id, from `nativeEndpointId` — for the
   service `erisdb`, as a client (`serves` false), under its client string,
   with its manifest. It asks on its first launch, and again whenever the
   person taps **Sign in with ErisAuth**.
2. ErisAuth shows the person a prompt naming the app as Android names it,
   by its package and the SHA-256 of its signing certificate. They approve
   all of the manifest or part of it, and type ErisDB's passphrase.
   ErisAuth signs the app's note and enrolls it at the mesh's ErisDB, which
   checks the passphrase.
3. Once ErisDB has taken the note, ErisAuth answers with it and a snapshot
   of the mesh. `nativeLocate` reads where ErisDB is from the snapshot; the
   app seals that beside its key before anything else, then `nativeDial`s it.
4. `GET /v1/permissions`, because the approval may be narrower than the
   request. See **Permissions**.

Every later launch dials the ErisDB it kept, as its key, with nothing else.
A denial, or a failure such as a phone in no mesh or a mesh with no ErisDB,
is said on the sign-in screen, with **Ask again**. Settings names the ErisDB
the app is signed in to and signs in again, to ask for what the person did
not give the first time or to reach a new ErisDB; a new ErisDB drops the
tasks cached from the old one.

ErisDB lets the app in while its installation stands, its device is still in
the mesh and its note is not revoked. When any of that ends the next request
is refused with 401, the status line says ErisDB no longer lets the app in,
and only signing in again brings it back.

The app's half of ErisAuth's protocol is ErisAuth's client library,
vendored in `android-shared` (`dev.eris.auth.client`); `SignIn.kt` is the
conversation and what its answers come to.

## Permissions

A grant is `<namespace>:<action>`; a required permission is concrete and
a grant may wildcard. `Grants.kt` carries the same matcher the core runs,
so a button this app shows is a request the core will take and a button
it hides is one the core would refuse. `*` matches one segment and, as a
grant's final segment, all remaining ones — which is why `*` means
everything, `meta:*` reaches `meta:clients:revoke`, and `*:read` does
**not** reach `meta:facets:read`.

The manifest is what this app asks ErisAuth for when it signs in:

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

## Tests on a device

`tests/android/run.sh` drives both actual APKs on an isolated emulator through
ADB, signing in through a real ErisAuth on the emulator to a real ErisDB in a
real mesh. Unit fixtures do not establish transport or security guarantees; the
device and Rust integration suites exercise those boundaries.

## Licence

MIT. See [LICENSE](LICENSE).
