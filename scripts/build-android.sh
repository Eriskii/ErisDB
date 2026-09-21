#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# cargo-ndk copies all required native dependencies as well as the JNI library.
native_dir="$(mktemp -d)"
trap 'rm -rf "$native_dir"' EXIT
(cd erisdb-client && cargo ndk -t arm64-v8a -t x86_64 -o "$native_dir" build --release --locked)
for app in tasks lists; do
  for abi in arm64-v8a x86_64; do
    destination="apps/$app-android/app/src/main/jniLibs/$abi"
    mkdir -p "$destination"
    # Hashed dependency filenames change after updates; remove obsolete generated
    # libraries only after their replacements have built successfully.
    rm -f "$destination"/liberisdb_client.so "$destination"/libiroh*.so
    cp -a "$native_dir/$abi/." "$destination/"
  done
  "./apps/$app-android/gradlew" -p "apps/$app-android" --no-daemon testDebugUnitTest assembleDebug "$@"
done
