# URL Shortener — A Systems Design Class

One system, built eight times. Each iteration is broken in a specific,
demonstrable way; the next one exists to fix it.

**Language:** Rust (edition 2024, toolchain pinned in `rust-toolchain.toml`).
**Infrastructure:** Docker Compose, from the third iteration onward.

---

## Problem Statement

> "Design a URL shortener, like bit.ly or tinyurl."

A user submits a long URL and receives a short code. Anyone who visits that code
is redirected to the original URL. The code must be short enough to paste into a
tweet or read aloud, and must resolve to exactly one destination.

The difficulty is not in the two operations. It is in the conditions they run
under:

| Constraint | Value | Consequence |
|---|---|---|
| Read/write ratio | ~100:1 | Optimising writes touches 1% of traffic. Every decision favours reads. |
| Write volume | ~40/sec (100M/month) | Trivial for one database. |
| Read volume | ~4 000/sec | Painful against a disk-backed index. This gap is the course. |
| Storage growth | ~50 GB/month | Exceeds one machine within a few years. |
| Redirect latency | < 10 ms server-side | Nothing synchronous in the redirect path: no logging, no blocklist calls. |
| Consistency | Availability over consistency | A stale redirect target is acceptable. A dead link is not. |
| Codes | Unique, ideally non-guessable | Uniqueness is non-negotiable; sequential codes leak the whole database. |
| Key space | 7 chars base62 ≈ 3.5 × 10¹² | ~2 800 years of runway at 40 writes/sec. |

Full derivation of these numbers, the CAP argument, and the discussion questions:
**[tier-0-requirements/](tier-0-requirements/)**.

---

## Functional Requirements

Two core operations. Everything the system does reduces to these.

| Operation | Signature | HTTP |
|---|---|---|
| Shorten | `shorten(long_url) -> code` | `POST /shorten` → `201 {"code", "short_url"}` |
| Redirect | `redirect(code) -> long_url` | `GET /{code}` → `307 Location: <long_url>` |

```
POST /shorten   {"url": "https://example.com/a/very/long/path"}
             -> 201 {"code": "1a", "short_url": "http://localhost:3000/1a"}

GET  /1a     -> 307 Location: https://example.com/a/very/long/path
```

Deferred, but designed for — each one forces a decision in a later iteration:

| Operation | Forces |
|---|---|
| `shorten(url, alias="my-link")` | A uniqueness conflict the counter scheme otherwise makes impossible. |
| `shorten(url, expires_at)` | A TTL column, which maps onto Redis TTLs. |
| `PATCH /{code}` — edit destination | Cache invalidation and the thundering herd. Also why codes come from a counter, not a hash. |
| `DELETE /{code}` | What a cache *hit* means once the row is gone. |
| Click analytics | HyperLogLog; the asynchronous pipeline. |

Out of scope: accounts and authentication, a web UI, billing.

---

## First iteration — [naive, in-memory](tier-1-naive/)

The baseline. One process, one `HashMap<code, url>` in RAM, one `u64` counter
encoded in base62. Serves both endpoints over HTTP.

Covers hash table internals — buckets, hash function, load factor, resizing, and
why "amortized O(1)" is the honest description — because the fourth iteration
replaces this exact structure with Redis. Also establishes base62 as an
*encoding*, not a hash: reversible, collision-free, and independent of the URL's
content.

**Breaks:** every link is lost on restart; a second app server has its own
counter and issues duplicate codes; bounded by RAM; codes are enumerable.

---

## Second iteration — [code generation and collisions](tier-2-collisions/)

Two competing schemes, compared directly.

**Path A — counter + base62.** Uniqueness guaranteed by construction. The code
is independent of the URL's content, so a destination can be edited without the
code changing. The class carries this path forward.

**Path B — truncated hash of the URL.** Codes derived from content, so real
collisions occur. Connects to textbook collision resolution: chaining (and why
it does not fit — a code must resolve to one URL), open addressing via
`hash(url + salt)` retries, and a birthday-paradox estimate against the 3.5 ×
10¹² key space. Editing a URL necessarily changes its code.

Also: sequential codes are enumerable, mitigated by a reversible permutation
(coprime multiplication, XOR mask, or a small Feistel network) applied before
encoding — preserving uniqueness while destroying the ordering.

**Breaks:** a single shared counter is a bottleneck once there are multiple app
servers (deferred to the fifth iteration).

---

## Third iteration — [persistence](tier-3-persistence/)

Moves the map into Postgres. Schema: `code (PK), long_url, created_at,
expires_at, user_id`.

Contrasts the B-tree backing the primary key — O(log n), disk-backed — against
the in-memory hash map's O(1). Restarts stop losing data.

**Breaks:** every redirect, which is 99% of traffic, now costs a disk-backed
query.

---

## Fourth iteration — [caching with Redis](tier-4.1-caching/) ⭐

The centerpiece. Unlike every other tier, it's split into quarters, each its
own copy-forward folder — `diff -ru tier-4.1-caching tier-4.2-caching` shows
exactly what each one adds, the same discipline as the tiers themselves.

- **[4.1 — cache-aside](tier-4.1-caching/):** `GET` from Redis; on a miss,
  read Postgres and `SET` with a TTL. Redis is presented as the first
  iteration's hash table moved onto the network and shared between
  processes. `shorten` writes through, so a code's first redirect is
  already a cache hit.
- **[4.2 — a Bloom filter](tier-4.2-caching/):** every minted code goes into
  a RedisBloom filter; `resolve` checks it on a cache miss, before Postgres,
  so a code that never existed stops costing a database query on every
  single request.
- **[4.3 — invalidation and a lease](tier-4.3-caching/):** `PATCH`/`DELETE`
  finally implemented, invalidating with a plain `DEL` — deliberately the
  simple, stampede-prone option, since it's the concrete trigger the lease
  exists to defend against. Redis runs with `allkeys-lfu` eviction.
  `resolve`'s miss path runs a real lease (`SET lease:{code} 1 NX EX ttl`):
  only one request rebuilds a hot key at a time, and if that request
  crashes, leadership transfers to the next waiter the instant the lease
  expires — no request ever queries Postgres unguarded, and none can be
  blocked forever.

**Deliberately not built:** the more sophisticated herd mitigations
(request coalescing beyond the lease, stale-while-revalidate, probabilistic
early expiration, TTL jitter) and a formal load test / sequence diagram of
the failure remain discussion material — see each folder's README and
`QUESTIONS.md` — rather than code. The lease alone is enough to demonstrate
and defend against the core failure.

**Breaks:** still one Postgres sequence and one Redis instance, both single
points of failure, and the counter is a bottleneck once there's more than
one app server. The lease itself only coordinates a single Redis instance —
Tier 6's replication reopens a version of the same race.

---

## Fifth iteration — distributed ID generation

Four ways to remove the single-counter bottleneck, with their trade-offs:

- **Redis `INCR`** — centralised, atomic, reuses infrastructure already present.
- **Database auto-increment** — works, but makes the database a write bottleneck
  and a single point of failure for ID generation.
- **Pre-allocated ranges (ticket server)** — each instance leases a block, e.g.
  `[5000, 6000)`, and hands out IDs locally.
- **Snowflake** — timestamp + worker ID + local sequence, no coordination.

**Breaks:** one Redis and one Postgres are both single points of failure.

---

## Sixth iteration — availability and partitioning

Redis primary-replica replication and the consistency it gives up. Consistent
hashing, motivated by first showing that naive `hash(code) % N` reshards
everything when a node joins or leaves. Database sharding by code range versus by
hash.

---

## Seventh iteration — analytics and abuse prevention *(optional)*

Click analytics pushed onto a queue so logging stays outside the redirect latency
budget: `INCR` for a raw per-code hit counter, HyperLogLog (`PFADD`/`PFCOUNT`) for
unique-visitor estimates in constant memory. Rate limiting in Redis via a
sliding-window `ZSET` or a token bucket. A Bloom filter rejecting known-malicious
URLs on the write path without an external call in the hot path.

---

## Repository layout

```
Cargo.toml              workspace root — lists every implemented iteration as a member
rust-toolchain.toml     pins the compiler
QUESTIONS.md            open discussion questions raised while actually building this
tier-0-requirements/    README only
tier-2-collisions/      README only — the design is worked out, no code yet
tier-N-.../
  README.md             problem, what changed, trade-offs, discussion questions
  src/                  self-contained implementation
  docker-compose.yml    local Postgres/Redis, from the third iteration on
```

Not every tier has code — some (Tier 0, Tier 2 so far) are design-only
READMEs, and that's a deliberate stopping point, not an oversight.

Iterations do not depend on each other. Each is a full copy of the previous one
plus the new idea, so a folder reads standalone and
`diff -ru tier-3-persistence tier-4.1-caching` shows exactly what an iteration
introduced.

## Running the code

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # one-time
source "$HOME/.cargo/env"

cargo build                       # every iteration
cargo test  -p tier-1-naive       # one iteration's tests
cargo run   -p tier-1-naive       # its server, on :3000
cargo clippy --workspace
cargo fmt --all
```

Short URLs are minted against `BASE_URL` (default `http://localhost:3000`). It
is the public origin users see, which behind a proxy differs from the address
the process binds to:

```bash
BASE_URL=https://sho.rt cargo run -p tier-1-naive
```

From the third iteration on, each folder ships a `docker-compose.yml` for its
Postgres and Redis; `docker compose up -d` inside the folder is enough.

Every implemented tier is complete and tested, not a skeleton to fill in —
`cargo test -p tier-N-...` is green against a live Postgres/Redis where the
tier needs one. Each tier's own README lists its discussion questions
whether or not there's code to go with it yet.

## Status

- [x] Requirements
- [x] First iteration — naive in-memory
- [x] Second iteration — code generation and collisions (design only, no code)
- [x] Third iteration — persistence
- [x] Fourth iteration, quarters 1–3 — cache-aside, Bloom filter, invalidation + lease
- [ ] Fourth iteration — remaining herd mitigations (singleflight, stale-while-revalidate, XFetch, TTL jitter)
- [ ] Fifth iteration — distributed ID generation
- [ ] Sixth iteration — availability and partitioning
- [ ] Seventh iteration — analytics and abuse prevention
