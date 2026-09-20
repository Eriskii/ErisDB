#!/usr/bin/env sh
# The poker: a clock that smacks an endpoint. It knows one URL, one token,
# and nothing else. All state lives in the store; overlapping pokes are
# harmless because the tick sweep is idempotent.
#
#   ERISDB_URL=http://127.0.0.1:7700 ERISDB_POKER_TOKEN=$(erisdb mint --grant meta:system:tick --no-expiry) ./poke.sh
set -eu
exec curl -fsS -m 30 -X POST "${ERISDB_URL}/v1/tick" \
    -H "Authorization: Bearer ${ERISDB_POKER_TOKEN}"
