# Tier 4.1 — Caching with Redis: Cache-Aside

**Problem being solved:** Tier 3 fixed durability by moving the mapping into
Postgres, but that traded away Tier 1's O(1) in-memory lookup for a B-tree
lookup that has to hit disk. Every redirect — Tier 0's ~99% of traffic — now
pays that cost. This tier puts Redis in front of Postgres so most redirects
never touch the database at all.

---

## Cache-aside, concretely

`resolve` (the redirect path):
1. `GET` the code from Redis.
2. On a hit, return it — Postgres is never touched.
3. On a miss, run Tier 3's `SELECT` against Postgres, then `SET` the result
   into Redis with a TTL before returning it.

`shorten` writes through: after Tier 3's `INSERT`, it also `SET`s the new
mapping into Redis — so a code's *first* redirect is already a cache hit
instead of a guaranteed miss.

Redis is framed deliberately as **Tier 1's hash table, moved onto the
network and shared between processes** — same structure (`GET`/`SET` is
just `HashMap::get`/`insert` over a wire), different location. If that
mental model from Tier 1 stuck, this tier is mostly plumbing.

### Why Postgres is written before Redis, not after

`shorten` does `INSERT` then `SET`, in that order, on purpose. Writing Redis
*first* would make a code externally resolvable before its row is durably
committed: a failed `INSERT` after a successful `SET` would leave Redis
confidently pointing at a URL Postgres never actually has. Durable-source-of-
truth-first, cache-second is the rule; getting this backwards is a real
correctness bug, not just a stylistic preference.

### What a racing `SET` actually does

Two concurrent requests writing the same key doesn't merge or compare
anything — `SET key value EX ttl` unconditionally overwrites whatever was
there, value and TTL both. Whichever `SET` lands last simply wins. It's
harmless here specifically because both racing writes compute the identical
value from the same immutable row; if the value being cached could ever
differ between two writers, this same mechanism would silently let a stale
write clobber a fresh one.

### This version is deliberately vulnerable to a thundering herd

There is no coordination at all yet: if a hot key's TTL lapses under
sustained load, every in-flight request misses Redis in the same instant and
every one of them independently queries Postgres to rebuild the identical
entry. That's a later quarter's problem to fix (distributed lock /
singleflight / stale-while-revalidate) — building the unprotected version
first is the right order, since the mitigation only makes sense once you've
felt the failure it's fixing.

---

## What breaks here

| Problem | Addressed in |
|---|---|
| A `resolve` for a code that was never minted still costs a full Postgres query, every time. | Tier 4.2 — Bloom filter |
| No coordination on a cache-miss stampede (thundering herd). | A later Tier 4.N |
| Still one counter in one process. | Tier 5 — distributed ID generation |

---

## Discussion questions

1. `CACHE_TTL_SECONDS` is a flat 300 seconds regardless of a link's actual
   traffic. Tier 0 §3.1 said access is Zipfian — a few URLs take most of the
   traffic. What would a smarter TTL policy look like, and what would it
   need to know that a flat constant doesn't?
2. `resolve` never writes to Redis on a miss that also misses Postgres (a
   `None` result is never cached), so a code that was never minted pays a
   full Postgres query on *every* request. What's a cheap way to short-
   circuit that without caching a real value?
3. Nothing invalidates a cached entry yet, because there's no `PATCH`/`DELETE`
   to invalidate it *for*. Once Tier 0's deferred `PATCH /{code}` exists,
   walk through what has to happen in `shorten`'s Postgres-then-Redis
   ordering discipline for an *update* instead of an insert.
4. `resolve`'s Redis check and Postgres fallback happen sequentially. Under
   what circumstance would checking both *concurrently* (e.g. `tokio::join!`)
   be a bad idea, given the whole point of caching is to avoid the Postgres
   query on a hit?
5. Estimate the redirect latency win. Tier 3's discussion questions had you
   measure a Postgres round trip; look up a typical local Redis `GET`
   latency and compare. At Tier 0's ~4,000 reads/sec, what does the
   database's query load look like once, say, 95% of reads hit cache?

**Previous:** [Tier 3 — Persistence](../tier-3-persistence/README.md) ·
**Next:** [Tier 4.2 — Bloom Filter for the Negative Case](../tier-4.2-caching/README.md)
