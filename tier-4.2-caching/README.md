# Tier 4.2 — Caching with Redis: Bloom Filter for the Negative Case

A copy of [Tier 4.1](../tier-4.1-caching/README.md) plus one addition.

**Problem:** `resolve` for a code that was never minted still costs a full
Postgres query, every time — there's no way to cache "this doesn't exist,"
only "this exists, here's the URL."

**Fix:** a RedisBloom filter of every minted code.
- `shorten` adds the new code to it (`BF.ADD`) right after the `INSERT`.
- `resolve`, on a Redis miss, checks it (`BF.EXISTS`) *before* querying
  Postgres — a "definitely absent" answer returns `None` immediately.

**Why it's safe:** a Bloom filter never false-negatives (a real link never
wrongly 404s), it can only rarely false-positive (one wasted-but-harmless
Postgres query). That's the direction you want the error to fall.

**Infra change:** `docker-compose.yml`'s Redis image is now
`redis/redis-stack-server` — plain `redis:7-alpine` doesn't ship the
RedisBloom module (`BF.*` commands).

**Previous:** [Tier 4.1 — Cache-Aside](../tier-4.1-caching/README.md) ·
**Next:** Tier 4.3 — eviction and invalidation
