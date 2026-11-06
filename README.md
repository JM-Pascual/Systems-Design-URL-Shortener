# URL Shortener: A Systems Design Class

One system, built tier by tier in Rust. Each tier breaks in a specific,
demonstrable way, and the next one exists to fix it.

| Operation | HTTP |
|---|---|
| Shorten a URL | `POST /shorten {"url": "..."}` → `201 {"code", "short_url"}` |
| Follow a code | `GET /{code}` → `307 Location: <long_url>` |

The hard part is the load those two operations run under:

| Constraint | Value | Consequence |
|---|---|---|
| Read/write ratio | ~100:1 | Every decision favours reads. |
| Traffic | ~40 writes/s, ~4,000 reads/s | Writes are trivial; reads are the course. |
| Redirect latency | < 10 ms server-side | Nothing slow on the redirect path. |
| Consistency | Availability first | A stale redirect is acceptable; a dead link isn't. |
| Key space | 7 base62 chars ≈ 3.5 × 10¹² | ~2,800 years of runway. |

[Tier 0](tier-0-requirements/) derives these numbers.

## The tiers

| Tier | Adds | What breaks next |
|---|---|---|
| [0: Requirements](tier-0-requirements/) | Operations, estimates, CAP | Nothing yet: no code |
| [1: Naive](tier-1-naive/) | `HashMap` and a counter in one process | Restarts lose everything |
| [2: Code generation](tier-2-collisions/) | Counter vs. truncated hash (design only) | Sequential codes can be enumerated |
| [3: Persistence](tier-3-persistence/) | Postgres | Every redirect costs a query |
| [4.1: Cache-aside](tier-4.1-caching/) ⭐ | Redis in front of Postgres | Codes that don't exist always reach Postgres |
| [4.2: Bloom filter](tier-4.2-caching/) | A filter of every code, kept complete by a background job, with snapshots in a bucket | Cached links can't change, and hot keys stampede |
| [4.3: Invalidation and a lease](tier-4.3-caching/) | `PATCH`/`DELETE`, LFU eviction, one rebuild at a time | Waiters still poll Redis, and hot keys still expire |
| [4.4: Singleflight and TTLs](tier-4.4-caching/) | In-process coalescing, TTL keep-alive, TTL jitter | Open: one Redis and one Postgres are still single points of failure |

The [thundering herd demo](thundering-herd-demo/) compares 4.1 and 4.3 live in
Grafana. [QUESTIONS.md](QUESTIONS.md) collects open discussion questions.

Accounts, a web UI, billing, click analytics, and rate limiting are out of scope.

## Running it

Each tier is a self-contained copy of the previous one plus one new idea, so
`diff -ru tier-3-persistence tier-4.1-caching` shows exactly what a tier added.

```bash
cargo run -p tier-1-naive    # serves on :3000
```

From Tier 3 on, start the tier's services first. Tier 4.2 is the example here:

```bash
docker compose -f tier-4.2-caching/docker-compose.yml up -d
```

```bash
cargo run -p tier-4-2-caching
```

Unit tests run with `cargo test`. Tests that need those services are ignored by
default:

```bash
cargo test -p tier-4-2-caching -- --ignored --test-threads=1
```

Set `BASE_URL` to mint short URLs under a public origin other than
`http://localhost:3000`. The toolchain is pinned in `rust-toolchain.toml`.
