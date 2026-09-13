#!/usr/bin/env bash
# Fires repeated concurrent bursts at the same code on two running app
# instances -- tier-4.1-caching (no lease) and tier-4.3-caching (with a
# lease) -- after clearing that code's cache entry each time, to simulate a
# TTL lapsing under sustained load. Watch the Grafana dashboard at
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

BURST_SIZE="${BURST_SIZE:-50}"
BURST_COUNT="${BURST_COUNT:-10}"
BURST_INTERVAL_SECONDS="${BURST_INTERVAL_SECONDS:-6}"

mint_code() {
  local base_url="$1"
  curl -s -X POST "$base_url/shorten" \
    -H 'content-type: application/json' \
    -d '{"url":"https://example.com/thundering-herd-demo"}' \
    | jq -r .code
}

fire_burst() {
  local base_url="$1" code="$2" n="$3"
  for _ in $(seq 1 "$n"); do
    curl -s -o /dev/null "$base_url/$code" &
  done
  wait
}

echo "Minting a code on each tier..."
CODE_41=$(mint_code "$TIER_41_URL")
CODE_43=$(mint_code "$TIER_43_URL")
echo "  tier-4.1 code: $CODE_41"
echo "  tier-4.3 code: $CODE_43"

echo "Firing $BURST_COUNT bursts of $BURST_SIZE concurrent requests, every ${BURST_INTERVAL_SECONDS}s..."
echo "(open http://localhost:3001 now if you haven't already)"

for i in $(seq 1 "$BURST_COUNT"); do
  echo "burst $i/$BURST_COUNT: clearing cache on both tiers, then firing..."
  docker exec "$TIER_41_REDIS_CONTAINER" redis-cli DEL "$CODE_41" > /dev/null
  docker exec "$TIER_43_REDIS_CONTAINER" redis-cli DEL "$CODE_43" > /dev/null

  fire_burst "$TIER_41_URL" "$CODE_41" "$BURST_SIZE" &
  fire_burst "$TIER_43_URL" "$CODE_43" "$BURST_SIZE" &
  wait

  sleep "$BURST_INTERVAL_SECONDS"
done

echo "Done. Tier 4.1's postgres_queries_total should have grown by roughly"
echo "$BURST_SIZE per burst; Tier 4.3's should have grown by roughly 1."
