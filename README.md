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

## Fourth iteration — caching with Redis ⭐

The centerpiece. Budget the most time here.

**Cache-aside:** `GET` from Redis; on a miss, read Postgres and `SET` with a TTL.
Redis is presented as the first iteration's hash table moved onto the network and
shared between processes.

**Primitives:** `SET`/`GET`/`EXPIRE` for the mapping; `INCR` for atomic counters;
HyperLogLog (`PFADD`/`PFCOUNT`) for unique-click estimates in constant memory;
Bloom filters (`BF.*`) for cheap existence checks before hitting the database.

**Eviction:** LRU, LFU, FIFO, TTL expiry, and why Zipfian access — a few URLs
taking most of the traffic — makes caching so effective here.

**Invalidation:** once `PATCH` and `DELETE` exist, immediate `DEL` (simple, but
stampedes on a hot key) versus write-through `SET` (no stampede, different
partial-failure behaviour).

### Thundering herd

Three triggers, each reproducible:

1. **TTL expiry** — a hot key's TTL lapses under sustained load; every in-flight
   request misses at once and queries the same row.
2. **Eviction under memory pressure** — with `maxmemory` and a FIFO policy,
   inserting N+1 keys into a cache sized for N evicts the hottest key. Motivates
   LRU/LFU as defaults.
3. **Explicit invalidation** — a `PATCH` deletes the key concurrent readers are
   requesting. Deterministic and instructor-triggerable with one `curl`. Works
   only under Path A: a hash-derived code changes on edit, so no shared hot key
   survives to be invalidated.

Five mitigations, in increasing sophistication: a distributed lock
(`SET key val NX EX ttl`); in-process request coalescing (singleflight);
stale-while-revalidate; probabilistic early expiration (XFetch); and TTL jitter
for the distinct case of many keys expiring in unison.

Ships with a sequence diagram of the failure and a before/after load test
measuring database query volume.

**Breaks:** still one counter in one process.

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
budget. Rate limiting in Redis via a sliding-window `ZSET` or a token bucket. A
Bloom filter rejecting known-malicious URLs on the write path without an external
call in the hot path.

---

## Repository layout

```
Cargo.toml              workspace root — lists every iteration as a member
rust-toolchain.toml     pins the compiler
tier-0-requirements/    README only
tier-N-.../
  README.md             problem, what changed, trade-offs, discussion questions
  src/                  self-contained implementation
  demo/ | loadtest/     scripts reproducing the failure and verifying the fix
```

Iterations do not depend on each other. Each is a full copy of the previous one
plus the new idea, so a folder reads standalone and
`diff -ru tier-3-persistence tier-4-caching` shows exactly what an iteration
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

Source ships as **skeletons**: types, signatures, tests and step-by-step comments
are given, function bodies are `todo!()`. `cargo test -p tier-N-...` starts red
and goes green as they are filled in.

## Status

- [x] Requirements
- [x] First iteration — naive in-memory (skeleton + tests)
- [ ] Second iteration — code generation and collisions
- [ ] Third iteration — persistence
- [ ] Fourth iteration — caching and the thundering herd
- [ ] Fifth iteration — distributed ID generation
- [ ] Sixth iteration — availability and partitioning
- [ ] Seventh iteration — analytics and abuse prevention
