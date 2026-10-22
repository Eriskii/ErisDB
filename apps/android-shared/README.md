# Shared Android client infrastructure

Both Android applications compile `src/main/java` through their Gradle main
source sets. This directory owns signing in through ErisOverlayNetwork and its screen,
the sealed key and the ErisDB it was let into, core requests, outbox
processing, and synchronization.

An app asks ErisOverlayNetwork to use a database, the capability `database/1`
(`CAPABILITY` in `SignIn.kt`), with the grants it wants there, and serves
nothing. ErisDB is what serves `database/1` in the mesh; the snapshot that
comes with an approval says where it is, and `nativeLocate` reads it.

The JNI bridge retains the package `dev.erisdb.client` required by the Rust
library. Common application code uses `dev.erisdb.android`. ErisOverlayNetwork's client
library, `ErisOverlayNetwork.kt` and `Wire.kt` in `dev.eris.eon.client`, is vendored
file for file from ErisOverlayNetwork's `android/client`. Each application keeps its own
schemas, storage and keystore names, domain models, and screens.

Build both applications with `scripts/build-android.sh` from the repository root.
Exercise the installed applications against real services with
`tests/android/run.sh` on a disposable emulator.

Licensed under [MIT](../../LICENSE-MIT).
