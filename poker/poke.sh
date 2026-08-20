#!/usr/bin/env sh
# The poker: a clock that smacks an endpoint. It knows one URL, one token,
# and nothing else. All state lives in the store; overlapping pokes are
# harmless because the tick sweep is idempotent.
#
#   BEZEL_URL=http://127.0.0.1:7700 BEZEL_POKER_TOKEN=$(bezel mint --facets system --verbs write --ttl 0) ./poke.sh
set -eu
exec curl -fsS -m 30 -X POST "${BEZEL_URL}/v1/tick" \
    -H "Authorization: Bearer ${BEZEL_POKER_TOKEN}"
