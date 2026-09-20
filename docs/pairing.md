# Pairing

`erisdb pair` prints a QR and a copyable deep link. A ticket carries the core
address and a short-lived capability to request enrollment. It does not contain
a data-access token. No mDNS, short-code broker, or DHT rendezvous is involved.

```sh
erisdb pair --name my-laptop --client-url https://db.example.com
# Also export a camera-readable file immediately:
erisdb pair --qr-output /tmp/erisdb-pair.png
```

During the initial wait, `s` also saves a PNG. EOF, cancellation and expiry end
the wait; the CLI tries to deny a cancelled session. The ticket's default lifetime
is ten minutes. `--token-ttl` sets the approved access-token lifetime (up to seven
days), independent of the installation's continuing ability to renew.

## Ceremony

1. The app scans, opens, or pastes the ticket and asks for its manifest's grants.
2. The core binds the request to the authenticated Iroh key or the browser's
   S256 commitment. Both screens display a comparison fingerprint derived from
   the session ID, identity, and requested permissions.
3. The person compares both fingerprints, then approves the request or a subset:

   ```text
   Compare this code with the app: 12AB-34CD-56EF
   Tasks (Android) wants:
     1. tasks:read
     2. tasks:create

   [a] approve as asked   [s] select   [d] deny
   ```

4. Only that installation can collect the credential. Collection creates its
   registration transactionally. Repeating collection with the same proof is
   safe while the ticket remains live, including after a lost response.
5. The app saves its installation credential and renews access until revoked.

`[s]` accepts comma-separated permission numbers. There is no grant-everything
shortcut. Every grant must fit the app request and the approver's own authority.
A copied QR can race to request enrollment, so compare fingerprints rather than
trusting a device name. After another installation has redeemed it, possessing
the QR alone cannot collect its approval.

## Ticket format

The existing URI prefix remains compatible across the rename:

```text
bezel://pair/<base64url-nopad(JSON)>
```

```json
{
  "v": 1,
  "name": "my-laptop",
  "eid": "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06",
  "url": "https://db.example.com",
  "token": "bz1.…"
}
```

`v` and `token` are required, along with at least one of `eid` and `url`.
`name` is an untrusted label. `eid` is a 64-character hex Iroh endpoint ID;
`url` is an HTTPS base URL (HTTP only on localhost). Native apps use Iroh;
browsers need a URL. Reject unknown versions, damaged encodings and missing
addresses rather than guessing. The base64url payload has no `=` padding.

Browser camera scanning uses the platform BarcodeDetector when available;
pasting remains available when the browser cannot scan. Android's system camera
opens the deep link through the app's intent filter. Both Android apps share the
scheme, so the system may ask which app should receive the ticket.

The API contract is in [api.md](api.md#pairing); installation identity, renewal,
revocation, transport, and migration are in [clients.md](clients.md).
