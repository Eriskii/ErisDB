#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# The JNI library links everything it uses statically. It builds with the NDK's compilers, as
# cargo-ndk sets them out, and links with the NDK's clang for each target. Cargo runs from the
# filesystem root, so a workspace around this checkout that patches dependencies in its Cargo
# configuration cannot rewrite the lockfile this build is locked to.
manifest="$PWD/erisdb-client/Cargo.toml"
declare -A triple=([arm64-v8a]=aarch64-linux-android [x86_64]=x86_64-linux-android)
for abi in arm64-v8a x86_64; do
  target="${triple[$abi]}"
  (
    source <(cargo ndk-env -t "$abi")
    export "CARGO_TARGET_$(tr 'a-z-' 'A-Z_' <<<"$target")_LINKER=$(dirname "$CLANG_PATH")/${target}21-clang"
    cd / && cargo build --manifest-path "$manifest" --target "$target" --release --locked
  )
done
for app in tasks lists; do
  for abi in arm64-v8a x86_64; do
    destination="apps/$app-android/app/src/main/jniLibs/$abi"
    mkdir -p "$destination"
    rm -f "$destination"/*.so
    cp "erisdb-client/target/${triple[$abi]}/release/liberisdb_client.so" "$destination/"
  done
  "./apps/$app-android/gradlew" -p "apps/$app-android" --no-daemon testDebugUnitTest assembleDebug "$@"
done
