# The mesh

ErisDB lives in its owner's mesh: the person's devices, each running
[ErisAuth](https://github.com/Eriskii/ErisAuth), which keep one signed list of
who is in ([ErisOverlayNetwork](https://github.com/Eriskii/ErisOverlayNetwork)).
Apps are not members. A device approves an app with a signed **note** naming the
app's Iroh key, the service it takes part in, and the grants it holds there.

ErisDB is one such app: the one approved to serve `erisdb`, which makes it the
mesh's ErisDB. A mesh has one. Every app that uses it is another: approved by a
device for `erisdb` with the grants the person chose, and enrolled here with
ErisDB's passphrase. After that the app's key is its credential, and nothing
else is.

## ErisDB joins

```sh
erisdb join             # where ErisAuth runs; approve it on the device
erisdb passphrase set   # the passphrase devices type to let apps in
erisdb serve
```

`erisdb join` asks this machine's ErisAuth, over its agent socket
(`$XDG_RUNTIME_DIR/erisauth/agent.sock`), to approve the core's Iroh key to
serve `erisdb`, with no grants. The key is the one derived from
`ERISDB_IROH_SECRET`, or `ERISDB_SECRET` when that is unset; `erisdb endpoint-id`
prints it. ErisAuth tells the person that this app wants to be the mesh's ErisDB.
When they approve, the device signs the note, and revokes the note of any ErisDB
it approved before: the newly approved one is the mesh's ErisDB. `join` keeps the
note and the mesh as the device showed it in Postgres, in the `mesh` table.
Joining again replaces both. `--name` sets what the device shows; it defaults to
`ErisDB`.

`erisdb serve` refuses to start before `join`, and refuses a note for another
key: the note approves one key, so the secret it was derived from stays the same.
Serving, the core runs a mesh node under its note. The node keeps ErisDB's copy
of the device list current in the same table, and is served as `eris-mesh/0`
beside `erisdb/0` on the core's one Iroh endpoint. Devices push every addition,
removal and revocation to it the moment they make one.

The core is the mesh's ErisDB while its own note counts. When the person
approves another ErisDB, the device revokes this one's note, and the list
carries the revocation here at once: the core logs `this ErisDB's approval no
longer counts`, and from then on refuses enrollments and passphrase changes with
`503 not_the_erisdb`, since apps find the other one. The same happens if the
device that approved it leaves the mesh. `erisdb join` makes it the mesh's ErisDB
again, which in turn retires the other. Should two ever count at once, apps take
the most recently approved.

`--network` (`ERIS_NETWORK`) says how the core reaches the mesh: `internet`, the
default, finds participants anywhere through relays and DNS and on the same
network through mDNS; a relay URL uses that relay alone; `loopback` is for tests.

## The passphrase

Letting an app into ErisDB takes the person's approval on a device and ErisDB's
passphrase, which ErisDB checks itself. The operator sets the first one on the
server with `erisdb passphrase set`: typed twice at a terminal, or read as one
line from standard input. Any device in the mesh can change it by knowing the
current one (`POST /v1/passphrase`). It is at least 8 characters and is kept as
an Argon2id hash in its own table, `passphrase`; no grant reaches it.

Wrong guesses are counted per device:

| Wrong guesses in a row | Then |
|---|---|
| 1, 2 | Nothing |
| 3 | The device waits 1 minute |
| 4 | 2 minutes |
| 5 | 4 minutes |
| Each one after | Double the last wait, up to a day |

A right guess clears the device's count. While a device waits, ErisDB checks
nothing it sends and answers `429 locked` with `retry_after` seconds. One
device's guesses never lock out another, so a stolen phone guessing away leaves
the laptop free to enroll apps, and to remove the phone.

`erisdb passphrase unlock DEVICE` clears one device's wrong guesses, named by its
endpoint id; `erisdb passphrase unlock` clears every device's. Setting a new
passphrase on the server leaves the counts as they are. `--lockout-secs`
(`ERISDB_LOCKOUT_SECS`) sets the first wait, 60 seconds by default; the doubling
and the day's ceiling follow from it. The lockout test sets it to one second.

## An app gets in

1. The app asks ErisAuth to approve its key for `erisdb`, not serving, with the
   grants it wants. On Linux it asks over the agent socket; on Android, ErisAuth's
   bound service; an app with no ErisAuth beside it, such as one in a browser,
   redeems a device's `eris://pair/` ticket.
2. The person approves on the device — all the grants asked for, or fewer — and
   types ErisDB's passphrase.
3. ErisAuth signs the note and enrolls it at the mesh's ErisDB, calling from the
   device's own endpoint: `POST /v1/enroll` with the note and the passphrase.
4. ErisDB checks that the caller is the device that signed the note and is in the
   mesh; that the note counts in ErisDB's list (same mesh, device a member, not
   revoked); that it approves an app to call `erisdb`, not to serve it; and that
   the passphrase is right. It creates the app's installation, recording the
   device and the note.
5. ErisAuth answers the app with its note and a snapshot of the mesh. The app
   finds ErisDB in the snapshot as the app approved to serve `erisdb`, and dials
   its endpoint id with ALPN `erisdb/0`, as its own key.

The Rust client does the app's side in one call, `erisdb_client::sign_in`, and
offers ErisAuth's side as `Client::enroll`. Nothing ever talks to any ErisDB but
the mesh's.

Enrolling an app that has an active installation updates that installation: same
id, the new note's device, id and grants. A revoked installation stays revoked;
enrolling its app again makes a new one.

## Browser apps

A browser app has no ErisAuth beside it and reaches nothing but relays. It gets
in by ticket, as WebAssembly: [ErisDB's browser client](../clients/web/README.md),
which Tasks and Lists load from `apps/shared/erisdb-web/`.

1. The page makes the app's key once and keeps it; Tasks and Lists keep it in
   `localStorage`. Its public half is the endpoint id the device approves.
2. The person chooses **Add a browser app** in ErisAuth on a device of the mesh
   (or runs `erisauth add-app`), which shows an `eris://pair/` ticket: the
   device, the relay that reaches it, and a one-time code.
3. The page redeems the code over the relay, asking for a note for `erisdb`
   with its grants, not serving. Both screens show the request's fingerprint,
   each computing it for itself, and the page shows the device's name.
4. The person compares the fingerprints and approves on the device with ErisDB's
   passphrase; ErisAuth enrolls the note here, as for any app, and only then
   approves. The page collects its note and a snapshot of the mesh. Only the
   key that redeemed the code can collect the answer, so a page that reloads
   while the person decides waits for the same answer under the same key.
5. The page finds ErisDB in the snapshot and dials it through the relay the
   snapshot names for it, with ALPN `erisdb/0`: one HTTP/1.1 exchange per QUIC
   stream, the change feed included, and no token.

From then on the page is an app like any other: a revoked note or a removed
device cuts it off at once, its change feed closes and its next request answers
401, and a new ticket lets the same key in again. Signing out forgets the key.

## Every request

An app calls over Iroh with no `Authorization` header. ErisDB looks up the
active installation for the key the QUIC handshake proved, and lets it in while
its device is a member of the mesh and its note is not revoked, as ErisDB's
list stands at that moment. Its grants are the installation's: what the note
approved, or less once the operator narrows them.

A request carrying a bearer token is authorized by the token instead. Operator,
minted and delegated tokens work over TCP and Iroh alike. A token an
installation mints names the installation, and holds no more than it does for as
long as it counts.

## Leaving

Any device can remove a device from the mesh or revoke an app's note. The list
carries it to ErisDB at once, and from then on the affected apps' requests are
refused with 401, their open change streams close, and the tokens they minted
stop working. ErisDB also revokes their installations in the registry, each with
an audit event on the feed whose source is `{"mesh": "device removed"}` or
`{"mesh": "note revoked"}`.

The operator narrows an installation's grants, never past its note's, or revokes
it outright with `erisdb clients`; see [installations](clients.md).

ErisDB decides nothing about who is in the mesh and holds no device's key. A
device holds no ErisDB data grants: here it can let apps in and change the
passphrase, and nothing more.

The routes are in the [API reference](api.md#enrollment); installations, their
administration and transport are in [clients.md](clients.md).
