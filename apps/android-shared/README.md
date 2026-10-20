# Shared Android client infrastructure

Both Android applications compile `src/main/java` through their Gradle main
source sets. This directory owns signing in through ErisAuth and its screen,
the sealed key and the ErisDB it was let into, core requests, outbox
processing, and synchronization.

The JNI bridge retains the package `dev.erisdb.client` required by the Rust
library. Common application code uses `dev.erisdb.android`. ErisAuth's client
library, `ErisAuth.kt` and `Wire.kt` in `dev.eris.auth.client`, is vendored
file for file from ErisAuth's `android/client`. Each application keeps its own
schemas, storage and keystore names, domain models, and screens.

Build both applications with `scripts/build-android.sh` from the repository root.
Exercise the installed applications against real services with
`tests/android/run.sh` on a disposable emulator.

Licensed under [MIT](../../LICENSE-MIT).
