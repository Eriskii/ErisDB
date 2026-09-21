# Pairing and client authorization audit

This records the state at migration commit `959d36a`, before the follow-up
implementation requested in `conv.md`.

## Already implemented

- Terminal QR rendering and PNG export (`erisdb/src/pair.rs`), with PNG
  decoding tests using a real QR decoder.
- Versioned pairing tickets, browser camera scanning and ticket pasting,
  and Android camera/deep-link handling.
- Persistent pairing sessions, human approval or denial, requested grants,
  approval of a subset, and grants bounded by the approver's authority.
- Namespaced permissions and enforcement across data, history, feed,
  plugin, pairing, and administrative endpoints.
- Token refresh, bounded delegation, and expiry of active change streams.
  Browser and Android apps refresh automatically, but the original default
  chain ends after 30 days and expired tokens cannot renew.
- Stable native installation keys and encrypted Android credential storage.

## Missing or needing correction

- Pairing collection authenticates the QR's bearer code, without proving
  that the collector is the client whose request the operator approved.
- No registered installation model or individual revocation. Iroh peer
  identity is recorded for attribution but does not constrain a token.
- No refresh credential independent of access-token expiry, so long-offline
  apps must pair again.
- No per-client permission management or revocation of open subscriptions.
- Approval currently permits grants beyond the app's request, including an
  explicit grant-everything terminal option.
- Pairing collection changes state through a GET and loses recovery after
  a response is lost. Native code treats GET as safely retryable.
- The terminal polling loop does not terminate cleanly on expiry or stdin
  closure, and does not check unsuccessful polling responses.
- Browser/Android full application flows are not covered by existing real
  end-to-end suites. Android unit tests use fake core implementations.

## Agreed scope

- QR, deep links, and pasted tickets. No mDNS or short-code broker/DHT.
- Each installation has its own identity.
- Postgres-backed immediate revocation, including renewal and subscriptions.
- Paired installations renew until revoked, including after access expiry.
- Pairing approval grants only requested permissions or a subset.
- Browser transport is HTTPS remotely, with HTTP allowed on localhost.
- Client administration through CLI and API initially.

## Baseline verification

The core, Rust client, and MCP bridge passed Clippy with warnings denied
and all Rust suites: 149 tests total. The integration suites use real
Postgres containers, TCP/HTTP, Iroh QUIC, and MCP subprocesses. Both browser
scripts passed JavaScript syntax checks.

Android verification initially hit a stale Nix loader in the local AAPT2
binary. A task-local wrapper supplies the installed loader without changing
the machine's Gradle configuration. Android tests remain separate from
actual device end-to-end coverage.

## Implemented after the audit

- Registered installations, with native Iroh key binding or a browser/MCP
  renewal secret whose S256 commitment is all the server stores.
- Comparison fingerprints on terminal, browser and Android approval screens.
- Proof-bound, repeatable collection with one transactional registration.
- Re-pairing updates the existing installation; stale approvals cannot undo
  permission changes or revocation, and concurrent enrollment cannot duplicate it.
- Renewal independent of access-token expiry, continuing until revocation.
- Postgres-backed permission changes and revocation, including delegated tokens
  and idle subscriptions across replicas; registry changes are audited.
- `erisdb clients list/show/permissions/revoke`, plus the corresponding API.
- Approval confined to the requested scope and the approver's own grants.
- Terminal cancellation/expiry handling and immediate `--qr-output` export.
- Browser HTTPS enforcement, redirect refusal, pending-pair reload recovery,
  current permission UI, and shared enrollment/renewal code with checked CSP hashes.
- MCP terminal pairing, automatically saved credentials, optional independent
  profiles, and renewal on an explicit authorization failure. Re-pairing preserves
  the existing identity and replaces its saved credential atomically.
- Android comparison codes, live permission/revocation UI, and rebuilt ARM64
  and x86_64 JNI libraries under the new name.

See [clients.md](clients.md) for the architecture, API, transport and migration
contract. Existing unregistered tokens retain their old semantics; re-pair them
for individual revocation. No mDNS or short-code service was added.

## Verification of the implementation

Locally passed: Clippy with warnings denied on all three Rust components;
166 Rust tests (including real Postgres, TCP, Iroh, terminal/PNG decoder and MCP
subprocess flows); all three Chromium E2E scenarios against a real core and
Postgres; 209 Android unit tests and both APK builds with real ARM64/x86_64 JNI.

The QR theft regression was reproduced before the fix: a second client holding
only the photographed ticket received the approved credential. It now receives
401. Additional integration cases cover post-expiry renewal, foreign Iroh keys,
concurrent collection, grant intersection/ceilings, stale revisions, renewal-proof
secrecy, delegated revocation, closing idle subscriptions across replicas, and
upgrading real pre-registry and duplicate-registration databases without losing
data or audit history. Real MCP processes also prove that default storage works,
re-pairing preserves identity, denial preserves the saved login, and revoking one
profile does not affect another.

`tests/android/e2e.py` drives actual APK screens through ADB, then validates writes
and installation identity in the actual core. It exercises deep-link pairing,
fingerprint comparison, UI writes, renewal after a stopped-app expiry, permission
changes and revocation. This host has no emulator, connected device or KVM; device
execution is assigned to the Android CI job and must be reported separately from
unit/build results.

## Dependency review

The 2026-09-21 UTC RustSec scan found two vulnerable locked dependencies. All
three component lockfiles now use rustls 0.23.45 or newer for
[RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285.html), and the
core's h2 was updated to 0.4.16 for
[RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html).
The other two components already held newer h2 versions. All three also replace
the withdrawn chacha20 0.10.1 with 0.10.2. The repeated scans report zero known
vulnerabilities and zero withdrawn releases; npm audit also reports zero.

Two informational warnings remain visible, without blanket suppressions:

- `paste` 1.0.15 is an unmaintained procedural macro used by Iroh's Linux netlink
  dependencies. It runs at build time; RustSec reports no vulnerability in it.
- `lru` 0.16.4 is used only by the `rqrr` test dependency. Its warning concerns a
  panicking key destructor during `LruCache::pop()`. The decoder uses `u8` keys
  and `pop_lru()`, and it is absent from production builds. No compatible upstream
  decoder update was available at this check.

CI audits the exact Rust lockfiles and npm dependencies on each run. Dependency
advisory checks complement the actual authorization and transport tests; they do
not establish that arbitrary application behavior is secure.
