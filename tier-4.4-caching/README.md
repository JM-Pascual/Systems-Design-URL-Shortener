# Tier 4.4 — Caching with Redis: Singleflight, TTL Keep-Alive, TTL Jitter

A copy of [Tier 4.3](../tier-4.3-caching/) plus three more mitigations. The
lease (Tier 4.3) was the first and simplest; these three layer on top of it
rather than replacing it.

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

### Why extend the TTL instead of re-fetching

The obvious alternative is to use the same roll to trigger a *refresh* —
re-query Postgres in the background and re-`SET` the value before it
expires. That would cost a real query per trigger, and here it buys
nothing:

- **The cached value can't have drifted.** `PATCH`/`DELETE` invalidate
  with an explicit `DEL`, so for as long as a cached value lives it is
  identical to Postgres by construction. Re-querying can't return anything
  different from what's already cached.
- **What matters is *when*, not *what*.** The whole point is that the key
  never reaches a synchronized hard expiry. Moving the expiry delivers that
  completely; fetching the same bytes again on the way adds only cost.
- **A cheap recompute means a tiny stampede anyway.** A PK lookup against a
  warm local Postgres is ~1ms, so the window in which concurrent readers
  could pile up on a miss is ~1ms wide. A refresh-based scheme would have
  almost nothing to prevent — and would have to start in that last
  millisecond to do it.

**What makes this sound**, and what would break it: it relies entirely on
invalidation being explicit. If anything wrote Postgres *outside* this app
(a manual migration, another service), a kept-alive hot key would serve
the old value indefinitely — that's the case where a real re-fetch would
earn its cost. And it relies on `allkeys-lfu` to bound memory, since
kept-alive keys don't age out on their own.

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

---

## What breaks here

| Problem | Status |
|---|---|
| Still one Postgres sequence and one Redis instance — both single points of failure, and the counter is a bottleneck once there's more than one app server. | Open problem |
| Singleflight only coordinates within one process; a fleet of app instances would still rely entirely on the lease to coordinate with each other. | Open problem |
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
6. The keep-alive extends rather than re-fetches because invalidation is
   explicit — the cache is told about every write. Name a system where
   that doesn't hold, where the cache *can't* be told about every write,
   and explain why the keep-alive would be actively wrong there and a
   background re-fetch would be the right call.

**Previous:** [Tier 4.3 — Invalidation and a Lease](../tier-4.3-caching/README.md)
