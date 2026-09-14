# Tier 4.3 — Caching with Redis: Invalidation and a Lease

A copy of [Tier 4.2](../tier-4.2-caching/README.md) plus three additions
that all serve the same goal: closing the gaps between "the cache says X"
and "the database says Y."

---

## `PATCH`/`DELETE` and cache invalidation

Tier 0's two deferred write operations, finally implemented:

- **`PATCH /{code}`** — `UPDATE`s the row, then invalidates the cache with a
  plain `DEL`.
- **`DELETE /{code}`** — removes the row, then `DEL`s the cache entry too.
  Without this, a deleted link would keep redirecting successfully off a
  stale cached value until its TTL happened to lapse on its own — the
  answer to "what does a cache hit mean once the row is gone?"

Both write Postgres first, same durability discipline as `shorten`: only
invalidate the cache once the new state is durably committed.

`DEL` here is deliberately the *simple* invalidation strategy from Tier 4.1's
discussion — "immediate `DEL` (simple, but stampedes on a hot key)" — rather
than a write-through `SET`. That's not an oversight: it's the concrete
trigger the lease below exists to defend against. A write-through `SET`
would have made the lease largely unnecessary for this trigger, but it
would have hidden the actual failure mode instead of fixing it.

One loose end, worth naming rather than silently accepting: `DELETE` cannot
remove a code from the Bloom filter (Tier 4.2) — standard Bloom filters have
no removal operation. Harmless, not a bug: `resolve` still falls through to
Postgres and correctly returns `None`; the code just permanently loses the
filter's fast-path benefit.

---

## `allkeys-lfu` eviction

`docker-compose.yml` now sets `maxmemory-policy allkeys-lfu` on Redis. This
is the second of the three thundering-herd triggers from the root README:
under memory pressure, a naive eviction policy can evict a key that's still
*hot*, not just old — collapsing to the exact same stampede as a TTL lapsing,
just triggered by memory instead of the clock. LFU (least-*frequently*-used,
tracked via an access counter rather than recency) keeps a hot key resident
specifically because it's still being read, which is what makes Tier 0's
Zipfian access pattern (a few URLs take most of the traffic) so cache-friendly
in the first place.

`maxmemory` itself is left at Redis's default (`0`, unlimited) so normal
runs and tests never evict anything unexpectedly. To actually reproduce the
trigger: `redis-cli CONFIG SET maxmemory 1mb` against a running instance,
then watch cold keys get evicted before hot ones under load.

---

## A real lease against the thundering herd

Before this quarter, `resolve`'s miss path had no coordination at all: every
in-flight request that missed Redis at the same instant independently
queried Postgres to rebuild the identical entry. `resolve` now does this on
a miss:

```text
loop:
    if the real key exists in Redis -> return it
    try to acquire lease:{code}  (SET ... NX EX 5)
    if acquired:
        query Postgres, populate the real key (or release the lease on a
        genuine miss), return
    else:
        sleep briefly, then loop back to the top
```

The critical property: a loser's next step after sleeping is to **try to
acquire the lease itself again** — not just to poll the real key and give up
after a timeout. If the current leader crashes before populating the cache,
its lease simply expires on its own `EX` after 5 seconds, and the *very
next* waiter's retry succeeds immediately and becomes the new leader. No
request ever falls through to querying Postgres unguarded, and no request
can be blocked forever by a leader that never comes back — the lease's TTL
is what bounds that.

### The lease's release isn't fenced

The success path's `redis.del(&lease_key)` deletes whatever is currently at
that key — it never checks that it's still *this* worker's lease. If a
leader finishes unusually late, after its own lease already expired and a
second worker re-acquired it, that late `DEL` deletes the second worker's
still-active lease instead of its own. Harmless here specifically because
the real cache key is always written before the lease is released, so
anyone new checks the real key first and never reaches the lease at all —
but a textbook-correct lease would use a unique per-holder token and a
conditional delete (a fencing token) to close this gap outright rather than
relying on that argument. See `QUESTIONS.md`.

This is deliberately the first, simplest mitigation from the root README's
list ("a distributed lock: `SET key val NX EX ttl`"), not the most
sophisticated one (singleflight and a TTL keep-alive are still ahead in
Tier 4.4). It's also the one that generalizes to trigger #3 above:
`PATCH`'s plain `DEL` and a TTL lapsing look identical from `resolve`'s
point of view — both are just "the key isn't there anymore" — so the same
lease defends against both without knowing which one happened.

Verified two ways beyond the test suite: a simulated crashed leader (a
short-lived lease manually set with nothing behind it) correctly makes a
fresh request wait out the expiry and take over cleanly; 30 real concurrent
requests against a cold key all resolve correctly with no errors or hangs.

---

## What breaks here

| Problem | Status |
|---|---|
| Still one Postgres `nextval()` sequence and one Redis instance — both single points of failure, and the counter is a bottleneck once there's more than one app server. | Open problem — the class ends at Tier 4.4 |
| The lease only protects a single Redis instance's view of a key. Replicating Redis for availability would mean a lease acquired against a primary that hasn't yet propagated to a replica is a new race. | Open problem |

---

## Discussion questions

1. The lease loop's retry interval is a flat 50ms. What would happen with a
   much longer interval? A much shorter one? Think in terms of: how quickly
   the real leader typically finishes, versus how many wasted lease-
   acquisition attempts pile up against Redis while everyone waits.
2. `PATCH` and `DELETE` both invalidate with a plain `DEL`. Walk through what
   would need to change if a `PATCH` under heavy concurrent read load should
   *not* cause a visible stampede at all — i.e., the write-through
   alternative this quarter deliberately didn't take.
3. The lease key and the real cache key are separate Redis keys
   (`lease:{code}` vs `{code}`). What would go wrong if they were the same
   key — e.g., storing a sentinel value in `{code}` itself to mean "someone
   is rebuilding this"?
4. `resolve`'s loop has no maximum iteration count — it retries until the
   key resolves, however long that takes. What's the argument for that being
   correct, and what's the argument for it being an operational risk worth
   bounding anyway?
5. LFU counters decay over time in Redis (so a key that *was* hot but no
   longer is eventually looks cold again). Why does that matter for a
   caching workload, and what would happen under `allkeys-lru` instead if a
   link goes viral for an hour and then is never touched again?

**Previous:** [Tier 4.2 — Bloom Filter for the Negative Case](../tier-4.2-caching/README.md) ·
**Next:** [Tier 4.4 — Singleflight, TTL Keep-Alive, TTL Jitter](../tier-4.4-caching/README.md)
