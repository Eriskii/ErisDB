# lists-android

The lists client as an Android app, speaking erisdb's native transport:
Iroh QUIC, HTTP/1.1 per bi-stream, no IP or port configured anywhere —
just the server's endpoint id and a capability token.

## Shape

- **`../../erisdb-client/`** — the Rust core: dials by endpoint id (or a
  full `EndpointAddr` JSON), holds one connection, one bi-stream per
  request. Compiled as `liberisdb_client.so` via `cargo ndk` into
  `app/src/main/jniLibs/`. Its contract is pinned by host-side tests
  against a real core over real QUIC.
- **Kotlin shell** — `ErisDB.kt` is the JNI surface (configure + request,
  JSON in/out) and `Core.kt` wraps it in the `CoreApi` interface the rest
  of the app is written against. `Sync.kt` is the read path, `Outbox.kt`
  the write path, `Store.kt` the persistence, `Secrets.kt` the sealed
  token and identity, `Capability.kt` the token's own arithmetic,
  `Pairing.kt` the ticket a core cuts, `Redeem.kt` the two-phase pairing
  conversation, `Grants.kt` the permission grammar and what this app asks
  for, `Facet.kt` the registration; `ListsApp.kt` wires them to the
  screens — pairing screen, list chips,
  add row, entry editor with frontmatter-style attributes. `Markdown.kt`
  is the small inline renderer and `LinkPreview.kt` is the opt-in
  og:image lookup.

The application id remains `dev.bezel.lists` to preserve installed app data.
The source namespace is `dev.erisdb.lists`. The phone mints a random 32-byte
iroh identity on first launch and keeps it, so `source.addr` names this
device stably across sessions. The client string is
`Lists (Android) v0.5` — what the operator reads on the approval prompt.

## The facet

`lists`, shared with the web client at `../lists/`. The name is also the
permission namespace, so the schema version lives in the registration
body: `lists:read` survives a move to schema v2, which is what a
permission should do. The registration also carries `version: 1` and a
line of prose per permission, so the pairing prompt reads *add entries*
rather than `lists:create`.

Registering a facet needs `meta:facets:write`, which is register, change
and remove *every* facet on the core — so this app does not ask for it
(see **Permissions**). An operator who granted `*` gets self-registration
anyway. Without it, the facet is the operator's to register in one
`erisdb` command, and a write against a facet nobody registered comes back
422 and lands on the status line as *the lists facet is not registered on
this core — ask your operator to register it*.

## Sync

First contact reads the change feed's head, then every item in the facet.
In that order: the snapshot is then at least as new as the cursor, and
anything that landed in between simply replays on the next poll —
replaying is free, because a change row *is* the state the write
produced. `GET /v1/items` orders by `updated_at` and answers at most a
thousand rows at a time, so the snapshot is paged forward with
`updated_since` until a short page ends it. A store of any size arrives
whole.

After that the whole dataset never crosses the network again. Every poll
asks `GET /v1/changes?since=<cursor>` and folds the rows it gets into the
cache — created and updated rows carry the body they produced, deleted
rows name an id. The cursor is held on the device, so a restart resumes
and a reinstall reseeds.

The cache is one file, written whole and renamed into place. It is not
SharedPreferences: that reads its entire XML into memory and rewrites all
of it on every commit, which is the wrong shape for a few hundred
kilobytes of entries.

## Offline-first

Every mutation is committed to an on-disk outbox with `commit()`, not
`apply()`, **before** the network is touched: killing the app mid-write
loses nothing, and the op replays on next launch. What the screen shows
is the cache with the queued ops replayed on top, so a change is visible
the instant it is made. The outbox drains in order and stops at the first
transport failure, so ordering holds; permanent rejections — a schema
violation, a 403, a facet nobody registered — drop out with a reason on
the status line instead of retrying forever.

A create names its entry with a pending id the moment it is queued, so
the screen has something to hand back on the next tap. The core mints the
real id when the create lands, and from then on an alias carries every op
that still names the pending one — the delete queued behind the create,
and the edit made a second later from an editor still holding the pending
entry. Without it, offline create-then-delete resurrects the entry and
offline create-then-edit loses the edit.

Sync is serialized. The ten-second poll and every user mutation both ask
for one, and two running at once would read the same outbox, send the
same op, and leave the core holding two identical writes.

## Link previews

An entry's card shows the picture the entry carries in its `image`
attribute. It can also show the one the linked page advertises — og:image
or twitter:image — but finding that means this phone connecting to
whatever address the entry names, which tells the site someone is looking
and from where. That is a decision for the person holding the phone, so
it is a switch in Settings and it is off by default.

With it on, only the public internet is reachable: a link resolving to
loopback, to a private or carrier-grade-NAT range, to link-local
(including the cloud metadata address) or to a unique-local IPv6 address
is not fetched, at any hop of the redirect chain. A hostname answering
with both a public and a private address is refused outright. Resolved
previews are remembered, up to a bounded number of them.

## Token refresh

Tokens carry their own expiry. The lifetime the admin chose at mint time
is captured at connect (`exp − now`) and every sync checks the clock:
under half that lifetime remaining, the app trades the token for a fresh
one via `POST /v1/capabilities/refresh`, asking for the same lifetime.
Refresh moves time, not privilege. A token that never expires is never
refreshed; a dead one says so on the status line. A token whose `exp` has
already arrived is refused when it is offered rather than saved —
there is no lifetime left to preserve, and no refresh can invent one.

## Secrets

The capability token and the iroh private key are the two things on the
phone worth stealing: one is write access to the store, the other is this
device's identity on the network. Both are sealed with an AES-256-GCM key
the Android keystore generates and never hands out; what sits on disk is
ciphertext and the key that opens it cannot leave the device. An install
that predates this carries its values across on first run.

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
cargo ndk -t arm64-v8a -o ../apps/lists-android/app/src/main/jniLibs build --release
cd ../apps/lists-android
./gradlew test              # the sync engine, the outbox, the preview guard
./gradlew assembleDebug     # apk lands in app/build/outputs/apk/debug/
```

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
a QR code holding `bezel://pair/<base64url-nopad(JSON)>`, which carries
where the core is and a **pairing code** — a token holding exactly
`meta:pairing:redeem`, naming one session, good for minutes. It is not a
capability over any data.

Point the phone's camera at it and tap the link it offers: Android routes
the `bezel` scheme to this app. There is no scanner in here. The camera
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

A ticket is read all-or-nothing. No `bezel://pair/` prefix, a payload
that is not base64url, a `v` this app does not know, neither `eid` nor
`url`, an `eid` that is not 64 hex characters, no token: each is refused
by name rather than stored as half a config. A ticket carrying only `url`
is refused too — that core answers plain HTTP on its LAN, which is a
browser's transport, and this app dials over iroh. One carrying both is
fine; the endpoint id wins. A ticket whose token is an ordinary
capability rather than a pairing code is refused at redeem, where the
core answers 403.

A ticket arriving over a working config asks before replacing it —
replacing drops the entries cached from the old core — and the pairing
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
| `lists:read`   | the entries, their history, and the change feed |
| `lists:create` | add an entry |
| `lists:update` | edit one |
| `lists:delete` | delete one |

Four, not two: `create` and `update` are separate permissions, so *add
entries but do not touch mine* is a real answer. `meta:facets:write` is
deliberately not asked for — it is authority over every facet on the
core, including another app's schema, and a lists app has no business
holding it.

An approval narrower than the manifest is honoured, not worked around.
Without `create` there is no Add button; without `update` Save is gone;
without `delete` the press-hold menu offers copying and nothing else;
with `read` alone the app says *read-only* above the cards and shows
every entry. Settings lists each grant ✓ or ✗. A 403 arriving anyway
drops the op from the outbox naming the permission, rather than retrying
forever.

Grants are unknown until `/v1/permissions` answers once, and unknown
draws every button: a phone with no signal at launch is not a phone that
lost its permissions. An install that already holds a token keeps it —
the tokens did not change, only how permissions are named — and reads its
grants back on the next connect.

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
