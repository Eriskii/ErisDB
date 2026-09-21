# Registered installations

An **installation** is durable, revocable authority owned by one app installation.
A **pairing session** is its short-lived enrollment ceremony. An **access token**
is an expiring, signed limit on what that installation may do. These are distinct
objects: losing an access token does not lose the installation, and possessing a
QR ticket does not prove possession of the installation key.

Postgres owns registration status and current permissions. Core replicas have no
local authorization cache. Each registered request verifies the token, loads its
installation, checks revocation and identity, then intersects token grants with
current installation grants. Delegated tokens retain the installation ID, so
revocation and permission reductions apply to them too.

Re-pairing with the same installation key updates its existing active registration
and replaces its permissions with the newly approved set. There is at most one
active registration per proof. Revoked registrations stay revoked: a fresh human
approval creates a new registration, so old tokens cannot return to use. Browser
apps reuse their current proof only when pairing with the same core URL.
Approval records the registration's current revision. Collection returns 409 if
another approval, permission change or revocation has since changed it; an old
approval cannot silently restore older authority.

Installations live in a dedicated `clients` table, outside generic item CRUD.
Pairings remain `pair` items for history and approval. Registry changes produce
`system` change events, with no item ID; they cannot be mistaken for a pairing
snapshot or reverted through ordinary item routes.

## Identity and renewal

- **Native apps:** the actual authenticated Iroh peer key is the identity.
  Collection, every registered access token, and renewal are bound to that key.
  Keep the same private key across process restarts. Android encrypts it and its
  token using its installation's Android Keystore.
- **Browser and HTTPS MCP clients:** generate a random 32-byte secret locally.
  Redeem with `challenge = base64url(SHA256(ASCII(base64url(secret))))`. Collection
  and renewal send the original base64url secret in `X-ErisDB-Client-Proof`.
  Postgres and audit records contain only the commitment. The encoding uses the
  [S256 construction](https://www.rfc-editor.org/rfc/rfc7636#section-4.2); this is
  an installation credential, not an OAuth authorization-code implementation.
  Access tokens are ordinary bearer credentials on HTTPS.

Default access lifetime is seven days, maximum seven days. The installation
renews until explicitly revoked, including when its last access token expired
while offline. An operator may explicitly set `max_ttl_secs` at approval to make
an installation temporary; absent that field there is no 30-day re-pairing clock.

Renew with `POST /v1/clients/{id}/refresh`, body `{}` or
`{"ttl_secs": 3600}`. A requested lifetime cannot exceed the approved access
lifetime. No bearer token is needed: prove the installation identity instead.
The response is `200 {token, exp, grants, client_id}`. An old token alone cannot
recover full installation authority. The access-token payload's `client` is only
a routing hint to clients; the server authenticates the actual key or secret.

The browser apps save the renewal secret with their existing local configuration
and save pending enrollment separately so reloads can resume collection. Serve
these apps from an origin you control: scripts executing on that origin can read
browser storage. MCP automatically saves its pairing in the user's configuration
directory; optional named profiles give separate applications independent identities.
Its credential files use mode 0600 on Unix, are replaced atomically, and never
appear in tool results. File backups must be protected
as credentials. A copied installation key/secret is the same identity until
revoked; display names are never an authentication factor.

## Transport

Browsers and MCP accept HTTPS remotely and HTTP only on localhost. HTTP
redirects are refused for credential-bearing calls. Run the core's TCP listener
on loopback behind a local TLS reverse proxy. The registry's HTTP authentication
checks the actual loopback TCP peer, never `Forwarded` or `X-Forwarded-For`.
A reverse proxy on another host/container bridge needs a local forwarding hop;
do not expose an unencrypted proxy on a remote interface. Iroh supplies its own
mutually authenticated encryption and needs no TLS proxy.

Serve the browser apps with `apps/shared/auth.js` at its relative path. Exact
script hashes are in each app's CSP and the shared script's integrity attribute.
After edits, run `python3 scripts/update-csp.py`; CI checks for stale hashes.

## Administration

```sh
export ERISDB_URL=http://127.0.0.1:7700
# ERISDB_SECRET is supplied through the operator's protected environment.
erisdb clients list
erisdb clients show CLIENT_UUID
erisdb clients permissions CLIENT_UUID --grant tasks:read,tasks:create
erisdb clients revoke CLIENT_UUID
```

The API is available to a future admin UI without adding a second authority:

| Route | Permission | Request / response |
|---|---|---|
| `GET /v1/clients` | `meta:clients:read` | `{clients: [...]}` |
| `GET /v1/clients/{id}` | `meta:clients:read` | One registration |
| `PUT /v1/clients/{id}` | `meta:clients:write` | `{grants, revision}` → updated registration |
| `POST /v1/clients/{id}/revoke` | `meta:clients:revoke` | Idempotent revocation → registration |
| `POST /v1/clients/{id}/refresh` | Installation proof | `{ttl_secs?}` → access credential |

Permission changes must fit both the original app request and the acting
administrator's authority. Stale revisions return 409. Reductions apply on the
next authorization check; increases require renewal before old narrower tokens
can use them. Revocation is irreversible: pair again to create a new registration.

Revocation blocks subsequent authorization and renewal on every replica. A
Postgres notification wakes idle change subscriptions; they recheck registration
before emitting another event and close when revoked. This does not undo data
already delivered or transactions already authorized and in flight. Plugin
invocations are authorized at invocation, not retroactively cancelled.

## Migration and recovery

Migration 0006 adds the registry and denies old unfinished pairings, because they
have no installation binding. Cut fresh QR tickets after updating core and apps.
Already issued legacy tokens remain valid under their existing scope and expiry;
they have no `client` claim and cannot be individually revoked. Re-pair apps to
register them. Manual/operator tokens keep their bounded refresh-chain behavior.

Migration 0007 enforces one active registration per proof. If an early registry
contains duplicates, it retains the newest and revokes the older registrations
with audit records. Approved but uncollected pairings without revision anchors
are denied and must be restarted.

Signing-key rotation invalidates old token signatures, but a registered client
can obtain a new token using its still-valid installation proof. Revoke the
registration to end that authority. Keep `ERISDB_IROH_SECRET` separate if a signing
key rotation must preserve the core's address. Restore `clients` and `items` from
the same Postgres backup; restoring an older backup can also restore authority
that had since been revoked.
