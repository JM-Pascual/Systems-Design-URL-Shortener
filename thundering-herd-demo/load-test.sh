#!/usr/bin/env bash
# Fires repeated concurrent bursts at several codes on two running app
# instances -- tier-4.1-caching (no coordination) and tier-4.3-caching (a
# lease in the key) -- after clearing those codes' cache entries each time, to
# simulate TTLs lapsing under sustained load. Watch the Grafana dashboard at
# http://localhost:3001 while this runs.
#
# Assumes:
#   - tier-4.1-caching is running on :3000 (its own docker-compose stack up)
#   - tier-4.3-caching is running on :3002 (its own docker-compose stack up)
#   - thundering-herd-demo's docker-compose (Prometheus/Grafana) is up
set -euo pipefail

TIER_41_URL="${TIER_41_URL:-http://localhost:3000}"
TIER_43_URL="${TIER_43_URL:-http://localhost:3002}"
TIER_41_REDIS_CONTAINER="${TIER_41_REDIS_CONTAINER:-tier-41-caching-redis-1}"
TIER_43_REDIS_CONTAINER="${TIER_43_REDIS_CONTAINER:-tier-43-caching-redis-1}"

KEYS="${KEYS:-20}"                 # codes per tier, each a cold key in every burst
BURST_SIZE="${BURST_SIZE:-100}"    # concurrent requests per code (curl caps one process at 300)
BURST_COUNT="${BURST_COUNT:-5}"
BURST_INTERVAL_SECONDS="${BURST_INTERVAL_SECONDS:-8}"

# Each request is a socket on both ends; macOS shells often default to 256.
ulimit -n "$(ulimit -Hn)" 2>/dev/null || ulimit -n 10240

mint_codes() {
  local base_url="$1"
  for _ in $(seq 1 "$KEYS"); do
    curl -sf -X POST "$base_url/shorten" \
      -H 'content-type: application/json' \
      -d '{"url":"https://example.com/thundering-herd-demo"}' \
      | jq -r .code
  done
}

# One curl process per code, holding BURST_SIZE connections open at once.
fire_burst() {
  local base_url="$1"; shift
  for code in "$@"; do
    yes "url = \"$base_url/$code\"
output = \"/dev/null\"" | head -n $((BURST_SIZE * 2)) \
      | curl -s -Z --parallel-immediate --parallel-max "$BURST_SIZE" -K - &
  done
  wait
}

echo "Minting $KEYS codes on each tier..."
CODES_41=($(mint_codes "$TIER_41_URL"))
CODES_43=($(mint_codes "$TIER_43_URL"))

TOTAL=$((KEYS * BURST_SIZE))
echo "Firing $BURST_COUNT bursts of $TOTAL concurrent requests per tier"
echo "($KEYS keys x $BURST_SIZE), every ${BURST_INTERVAL_SECONDS}s..."
echo "(open http://localhost:3001/d/thundering-herd now if you haven't already)"

for i in $(seq 1 "$BURST_COUNT"); do
  echo "burst $i/$BURST_COUNT: clearing cache on both tiers, then firing..."
  docker exec "$TIER_41_REDIS_CONTAINER" redis-cli DEL "${CODES_41[@]}" > /dev/null
  docker exec "$TIER_43_REDIS_CONTAINER" redis-cli DEL "${CODES_43[@]}" > /dev/null

  fire_burst "$TIER_41_URL" "${CODES_41[@]}" &
  fire_burst "$TIER_43_URL" "${CODES_43[@]}" &
  wait

  sleep "$BURST_INTERVAL_SECONDS"
done

echo "Done. Per burst, Tier 4.1's postgres_queries_total should grow by about"
echo "$TOTAL (one per request), and Tier 4.3's by about $KEYS (one per key)."
