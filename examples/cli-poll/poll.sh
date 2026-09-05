#!/bin/sh
# Pull model: register once, then poll `sessions --json` whenever you need state.
# See ../../README.md#integrating. jq gives a formatted table; without it this
# still enforces the protocol check and falls back to printing the raw envelope.
set -eu

BIN="${HOOKLINESINKER_BIN:-hooklinesinker}"
CONSUMER="${HOOKLINESINKER_CONSUMER:-cli-poll-example}"

if ! command -v "$BIN" >/dev/null 2>&1; then
  echo "poll.sh: hooklinesinker binary not found (set HOOKLINESINKER_BIN)" >&2
  exit 1
fi

"$BIN" install --consumer "$CONSUMER" >/dev/null 2>&1 \
  || echo "poll.sh: install --consumer $CONSUMER failed; continuing with a stale registration" >&2

if ! json="$("$BIN" sessions --json)"; then
  echo "poll.sh: '$BIN sessions --json' failed" >&2
  exit 1
fi

# The envelope's own "protocol" is the only field guaranteed to appear before any
# nested per-session "protocol"; cut there so the check never reads a session's copy.
envelope_head=$(printf '%s' "$json" | sed 's/"sessions":\[.*//')
protocol=$(printf '%s' "$envelope_head" | grep -o '"protocol":[0-9]*' | head -n1 | cut -d: -f2)

if [ "$protocol" != "1" ]; then
  echo "poll.sh: refusing envelope with protocol ${protocol:-unknown}; this script speaks protocol 1" >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "poll.sh: jq not found; printing the raw envelope instead of a table" >&2
  printf '%s\n' "$json"
  exit 0
fi

problems=$(printf '%s' "$json" | jq -r '.problems[]? | "\(.observedAt)  \(.message)"')
if [ -n "$problems" ]; then
  echo "problems:"
  printf '%s\n' "$problems"
fi

rows=$(printf '%s' "$json" | jq -r '.sessions[]? | select(.protocol == 1) | [.agent, .phase, .session.id, .session.cwd] | @tsv')
if [ -z "$rows" ]; then
  echo "no live sessions"
  exit 0
fi

{
  printf 'AGENT\tPHASE\tSESSION\tCWD\n'
  printf '%s\n' "$rows"
} | awk -F'\t' '{ printf "%-10s %-12s %-24s %s\n", $1, $2, $3, $4 }'
