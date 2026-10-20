# erisdb-web

ErisDB for browser apps, as WebAssembly. An app signs in by a ticket from ErisAuth and calls
ErisDB over Iroh, through a relay, as its own key. Tasks and Lists (`apps/`) load it from
`apps/shared/erisdb-web/`.

- **The key.** An app is a key: 32 bytes the page makes once and keeps (Tasks and Lists keep
  them in `localStorage`). Its public half is the endpoint id a device approves and ErisDB
  knows the app by, and it is the only credential there is.
- **The relay.** A browser reaches nothing but relays. The client binds one endpoint per key,
  homed on the first relay it needs, with no other way out, as the mesh's `Network::Relay`
  does. Signing in goes through the relay the ticket names; calling ErisDB, through the one
  the mesh's snapshot names for it.
- **Signing in.** The person opens a ticket in ErisAuth on a device of the mesh (**Add a
  browser app**, or `erisauth add-app`). The client redeems it over `eris-pair/0`, asking for
  a note for `erisdb` with the app's grants, not serving, and computes the request's
  fingerprint itself; the device shows the same one. The person compares them and approves
  on the device with ErisDB's passphrase, and the device enrolls the note at ErisDB before it
  approves. The client then collects the note and a snapshot of the mesh, checking that the
  note is for this key, from the device that showed the ticket, and counts in that mesh.
- **Calling ErisDB.** ErisDB is the app approved to serve `erisdb` in the snapshot. Each
  request is one HTTP/1.1 exchange (hyper's client connection) on a QUIC stream of its own,
  under ALPN `erisdb/0`, carrying no token. A failed request is sent again on a fresh
  connection only when nothing of it was sent, or when its method changes nothing.
- **The change feed.** `/v1/changes/stream` on a stream of its own, read as server-sent
  events and handed over one change row at a time. ErisDB closes it when it stops letting
  the app in.

## The JavaScript API

The build is a classic script, `erisdb_web.js`, that defines the global `ErisDBWeb`
(wasm-bindgen's `no-modules` target), and the module it compiles, `erisdb_web_bg.wasm`.

```js
await ErisDBWeb({ module_or_path: fetch("../shared/erisdb-web/erisdb_web_bg.wasm", { integrity }) });

const key = ErisDBWeb.generateKey();          // Uint8Array(32): keep it, the same every load
ErisDBWeb.appId(key);                         // the endpoint id it calls as, hex
ErisDBWeb.readTicket(text);                   // {device, name, relay}, or throws

// Sign in: resolves once the device holds the request.
const joining = await ErisDBWeb.signIn(key, text, "Tasks (Web)", ["tasks:read", "tasks:create"]);
joining.fingerprint;                          // "D3F7-4B3C-2623": the device shows the same
joining.host;                                 // the device's name
joining.saved;                                // keep it to wait again after a reload
const approval = await joining.approval();    // {note, snapshot}, or null when the person said no
await joining.withdraw();                     // or take the request back

const resumed = await ErisDBWeb.resumeSignIn(key, saved);   // a later page, the same answer

// Call ErisDB.
const erisdb = await ErisDBWeb.connect(key, approval.snapshot, "Tasks (Web) v0.3");
const { status, body } = await erisdb.request("POST", "/v1/items", { facet: "tasks", body: { title: "…", done: false } });
const changes = await erisdb.changes(since, "tasks");          // resolves once ErisDB opens it
for (let change; (change = await changes.next());) apply(change);
changes.close();
erisdb.close();                               // hang up; the next request dials again
await ErisDBWeb.closeEndpoint(key);           // leave the relay, for signing out
```

`request` answers whatever ErisDB answers, `{status, body}`, the body JSON where it is JSON;
a 401 means ErisDB no longer lets the app in. What fails otherwise throws an `Error` with a
`code`:

| `code`        | Means                                                                              |
| ------------- | ---------------------------------------------------------------------------------- |
| `bad_key`     | The key is not 32 bytes                                                            |
| `bad_ticket`  | Not an `eris://pair/` ticket, or a damaged one                                     |
| `no_relay`    | The ticket, or the mesh for its ErisDB, names no relay a browser could reach it by |
| `no_erisdb`   | The snapshot holds no ErisDB                                                       |
| `unreachable` | The device or ErisDB could not be reached, or stopped answering                    |
| `used`        | The ticket was redeemed already                                                    |
| `expired`     | The ticket ended, or no longer works                                               |
| `refused`     | The device turned the request away; or ErisDB would not open the change feed, with its `status` and `body` |
| `bad_request` | Something the client was handed does not read                                      |
| `internal`    | Anything else, with what went wrong                                                |

A page that loads it needs, in its content security policy, `'wasm-unsafe-eval'` and the
glue's hash in `script-src` (load the glue with the same hash as its `integrity`), and a
`connect-src` that reaches the relay over WebSocket. `scripts/update-csp.py` writes Tasks'
and Lists'.

## Building

```sh
scripts/build-web.sh        # from the ErisDB checkout; or npm run build:web
```

It builds the crate for `wasm32-unknown-unknown` in release, runs wasm-bindgen into
`apps/shared/erisdb-web/`, and pins the result in the apps' policies. It needs the
`wasm32-unknown-unknown` target, the wasm-bindgen CLI at the version `Cargo.toml` pins (on
`PATH`, at `~/.cargo/bin`, or named by `WASM_BINDGEN`), and a C compiler for wasm32, which
ring needs: `CC_wasm32_unknown_unknown`, or a clang with the wasm32 target. With Nix and
neither set, the build runs in [`shell.nix`](shell.nix), which provides one. The crate
builds inside the Eris monorepo, whose Cargo configuration supplies ErisOverlayNetwork.

## Testing

The browser suite runs Tasks and Lists in Chromium against real processes: an Iroh relay in
plain-HTTP mode on loopback ([`tests/browser/relay`](../../tests/browser/relay)), ErisAuth as
the person's device starting a mesh, ErisDB's core on PostgreSQL in Docker, joined to the
mesh through that device with its passphrase set, and the static apps. Every participant
reaches the others through the relay alone (`ERIS_NETWORK`). The tests act as the person
through ErisAuth's socket and look at what ErisDB holds as its operator. They cover signing
in by ticket with the fingerprint on both screens, a reload while the person decides, a
wrong passphrase and then the right one, part of the grants approved, the person saying no,
create, update and delete, a change from another client arriving live, writes waiting while
ErisDB is away, and the app cut off when its note is revoked, then let in again.

```sh
npm ci && npx playwright install chromium
npm run test:e2e
```

It needs Docker and a Rust toolchain, and builds the relay, ErisDB's core and ErisAuth on its
first run: ErisAuth from `ERISAUTH_SOURCE` (`../ErisAuth` beside this checkout by default),
or ready-built from `ERISAUTH_BIN`; `ERISDB_BIN` and `ERISDB_RELAY_BIN` do the same for the
others. `ERISDB_CHROMIUM` names a Chromium to run instead of Playwright's, as on NixOS:
`ERISDB_CHROMIUM=$(command -v chromium) npm run test:e2e`. It tests the client as built into
`apps/shared/erisdb-web/`, so rebuild after changing the crate.
