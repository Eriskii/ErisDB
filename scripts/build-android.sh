#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# cargo-ndk copies all required native dependencies as well as the JNI library.
(cd erisdb-client && cargo ndk -t arm64-v8a -t x86_64 -o ../apps/tasks-android/app/src/main/jniLibs build --release --locked)
mkdir -p apps/lists-android/app/src/main/jniLibs
cp -a apps/tasks-android/app/src/main/jniLibs/. apps/lists-android/app/src/main/jniLibs/
for app in tasks lists; do
  "./apps/$app-android/gradlew" -p "apps/$app-android" --no-daemon testDebugUnitTest assembleDebug "$@"
done
