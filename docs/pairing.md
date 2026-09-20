# Pairing

Connecting a client to an ErisDB means telling it where the core is and
getting it a token. Typing either by hand is miserable — an endpoint id is
64 hex characters and a token is longer.

So the core shows a scannable ticket, and no pairing service exists
anywhere: the phone learns the address from the QR code, and nothing in the
middle ever holds the mapping. What the ticket does *not* carry is
authority. It carries a code to start a conversation, and a human ends it.

## The ticket

```
bezel://pair/<base64url-nopad(JSON)>
```

The JSON, compact, with no whitespace:

```json
{
  "v": 1,
  "name": "my-laptop",
  "eid": "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06",
  "url": "http://192.168.1.20:7700",
  "token": "bz1.eyJncmFudHMi…"
}
```

| field   | required | meaning |
|---------|----------|---------|
| `v`     | yes      | Ticket version. `1`. A client that does not know the version refuses the ticket rather than guessing. |
| `token` | yes      | The **pairing code**: a token holding exactly `meta:pairing:redeem` and naming its own session, good for minutes. It is not a capability over any data — the client redeems it and a human decides what it gets. |
| `eid`   | one of   | Iroh endpoint id, 64 lowercase hex characters. What a client dials when it speaks QUIC. |
| `url`   | one of   | Plain HTTP base URL. What a client dials when it does not — browsers, mostly. |
| `name`  | no       | A label for the human, so an app can say *paired with my-laptop*. Never trusted for anything. |

At least one of `eid` and `url` must be present. A ticket carrying both lets
one QR code serve a browser on the LAN and a phone anywhere, and each client
picks the transport it can actually speak.

Base64url is unpadded (RFC 4648 §5, no `=`). The payload is the only thing
that is encoded; `bezel://pair/` is literal.

## Reading one

Every client platform can parse a ticket with what it already has — that is
why the format is base64url of JSON and not something denser.

- **Browser** — `atob` after mapping `-_` to `+/`, then `JSON.parse`.
- **Kotlin** — `android.util.Base64.URL_SAFE or NO_PADDING`, then `org.json`.
- **Rust** — `base64::engine::general_purpose::URL_SAFE_NO_PAD`, then `serde_json`.

A client must reject a ticket that: is not `bezel://pair/`-prefixed, does not
decode, carries an unknown `v`, has neither `eid` nor `url`, or has an `eid`
that is not 64 hex characters. Refusing loudly beats storing half a config.

## Cutting one

```sh
erisdb pair --name my-laptop
```

No scope flags. The client says what it wants; you answer. `erisdb pair`
cuts a code against a running core over the ordinary API — the same
endpoints a dashboard uses — prints the QR as block characters with the
fields underneath, and waits. Press `s` while waiting to write
`~/erisdb-pair-<name>.png`, for when a terminal renders blocks badly.

When a client redeems, the request appears:

```
Tasks (Android) v0.3 wants:
  1. tasks:read
  2. tasks:create
  3. tasks:update
  4. tasks:delete

[a] approve as asked   [e] everything   [s] select   [d] deny
```

`s` takes a comma-separated list of numbers, so approving part of a request
is one keystroke away rather than a special case.

## What a ticket is worth

**A ticket is worth nothing on its own.** The code in it grants exactly
`meta:pairing:redeem` on one session and expires in minutes. Photographing
a pairing screen gets an attacker as far as raising a prompt on your screen,
naming a client you did not install, asking for permissions you can refuse.

That is the property the two-phase flow buys, and it is why the approval
step exists rather than the QR simply carrying a capability. What still
deserves care:

- Approve the narrowest set that works. `s` exists for exactly this.
- `e` grants `*`. It is one keystroke and it is a master key; the prompt
  says so.
- An approval cannot exceed the approver. An operator holding only
  `tasks:*` cannot grant `lists:read`, whatever the client asked for.
- The token is handed to the client **once**. Collecting it clears it from
  the session, so a replayed code cannot fetch it again.

## Where this goes

One upgrade is worth the protocol work later, and the ticket format is
versioned so it can land without breaking clients:

- **Endpoint identity as the auth anchor.** Iroh connections already
  authenticate both ends with Ed25519. Once a client is approved, its
  endpoint id could *be* its identity, with capabilities attached to it
  server-side — and then no long-lived bearer token needs to exist at all.

Nearby discovery over mDNS is a second direction: on one LAN a client could
find a core with no ticket at all. It needs the approval step above to be
safe, so it follows rather than leads.
