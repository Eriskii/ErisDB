#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
ERISDB_TEST_IROH=1 node tests/browser/server.cjs > /tmp/erisdb-android-core.log 2>&1 &
fixture_pid=$!
trap 'kill "$fixture_pid" 2>/dev/null || true' EXIT
for attempt in $(seq 1 600); do
  if curl --silent --fail http://127.0.0.1:18770/ready > /dev/null; then break; fi
  if ! kill -0 "$fixture_pid" 2>/dev/null; then cat /tmp/erisdb-android-core.log; exit 1; fi
  sleep 1
done
python3 tests/android/e2e.py
