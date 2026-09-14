# Tier 4.4 — Caching with Redis: Singleflight, TTL Keep-Alive, TTL Jitter

A copy of [Tier 4.3](../tier-4.3-caching/) plus three more mitigations. The
lease (Tier 4.3) was the first and simplest; stale-while-revalidate was
deliberately skipped; and XFetch — the textbook "probabilistic early
recompute" — was built, found not to fit this system, and replaced by the
simpler thing it reduces to here (see the keep-alive section for the full
reasoning). These three layer on top of the lease rather than replacing it.

---

## Singleflight: coalescing within one process

The lease still costs every *loser* a Redis round trip just to discover
they lost. Within a single process, that's pure waste: if 20 concurrent
requests miss the same code, they're all running on the same instance's
`Store`, so there's no reason more than one of them should even talk to
Redis's lease.

`resolve_miss_coalesced` fixes that with a `tokio::sync::OnceCell` per
in-flight code (`inflight: Arc<Mutex<HashMap<String, Arc<OnceCell<...>>>>>`):
the first caller for a code creates the cell and starts `resolve_miss`
(Tier 4.3's lease loop, unchanged); every other concurrent caller for that
*same* code finds the cell already there and just awaits its result
directly — no Redis command, no lease attempt, nothing. The cell is removed
once it resolves, so the *next* stampede on that code (later, after this one
settles) starts fresh rather than being stuck with a `OnceCell` that can
only ever initialize once.

This only coordinates within one process. A fleet of multiple app instances
would each have their own `inflight` map — the lease is still what would
coordinate *between* them. Singleflight and the lease are complementary,
not alternatives: singleflight handles the common case cheaply, the lease
handles the case singleflight can't see.

Verified with an actual test
(`store::tests::concurrent_misses_singleflight_to_one_query`): 20 concurrent
`resolve` calls for a freshly-cleared code produce exactly 1 Postgres query,
not 20.

---

## TTL keep-alive: hot keys never reach a hard expiry

Every hit also pulls the key's remaining TTL (one pipelined, atomic
`GET`+`TTL`, not two round trips) and rolls: the closer that TTL is to
zero, the higher the chance this particular read pushes the expiry out —
a single inline `EXPIRE code 30`, no Postgres, nothing to background.

The roll: extend if `window * -ln(rand()) >= remaining_ttl`, with
`window = 5s`. `rand()` is uniform on `(0, 1)`, so `-ln(rand())` is
exponentially distributed with mean 1, which makes the per-read chance
exactly `e^(-remaining / window)`: effectively zero five-plus windows out,
~37% at one window out, near-certain in the last second.

Two design choices worth understanding:

- **The randomness is a hotness filter, not cost-spreading.** `EXPIRE` is
  free and idempotent, so there'd be no harm in every reader doing it —
  the point of the low per-read chance is that a key needs *many* reads
  inside the window for one to land. A hot key collects those rolls and
  never expires; a key read twice a minute sails past its window and
  expires normally. The extension has to be earned with sustained traffic.
- **The extension is small (30s), not a full 300s reset.** One lucky read
  buys a little more life, not a whole new lifetime, so a key's survival
  tracks its recent traffic closely: a hot key hovers in 30-second
  increments near its expiry. The flip side is that a hot key whose
  traffic pauses for more than 30s expires, and its next burst is a miss —
  a lease-guarded, single-query miss, so not a stampede, but a miss.

### Why this and not XFetch

This started as XFetch — the paper's "Optimal Probabilistic Cache Stampede
Prevention": on a hit near expiry, probabilistically kick off a *real
recompute* (the Postgres query) in the background, weighted by how long
that recompute takes. It was built, and dropped, for three reasons that
are worth more as class material than the technique itself:

1. **It didn't work — a real bug.** The background refresh went through
   `resolve_miss`, whose first line is "if the key is in Redis, return it."
   XFetch fires *while the key is still alive* by definition, so every
   refresh found the key, returned, and touched neither Postgres nor the
   TTL. The key expired on schedule anyway. A live test showed exactly this
   (`postgres_queries` stayed at 0 after a confirmed trigger, then the key
   expired and a normal miss repopulated it) and was initially misread as
   "the refresh lost the race against expiry." It hadn't raced anything.
   No test exercised the path, which is why it went unnoticed — this tier
   now has two that do.
2. **The recompute buys nothing here.** `PATCH`/`DELETE` invalidate with an
   explicit `DEL`, so for as long as a cached value lives it is identical to
   Postgres by construction. Re-querying can't return anything different
   from what's already cached. The entire benefit of XFetch is *when* the
   refresh happens (before the synchronized miss), not *what* it fetches —
   and that benefit is fully delivered by just moving the expiry.
3. **XFetch is built for expensive recomputes.** Its `delta` (recompute
   latency) sets how far before expiry to start. A PK lookup against a warm
   local Postgres is ~1ms, so honest XFetch would only ever fire in the
   last millisecond — the miss window for a cheap recompute is tiny, and so
   is the stampede it prevents. It only visibly "worked" when
   `DEMO_QUERY_DELAY_MS` inflated `delta` artificially.

Strip the recompute out of XFetch and what's left is exactly the keep-alive
above: the same exponential roll, but `EXPIRE` instead of a query, and
`window` as an honest tuning knob instead of a latency estimate that was
never really measured.

**What makes this sound**, and what would break it: it relies entirely on
invalidation being explicit. If anything wrote Postgres *outside* this app
(a manual migration, another service), a kept-alive hot key would serve
the old value indefinitely — that's the case where XFetch's real recompute
would earn its cost. And it relies on `allkeys-lfu` to bound memory, since
kept-alive keys don't age out on their own.

Verified with two tests: `sustained_reads_near_expiry_extend_ttl` (50 reads
at 3s remaining leave the TTL at ~30s and `postgres_queries` unchanged) and
`reads_far_from_expiry_do_not_extend_ttl` (50 reads at ~300s remaining
leave the TTL untouched).

---

## TTL jitter: spreading out *mass* expiry

Every `SET ... EX` now uses `CACHE_TTL_SECONDS + random(0, 30)` instead of a
flat 300. This defends against a different stampede shape than the lease or
the keep-alive: not one hot key losing its cache entry, but *many different* keys
that happened to be written around the same time (e.g., a burst of
`shorten` calls) all expiring in near-unison later, each independently
triggering its own (small) miss at the same moment. Spreading their
expiries over a 30-second window turns one synchronized event into many
small, staggered ones.

Verified: five codes minted back-to-back came back with TTLs of 303, 311,
322, 325, and 314 — all in range, genuinely different.

---

## What breaks here

The class ends here — everything below is left as an open problem, not an
upcoming chapter.

| Problem | Status |
|---|---|
| Still one Postgres sequence and one Redis instance — both single points of failure, and the counter is a bottleneck once there's more than one app server. | Open problem |
| Singleflight only coordinates within one process; a fleet of app instances would still rely entirely on the lease to coordinate with each other. | Open problem |
| Stale-while-revalidate remains deliberately unbuilt — there's no "serve the old value while a refresh runs in the background *because the value is already gone*" path. The keep-alive prevents the value from going away in the first place, which is a different thing. | Not planned |
| The keep-alive assumes every write goes through this app's `PATCH`/`DELETE`. Anything writing Postgres directly would leave hot keys serving stale data indefinitely. | Open problem |

---

## Discussion questions

1. `inflight`'s `HashMap` is guarded by a `std::sync::Mutex`, not a
   `tokio::sync::Mutex`. The lock is only ever held for a synchronous
   `entry(...).or_insert_with(...).clone()` — never across an `.await`.
   Why is a `std` mutex the *correct* choice here, not just a permissible
   one?
2. The keep-alive extends by a fixed 30s. What changes if you make it 300s
   (a full reset on every lucky read)? What if you make it 5s? Think about
   two things separately: how tightly a key's survival tracks its traffic,
   and how many `EXPIRE` commands a hot key generates per minute.
3. Singleflight's `OnceCell` is removed from the map immediately after it
   resolves. What would go wrong — concretely, not just "it'd be less
   efficient" — if it were left in the map permanently instead?
4. TTL jitter changes *when* a key expires but nothing about *how* it's
   invalidated. Does jitter interact at all with `PATCH`/`DELETE`'s plain
   `DEL`, or are they addressing two completely independent trigger paths?
5. The keep-alive's per-read chance is `e^(-remaining / window)`. For a key
   read `r` times per second, what's the probability it survives past its
   expiry? Find the traffic rate below which a key is more likely to
   expire than not — that's the keep-alive's effective definition of
   "hot." Is it the definition you'd want?
6. XFetch was dropped because a real recompute buys nothing when
   invalidation is explicit. Name a system where the opposite holds — where
   the cache *can't* be told about every write — and explain why the
   keep-alive would be actively wrong there.

**Previous:** [Tier 4.3 — Invalidation and a Lease](../tier-4.3-caching/README.md)

This is the last chapter. The class concludes here — see the root
[README](../README.md#status) and this file's "What breaks here" section
for what's deliberately left as an open problem rather than a next tier.
