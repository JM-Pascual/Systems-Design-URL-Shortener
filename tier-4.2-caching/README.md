# Tier 4.2 — Caching with Redis: Bloom Filter for the Negative Case

A copy of [Tier 4.1](../tier-4.1-caching/README.md) plus one addition.

**Problem:** `resolve` for a code that was never minted still costs a full
Postgres query, every time — there's no way to cache "this doesn't exist,"
only "this exists, here's the URL."

**Fix:** a RedisBloom filter of every minted code.
- `shorten` adds the new code to it (`BF.ADD`) *before* the `INSERT`.
- `resolve`, on a Redis miss, checks it (`BF.EXISTS`) *before* querying
  Postgres — a "definitely absent" answer returns `None` immediately.
- `connect` backfills it from `SELECT code FROM links` on every start.

**Why it's safe:** a Bloom filter never false-negatives (a real link never
wrongly 404s), it can only rarely false-positive (one wasted-but-harmless
Postgres query). That's the direction you want the error to fall.

**Keeping it safe** is the subtle part, because the guarantee only holds
while the filter actually contains every code — and the filter lives in
Redis, which is otherwise treated as disposable:

- The `BF.ADD` goes *before* the `INSERT` — the opposite order from the
  value cache, deliberately. A code in the filter whose `INSERT` then fails
  is a false positive (harmless). A committed row whose `BF.ADD` then fails
  would be a false negative: a real link that 404s forever.
- The startup backfill means a Redis flush or restart can't empty the
  filter out from under Postgres. Without it, `docker compose down -v`
  would turn every existing link into a 404. `BF.ADD` is idempotent, so
  re-adding what's already there costs nothing.

**Infra change:** `docker-compose.yml`'s Redis image is now
`redis/redis-stack-server` — plain `redis:7-alpine` doesn't ship the
RedisBloom module (`BF.*` commands) — and runs with `--appendonly yes` on a
volume, since Redis now holds something worth not losing on a restart.

**Previous:** [Tier 4.1 — Cache-Aside](../tier-4.1-caching/README.md) ·
**Next:** [Tier 4.3 — Invalidation and a Lease](../tier-4.3-caching/README.md)
