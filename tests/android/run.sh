#!/usr/bin/env bash
# The Android device suite: Tasks and Lists on an isolated emulator, signing in through ErisAuth
# on the emulator to a real ErisDB in a real mesh. It needs an emulator on adb with internet access,
# Docker, this machine's internet access, and the APKs built first (scripts/build-android.sh here
# and in ErisAuth); e2e.py's header says it all. Arguments name the apps to run: tasks, lists.
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 tests/android/e2e.py "$@"
