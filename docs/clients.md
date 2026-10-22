# Installations

An **installation** is an app its owner's mesh let in: the app's Iroh key, the
device that approved it, and the note the device approved it with. A device
creates one by enrolling the app with ErisDB's passphrase; [the mesh](mesh.md)
describes how. Its grants are the note's grants in `database/1`, or fewer once
the operator narrows them.

Postgres owns each installation's status and grants; the core's copy of the
mesh's device list owns whether its device is still a member and its note still
unrevoked. Every request checks both. Core replicas keep no other
authorization state: each one's mesh node keeps its copy of the list current.

Installations live in a dedicated `clients` table, outside generic item CRUD.
Every change to one — enrollment, a permission change, a revocation by the
operator or by the mesh — appends a `system` change event with no item ID and
body `{"event":"client","client":…}`; such events cannot be mistaken for item
snapshots or reverted through the item routes.

## The key is the credential

An app calls ErisDB over Iroh as its key and sends nothing else: no token, no
secret, nothing that expires or renews. ErisDB knows the app by the key the QUIC
handshake proved, so the app keeps the same key for as long as it is the same
app. The Android apps keep theirs in the Android Keystore; the MCP client keeps
its own in a mode-0600 profile file; any other app keeps it where its platform
protects a secret. A copied key is the same app until its installation is
revoked; the app's name is never an authentication factor.

There is at most one active installation per app key. Enrolling the app again
updates it, with the new note's device, id and grants. A revoked installation
stays revoked: enrolling the app again creates a new one, with a new id.

## Transport

Apps reach ErisDB over Iroh, ALPN `eris/database/1`, which supplies its own
mutually authenticated encryption and needs no open port, proxy or certificate.
They find it as the app the mesh most recently approved to serve `database/1`
and dial its endpoint id.

The TCP listener carries bearer tokens only: operator and minted tokens, the
poker's, plugins'. Keep it on loopback, and put a local TLS reverse proxy in
front of it for anything remote. Nothing over TCP is an installation.

## Administration

```sh
export ERISDB_URL=http://127.0.0.1:7700
# ERISDB_SECRET is supplied through the operator's protected environment.
erisdb clients list
erisdb clients show CLIENT_UUID
erisdb clients permissions CLIENT_UUID --grant tasks:read,tasks:create
erisdb clients revoke CLIENT_UUID
```

The API is available to an admin UI without adding a second authority:

| Route | Permission | Request / response |
|---|---|---|
| `GET /v1/clients` | `meta:clients:read` | `{clients: [...]}` |
| `GET /v1/clients/{id}` | `meta:clients:read` | One installation |
| `PUT /v1/clients/{id}` | `meta:clients:write` | `{grants, revision}` → updated installation |
| `POST /v1/clients/{id}/revoke` | `meta:clients:revoke` | Idempotent revocation → installation |

Permission changes must fit both the installation's `approved` grants — its
note's — and the acting administrator's authority. Stale revisions return 409.
A change applies to the app's next request. Revocation is irreversible: the app
gets back in only through a device, as a new installation.

Revocation blocks subsequent requests on every replica, and the tokens the app
delegated. A Postgres notification wakes idle change subscriptions; they recheck
the installation before emitting another event and close when it is revoked.
This does not undo data already delivered or transactions already authorized and
in flight. Plugin invocations are authorized at invocation, not retroactively
cancelled.

## The mesh ends installations too

When any device removes a device from the mesh, or revokes an app's note, the
list reaches ErisDB at once. The affected apps are refused from that moment,
their open streams close, and ErisDB revokes their installations in the registry
with `{"mesh": "device removed"}` or `{"mesh": "note revoked"}` as the audit
event's source. The person needs no access to the server to cut off a lost
phone's apps: removing the phone from any other device does it.

## Keys and backups

Rotating `ERISDB_SECRET` invalidates token signatures and leaves installations
working, since apps hold no tokens. Unless `ERISDB_IROH_SECRET` is set
separately, it also changes the core's Iroh key, which is what its note
approves: the core refuses to serve until it joins the mesh again, and every app
then finds it anew as the mesh's database. Keep the Iroh seed separate when the
signing key must rotate on its own.

Restore `clients`, `items`, `mesh` and `passphrase` from the same Postgres
backup. A restored device list catches up from the devices as soon as the core
syncs, so a device removed since the backup stays removed; an operator's
revocation since the backup does not survive the restore.
