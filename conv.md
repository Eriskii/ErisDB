k thanks. one more thing. rn to connect clients u gotta put in a massive iroh ID and token. I'm thinkin of how to make it smaller. what if there's just a small server somewhere. you tell the server to prepare for a connection, and it wedges it's ID and token in there, outside clients cannot read - and it gives u a short code, like 8-10 characters, then you put that into the app, it grabs the stuff from the server, and before the server hands over creds it checks with the bezeldb (yours) and it asks you if the client looks correct. Safe or not safe. thoughts?

(⌐■_■)

Yes — the general pairing-code idea is sound, but I would change one important detail:

don’t have the rendezvous server hand the client a reusable Bezel credential.

Make the short code authorize a pairing ceremony, after which the Bezel instance itself gives that specific client its own credential.

Iroh itself is already built around dialing an EndpointId, and connections are end-to-end encrypted; current Iroh also has hooks for connection inspection/authentication. So your little public service only really needs to solve human-friendly rendezvous, not become a credential custodian.

I’d do something approximately like:

BEZELDB                         PAIR SERVER                         CLIENT
   │                                │                                │
   │ create pairing session         │                                │
   │───────────────────────────────>│                                │
   │                                │                                │
   │     code: K7M4-VP2Q            │                                │
   │                                │                                │
   │                                │<──── redeem K7M4-VP2Q ─────────│
   │                                │     + client ephemeral key      │
   │                                │                                │
   │<──── pairing attempt ──────────│                                │
   │      client pubkey/info        │                                │
   │                                │                                │
   │   "Allow this client?"         │                                │
   │          [approve]             │                                │
   │                                │                                │
   │════════════════ authenticated / E2E connection ════════════════>│
   │                                                                │
   │──── issue CLIENT-SPECIFIC credential directly ─────────────────>│
   │                                                                │

The pairing server can tell the client:

the Iroh endpoint ID
routing/discovery information
an ephemeral pairing nonce / session ID
whatever non-secret metadata is necessary to establish the connection

But the actual durable auth credential should originate from BezelDB after approval and travel across the authenticated encrypted connection.

Why I like that better

If your proposed server stores:

ABCD1234 -> {
    endpoint_id,
    master_bezel_token
}

then that little convenience server becomes a hilariously juicy database. Compromise it and suddenly an attacker potentially gets credentials for every currently pairing Bezel instance.

Instead, it can store something like:

ABCD1234 -> {
    endpoint_id,
    pairing_session_id,
    ephemeral_public_key,
    expires_at
}

None of which should grant access to the actual database by itself.

Then compromising the pairing service mostly lets someone interfere with pairing attempts, rather than immediately impersonate approved clients.

8–10 characters is plenty, with conditions

I'd use something like Crockford Base32 and drop ambiguous characters:

7K3M-9QXP

Eight Base32 characters gives you about 40 bits of entropy; ten gives you about 50 bits.

For a credential that is:

one-use
expires after ~5 minutes
aggressively rate-limited
invalidated after successful pairing
requires interactive approval anyway

40–50 bits is extremely comfortable against online guessing.

I actually prefer 8 characters formatted XXXX-XXXX over 10 if humans will type it manually. You could go to 10 if you want an extra margin.

Do not make this a permanent address alias, though. K7M4VP2Q should disappear after pairing.

One subtle attack: racing the real client

Imagine I see someone's pairing code somehow.

They type:

K7M4-VP2Q

but I redeem it milliseconds before them.

If redemption automatically consumes the code, I've at least created an easy DoS. If redemption automatically receives credentials, much worse.

Your approval step largely solves the security part:

New client requesting access

Android
BezelDB Client 0.4.2

Fingerprint:
WOLF-COMET-TEA

[ Deny ] [ Allow ]

But “client looks correct” is not really authentication. Device name, OS, application version, etc. can all be spoofed.

I'd bind approval to an ephemeral client public key.

For extra niceness, derive a tiny human-readable verification value from the full pairing transcript:

Client:
   WOLF  COMET  TEA

Server:
   WOLF  COMET  TEA

If both displays match, you know the rendezvous server hasn't swapped keys.

You probably don't even need users to do this in the normal flow; it could be an expandable “verify connection” feature. But cryptographically, it gives you a path where the pairing server doesn't need to be trusted for identity.

And definitely issue per-client credentials

I wouldn't have this:

bezel_token = one giant secret every app shares

after pairing.

Have:

Client A
  id: 73a...
  public_key: ...
  permissions: ...
  created: ...
  revoked: false

Client B
  id: c82...
  ...

Then losing a phone means:

bezel clients revoke c82...

rather than rotating the universe.

It also naturally gives you a future permissions UI:

Claude MCP
✓ tasks
✓ workouts
✓ mail
✗ admin

My Phone
✓ everything

Tiny Workout App
✓ workouts

Even if you initially make every paired client omnipotent, designing the credential format around client identity + grants now will save you pain later.

There's an even nicer conceptual split

I'd call the short code a pairing code, not an auth token.

Its only authority is:

“I possess the temporary capability necessary to ask this BezelDB instance to pair with me.”

It doesn't mean:

“I possess the capability necessary to access this BezelDB.”

That distinction is very useful.

In fact, the Iroh EndpointId itself doesn't need to be treated as sensitive. Iroh's model is specifically that you can connect to another device if you know its EndpointId; the transport provides encrypted connections keyed to endpoint identity. It's your application authorization layered over that connection that should determine whether the peer gets to do anything.

So:

PAIR CODE
    ↓
discover endpoint
    ↓
request pairing
    ↓
human approves client key
    ↓
Bezel issues client capability
    ↓
normal operation

is the architecture I'd choose.

And this is arguably nicer UX than copying the huge Iroh stuff while preserving the fun P2P property: the centralized thing is only a phone book / introduction service. It doesn't have to sit in the data path afterward.

One other current-Iroh note: if you're self-hosting Iroh relays, make sure you're on a patched release; Iroh disclosed and fixed a pre-auth relay DoS in 1.0.2 in July 2026.

This makes sense. Think of any way to do it that isn't me needing to have a server up? Could I somehow make this, too, infraless?

(✧▽✧)

Yes. I think you can make “I don't operate any pairing infrastructure” a real property, with one caveat:

If you want an arbitrary remote client to type 8–10 characters and discover an arbitrary 256-bit Iroh identity, something somewhere has to hold the mapping.

That's basically an information-theory problem. The trick is deciding whether that “something” is physical proximity, a QR code, or decentralized/public infrastructure rather than your server.

Current Iroh actually gives you some very useful pieces here: it has separate mDNS address lookup for local-network peers and Pkarr/Mainline-DHT address lookup for decentralized global resolution of known endpoint IDs.

I think the nicest BezelDB design is actually three pairing modes

Nearby pairing: genuinely infraless.

If the phone and BezelDB are on the same LAN, use mDNS. Bezel advertises:

BezelDB: my-laptop
pairing: available
ephemeral pairing key: ...

The phone sees:

Nearby BezelDBs

> alex-laptop

Tap it → Bezel displays an approval/SAS → approve → they exchange permanent client identities.

No code. No server. No DHT. No copying an Iroh ID.

Iroh already has an mDNS address-lookup implementation specifically for endpoints on the local network.

For the overwhelmingly common case of “I'm installing my little app on my phone and connecting it to my Bezel running here”, that's probably better UX than a short code anyway.

QR/deep-link pairing: also genuinely infraless, and works remotely.

This might be the best general solution.

Bezel prints:

┌───────────────────┐
│                   │
│      QR CODE      │
│                   │
└───────────────────┘

bezel://pair/...

The QR contains the otherwise-huge:

endpoint ID
ephemeral pairing secret/key
protocol/version info

The human never sees it.

Scan → Iroh connects → Bezel asks:

Pair "Workout Tracker"
on Pixel 10?

[Allow] [Deny]

Once paired, throw the one-time secret away and persist the client's public key/capability.

That's zero rendezvous infrastructure at all. Iroh tickets are explicitly intended to bundle connection information and application-specific data; they're just long when represented as text. QR codes don't care.

For terminals, this is especially cute because you can render the QR directly in the terminal.

And for desktop-to-desktop:

bezel pair

could output both:

[QR]

or open:
bezel://pair/docaaaca...

So you get scanning, clicking, copy/paste, etc.

The really interesting one: decentralized remote short codes

You could replace your pairing server with the BitTorrent Mainline DHT.

Iroh's ecosystem already uses Pkarr over Mainline for decentralized endpoint address lookup, and n0 currently maintains iroh-mainline-address-lookup for exactly that purpose.

Conceptually you'd want:

BezelDB
   │
   │ publish temporary rendezvous record
   ▼
BITTORRENT MAINLINE DHT
   ▲
   │ lookup "F7KM-2QWP"
   │
Client

Then your infrastructure dependency becomes:

“the BitTorrent DHT exists”

rather than:

“bezel.dev has a server I pay for and keep running.”

Which is extremely on-brand for what you're making.

But: Pkarr doesn't directly give you arbitrary short-code → record storage. BEP44 mutable records are addressed from an Ed25519 public key (plus optional salt); you can't simply choose an arbitrary 40-bit lookup key and PUT something there.

So I'd consider this a little Bezel protocol you'd need to design, rather than “turn on an Iroh option.”

And there is an awkward security property: if you derive the DHT signing key directly from:

F7KM-2QWP

then the signing key has only ~40 bits of effective entropy. Anyone who observes the record can potentially brute-force the human code offline and then forge the rendezvous record.

You can mitigate that because:

pairing sessions expire in minutes;
possessing the code doesn't grant database access;
Bezel still asks for approval;
permanent credentials are client-specific;
the actual Iroh connection authenticates its endpoint identities.

But I wouldn't build a supposedly cryptographically pristine protocol around a low-entropy DHT signing key without thinking through it carefully.

So DHT short-code rendezvous is feasible-ish, but it's considerably more protocol work than the QR solution.

There's one thing I'd change regardless: get rid of the existing shared token

Your pairing flow means you might not need that giant manually configured token at all.

You already have cryptographic endpoint identities. Iroh endpoints use Ed25519 identities, and the resulting connections are end-to-end encrypted/authenticated.

So bootstrap with a one-time pairing capability, then have Bezel say:

client endpoint ID:
    abc123...

approved:
    yes

capabilities:
    [read, write, ...]

From then on:

Iroh peer identity -> Bezel client identity

is your authentication anchor.

You might issue an additional application-layer capability/token if you need delegation, scopes, multiple identities behind one Iroh endpoint, etc., but it doesn't need to be a giant global secret someone configures manually.

What I'd ship

I'd honestly make pairing UX:

$ bezel pair

Waiting for a new client...

█████████████████
██  ▄▄▄▄▄  █ ...
██  █   █  █ ...
██  ▀▀▀▀▀  █ ...
█████████████████

Scan this QR

Nearby devices can also discover this BezelDB automatically.

Manual pairing:
    [whatever fallback]

And in apps:

Connect to BezelDB

[ Find nearby ]

[ Scan QR ]

[ Enter pairing code ]

Where:

Find nearby → mDNS, totally infrastructurless.

Scan QR → complete Iroh bootstrap information, totally infrastructurless.

Enter pairing code → optional convenience rendezvous mechanism. This could initially use a tiny hosted broker and eventually move to a decentralized design.

The nice part is that the protocol doesn't have to care how discovery happened:

             mDNS ───────┐
                         │
QR / deep link ──────────┼─→ EndpointID + pairing capability
                         │               ↓
short-code broker ───────┘          Iroh connect
                                         ↓
                                  user approval
                                         ↓
                                 permanent client

So you can ship the simple reliable options first without committing yourself to operating infrastructure.

And I'd arguably make QR + mDNS the primary experience, not the short code. An eight-character code feels elegant, but scanning a QR and having the new app instantly appear in:

New client "Exercise Tracker" wants access

is arguably an even better demonstration of the whole Bezel idea.