# Shared Android client infrastructure

Both Android applications compile `src/main/java` through their Gradle main
source sets. This directory owns capability parsing, core requests, outbox
processing, synchronization, pairing, and the pairing input screen.

The JNI bridge retains the package `dev.erisdb.client` required by the Rust
library. Common application code uses `dev.erisdb.android`. Each application
keeps its own schemas, storage and keystore names, domain models, and screens.

Build both applications with `scripts/build-android.sh` from the repository root.
Exercise the installed applications against real services with
`tests/android/run.sh` on a disposable emulator.

Licensed under [MIT](../../LICENSE-MIT).
