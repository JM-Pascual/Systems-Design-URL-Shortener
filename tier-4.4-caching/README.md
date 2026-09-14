# Tier 4.4 — Caching with Redis: Singleflight, XFetch, TTL Jitter

A copy of [Tier 4.3](../tier-4.3-caching/) plus three more mitigations from
the root README's "five mitigations, in increasing sophistication" list.
The lease (Tier 4.3) was the first and simplest; stale-while-revalidate was
deliberately skipped. These three layer on top of the lease rather than
replacing it.

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

## XFetch: recomputing before the miss ever happens

Every hit now also pulls the key's remaining TTL (one pipelined
`GET`+`TTL`, not two round trips) and runs a probabilistic check: the
closer that TTL is to zero, the higher the chance this particular read
triggers a *background* refresh — via the same singleflight/lease path —
while still returning the current, still-valid value immediately. The
reader never waits on it.

The formula (from the original paper, "Optimal Probabilistic Cache
Stampede Prevention"): recompute now if `delta * beta * -ln(rand()) >=
remaining_ttl`, where `delta` is an estimate of how long a recompute takes
and `beta` is a tuning constant (`1.0` here, the paper's default). `rand()`
is uniform on `(0, 1)`, so `-ln(rand())` is exponentially distributed —
this is what makes the *chance* of triggering climb sharply as
`remaining_ttl` shrinks, without ever being deterministic. `delta` uses
`DEMO_QUERY_DELAY_MS` when set (so a slower simulated backend makes XFetch
trigger earlier — a demonstrable, correct property) and a small floor
otherwise, since a real deployment would measure actual recompute latency
rather than hardcode it.

Verified live, with the formula's actual computed values logged temporarily:
against a 15-second TTL and a 2-second simulated recompute cost, a read at
5 seconds remaining rolled a low enough random draw to trigger — and in a
separate run, the trigger fired close enough to the real expiry that its
own 2-second recompute lost the race against the real TTL lapsing, at which
point the existing lease/singleflight safety net caught the resulting real
miss and repopulated correctly, with **no duplicate Postgres query and no
wrong response at any point**. That losing race is expected, not a bug —
XFetch is a probabilistic reduction in how often a hard miss happens, not a
guarantee it never does; recomputing "just in time" inherently means
sometimes losing to the clock by a little.

---

## TTL jitter: spreading out *mass* expiry

Every `SET ... EX` now uses `CACHE_TTL_SECONDS + random(0, 30)` instead of a
flat 300. This defends against a different stampede shape than the lease or
XFetch: not one hot key losing its cache entry, but *many different* keys
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
| Stale-while-revalidate remains deliberately unbuilt — a hit is still a hit-or-miss, XFetch aside; there's no "serve stale while a refresh runs in the background *because the value is already gone*" path. | Not planned |

---

## Discussion questions

1. `inflight`'s `HashMap` is guarded by a `std::sync::Mutex`, not a
   `tokio::sync::Mutex`. The lock is only ever held for a synchronous
   `entry(...).or_insert_with(...).clone()` — never across an `.await`.
   Why is a `std` mutex the *correct* choice here, not just a permissible
   one?
2. XFetch's `delta` estimate is currently either `DEMO_QUERY_DELAY_MS` or a
   fixed floor. Sketch how you'd measure it for real: what would you
   average, over what window, and what happens to the formula if `delta` is
   badly underestimated versus badly overestimated?
3. Singleflight's `OnceCell` is removed from the map immediately after it
   resolves. What would go wrong — concretely, not just "it'd be less
   efficient" — if it were left in the map permanently instead?
4. TTL jitter changes *when* a key expires but nothing about *how* it's
   invalidated. Does jitter interact at all with `PATCH`/`DELETE`'s plain
   `DEL`, or are they addressing two completely independent trigger paths?
5. XFetch and the lease can now both be "in flight" for the same code at
   once (a background XFetch refresh racing a foreground lease-guarded
   miss). Walk through why that specific overlap is safe here, tracing
   which mechanism actually prevents a duplicate Postgres query.

**Previous:** [Tier 4.3 — Invalidation and a Lease](../tier-4.3-caching/README.md)

This is the last chapter. The class concludes here — see the root
[README](../README.md#status) and this file's "What breaks here" section
for what's deliberately left as an open problem rather than a next tier.
