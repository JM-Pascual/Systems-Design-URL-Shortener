# Tier 0 — Requirements Gathering

> **No code in this tier.** The deliverable is a written specification.
> In a systems design interview, the candidate who starts drawing boxes before
> agreeing on requirements has already lost points. This tier is the habit we
> are trying to build.

---

## 1. The prompt

> "Design a URL shortener, like bit.ly or tinyurl."

That sentence is under-specified on purpose. Before designing anything, we have
to turn it into a contract. We do that in two passes: **functional** ("what must
it do?") and **non-functional** ("how well, and under what conditions?").

---

## 2. Functional requirements

These define the API surface. Everything the system does must be expressible in
terms of these operations.

### Core (must have)

| Operation | Signature | Description |
|---|---|---|
| Shorten | `shorten(long_url) -> code` | Accept a long URL, return a short code. |
| Redirect | `redirect(code) -> long_url` | Given a code, return the original URL (as an HTTP 301/302/307 — Tier 1 discusses which). |

Concretely, as HTTP:

```
POST /shorten      {"url": "https://example.com/a/very/long/path"}
                -> 201 {"code": "1a", "short_url": "http://localhost:3000/1a"}

GET  /1a        -> 307 Location: https://example.com/a/very/long/path
```

### Optional (explicitly deferred, but designed for)

| Operation | Why it matters later |
|---|---|
| Custom alias — `shorten(url, alias="my-link")` | Introduces a *uniqueness conflict* that the counter scheme (Tier 2) otherwise makes impossible. |
| Expiration — `shorten(url, expires_at)` | Motivates a TTL column in the DB (Tier 3), which maps naturally onto Redis TTLs (Tier 4). |
| Edit destination — `PATCH /{code}` | **Critical.** This is the trigger for the cache-invalidation and thundering-herd demo in Tier 4, and it is the reason we pick counter-based codes over hash-based ones in Tier 2. |
| Delete / disable — `DELETE /{code}` | Same as above, plus it raises "what does a cache hit mean when the row is gone?" |
| Click analytics — unique visitor counts | Motivates HyperLogLog (Tier 4) and the async pipeline (Tier 7). |

> **Out of scope, stated explicitly.** User accounts and authentication, a web
> UI, and billing. Naming what you are *not* building is part of the exercise;
> it prevents scope creep and shows the interviewer you are making choices
> rather than forgetting things.

---

## 3. Non-functional requirements

This is where the interesting design pressure comes from. Each one below maps
directly onto a later tier.

### 3.1 Read-heavy workload (~100:1)

For every URL that gets created, we expect roughly **100 redirects**. This is
the single most important number in the whole design.

Consequences:

- Optimising the write path buys us ~1% of the traffic. Optimising the read path
  buys us ~99%. **Every architectural decision should favour reads.**
- Caching is unusually effective here (Tier 4). A read-heavy, mostly-immutable
  dataset is the ideal caching workload.
- We can afford an expensive write (a DB round-trip, a uniqueness check, a
  malicious-URL scan) if it makes reads cheaper.

**Back-of-the-envelope.** Suppose 100M new URLs per month:

```
writes: 100e6 / (30 * 24 * 3600 s) ≈ 40 writes/sec
reads : 40 * 100                   ≈ 4 000 reads/sec
storage: 100e6 rows/month * ~500 bytes/row ≈ 50 GB/month
```

40 writes/sec is nothing — a single Postgres handles it. 4 000 reads/sec against
a disk-backed B-tree is where it starts to hurt. That gap *is* the class.

### 3.2 Low redirect latency

A redirect sits in the critical path of a human clicking a link. Budget: the
redirect should add **< 10 ms** server-side. Anything the redirect does
synchronously (logging a click, checking a blocklist, writing analytics) is
spending that budget — which is why Tier 7 pushes analytics onto a queue.

### 3.3 Availability over strong consistency (AP, not CP)

Framed in CAP terms: when the network partitions, we choose to keep serving.

- **Serving a slightly stale redirect target is acceptable.** If someone edits a
  link and a replica serves the old destination for 30 seconds, nobody is
  materially harmed. This single sentence is what licenses the entire caching
  strategy in Tier 4 (including *stale-while-revalidate*).
- **Refusing to serve a redirect is not acceptable.** A dead short link breaks
  every place it was ever pasted.
- Counter-example for contrast: a bank ledger would choose the opposite. Ask the
  class what changes in the design if we did.

### 3.4 Unique codes, ideally non-guessable

- **Unique** is non-negotiable: one code must resolve to exactly one URL. Tier 2
  compares two ways to get there — guaranteeing uniqueness (a counter) vs.
  probabilistically avoiding collisions (a hash).
- **Non-guessable** is a soft requirement. Sequential codes (`1`, `2`, `3`, …)
  let anyone enumerate every link in the system, which is a privacy leak for
  "unlisted" links. Tier 2 discusses cheap mitigations (a reversible permutation
  applied before encoding) that keep the counter's uniqueness guarantee.

### 3.5 Code length / key space

We want codes short (they are meant to be typed and pasted) but numerous enough
to never run out.

Using a base62 alphabet (`0-9`, `a-z`, `A-Z`):

| Length | Distinct codes | Enough for |
|---|---|---|
| 5 | 62^5 ≈ 9.2 × 10^8 | ~9 months at 40 writes/sec |
| 6 | 62^6 ≈ 5.7 × 10^10 | ~45 years |
| **7** | **62^7 ≈ 3.5 × 10^12** | **~2 800 years** |
| 8 | 62^8 ≈ 2.2 × 10^14 | far beyond need |

**7 characters** is the standard answer, and it is the number we will use in
Tier 2's birthday-paradox calculation.

---

## 4. Discussion questions

Use these before revealing anything above.

1. Should `shorten()` be idempotent — i.e. should shortening the *same* URL
   twice return the *same* code? What breaks if it does? (Hint: per-user
   analytics; and it forces a lookup-by-URL index on the write path.)
2. What happens if two users request the custom alias `promo` at the same
   instant? Which layer of the system is responsible for saying no?
3. The read/write ratio is 100:1. Name three design decisions that would change
   if it were 1:100 instead.
4. Is it acceptable for a redirect to return a URL that was edited 5 seconds
   ago? What about 5 minutes ago? Where exactly do you draw the line, and what
   mechanism enforces it?
5. Estimate the storage after 5 years at 100M writes/month. Does it fit on one
   machine? (This question is the seed for Tier 6's sharding discussion.)

---

## 5. What the requirements bought us

Every later tier traces back to a line in this document:

| Requirement | Cashed out in |
|---|---|
| Unique, non-guessable codes | Tier 2 — counter + base62 vs. hashing |
| Durability (survive a restart) | Tier 3 — Postgres |
| 100:1 reads, low latency | Tier 4 — Redis cache-aside |
| Editable destinations | Tier 4 — invalidation & thundering herd |
| Multiple app servers | Tier 5 — distributed ID generation |
| Availability over consistency | Tier 6 — replication, partitioning |
| Redirect latency budget | Tier 7 — async analytics, rate limiting |

**Next:** [Tier 1 — Naive Single-Server Solution](../tier-1-naive/README.md)
