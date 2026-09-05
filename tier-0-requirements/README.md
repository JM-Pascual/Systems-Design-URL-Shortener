# Tier 0 — Requirements Gathering
---

## What this session is for

By the end of it you should be able to:

1. Turn a one-sentence problem statement into a set of operations with types.
2. Separate **functional** requirements (what it does) from **non-functional**
   ones (how well, under what load, with what failure behaviour).
3. Do a back-of-the-envelope estimate and read a design decision off it.
4. State, in one sentence, what the system is allowed to get *wrong* — and see
   why that sentence is the most useful one in the document.

That last point is the one students usually find surprising, so we spend real
time on it in §3.3.

---

## 1. The problem statement

> "Design a URL shortener, like bit.ly or tinyurl."

That sentence is under-specified on purpose — real ones always are. A ticket, a
product brief, or a professor's prompt all arrive in roughly this shape, and the
first engineering act is to turn it into a contract. We do that in two passes:
functional, then non-functional.

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

Two operations. That is the whole product. Hold on to how small this list is —
by Tier 6 we will have replication, sharding, and a cache hierarchy behind
exactly these two calls, and it is worth being able to point back at the
smallness.

### Optional (deferred, but designed for)

We are not building these now. We write them down anyway, because a requirement
you know is coming changes the design you choose today.

| Operation | What it forces later |
|---|---|
| Custom alias — `shorten(url, alias="my-link")` | Introduces a *uniqueness conflict*, which the counter scheme of Tier 2 otherwise makes impossible. |
| Expiration — `shorten(url, expires_at)` | Motivates a TTL column in the database (Tier 3), which maps naturally onto Redis TTLs (Tier 4). |
| Edit destination — `PATCH /{code}` | **The important one.** It is the trigger for the cache-invalidation and thundering-herd work in Tier 4, and the reason we pick counter-based codes over hash-based ones in Tier 2. |
| Delete / disable — `DELETE /{code}` | Same, plus it raises a genuinely hard question: what does a cache *hit* mean when the underlying row is gone? |
| Click analytics — unique visitor counts | Motivates HyperLogLog (Tier 4) and the asynchronous pipeline (Tier 7). |

### Explicitly out of scope

User accounts and authentication, a web UI, billing.

Writing down what we are *not* building is not padding. It is how a scope stays
fixed for the ten weeks of the course, and it is the difference between a
decision and an omission.

---

## 3. Non-functional requirements

This is where all the design pressure in the course comes from. Each subsection
below is cashed out in a specific later tier.

### 3.1 The workload is read-heavy (~100:1)

For every URL created, expect roughly **100 redirects**. This is the single most
important number in the design.

What follows from it:

- Optimising the write path improves ~1% of traffic. Optimising the read path
  improves ~99%. **Every architectural choice in this course favours reads.**
- Caching is unusually effective here: a read-heavy, mostly-immutable dataset is
  close to the ideal case (Tier 4).
- We can afford an expensive write — a database round-trip, a uniqueness check,
  a malicious-URL scan — if it buys us a cheaper read.

**Back-of-the-envelope.** Say 100M new URLs per month:

```
writes:  100e6 / (30 · 24 · 3600 s)      ≈ 40 writes/sec
reads:   40 · 100                        ≈ 4 000 reads/sec
storage: 100e6 rows/month · ~500 B/row   ≈ 50 GB/month
```

Read those three numbers as a verdict: 40 writes/sec is nothing — one Postgres
handles it without noticing. 4 000 reads/sec against a disk-backed B-tree is
where it starts to hurt. **That gap is the course.** Tiers 3 through 6 are all
attempts to close it.

*In class:* do this estimate on the board before showing the numbers. Students
should practise picking a plausible input (100M/month) and defending it, rather
than deriving one true answer.

### 3.2 Redirect latency is in a human's critical path

A redirect sits between someone clicking a link and the page loading. Budget:
the redirect should add **under 10 ms** server-side.

The useful consequence is a rule about what the redirect may *not* do. Anything
synchronous — logging the click, checking a blocklist, writing analytics —
spends that budget. That rule is what forces analytics onto a queue in Tier 7.

### 3.3 We choose availability over strong consistency

In CAP terms: when the network partitions, we keep serving.

- **A slightly stale redirect target is acceptable.** If someone edits a link and
  a replica serves the old destination for thirty seconds, nothing is materially
  harmed.
- **Refusing to serve a redirect is not acceptable.** A dead short link breaks
  every place it was ever pasted — emails, printed posters, other people's blog
  posts. We do not control any of them.

That first bullet is the licence for everything in Tier 4, up to and including
deliberately serving expired data while a refresh runs in the background
(*stale-while-revalidate*). Without it, half of Tier 4 would be indefensible.

*Worth doing in class:* take the same two bullets and rewrite them for a bank
ledger, where the answers invert. Then ask which parts of Tiers 4 and 6 survive
the rewrite. This is usually the moment CAP stops being a slogan.

### 3.4 Codes must be unique, and ideally not guessable

- **Unique** is non-negotiable: one code resolves to exactly one URL. Tier 2
  compares two ways of getting there — *guaranteeing* uniqueness with a counter,
  versus *probabilistically avoiding* collisions with a hash.
- **Non-guessable** is a soft requirement. Sequential codes (`1`, `2`, `3`, …)
  let anyone walk the entire database, which leaks every "unlisted" link in the
  system. Tier 2 shows cheap mitigations that keep the counter's uniqueness
  guarantee intact.

### 3.5 Key space and code length

Codes should be short enough to type and paste, and numerous enough that we
never run out.

With a base62 alphabet (`0-9`, `a-z`, `A-Z`):

| Length | Distinct codes | Runway at 40 writes/sec |
|---|---|---|
| 5 | 62⁵ ≈ 9.2 × 10⁸ | ~9 months |
| 6 | 62⁶ ≈ 5.7 × 10¹⁰ | ~45 years |
| **7** | **62⁷ ≈ 3.5 × 10¹²** | **~2 800 years** |
| 8 | 62⁸ ≈ 2.2 × 10¹⁴ | far beyond need |

We take **7 characters**. Note how the choice falls out of §3.1's write rate
rather than out of taste — and keep the number, because Tier 2 runs a birthday
paradox calculation against exactly this key space.

---

## 4. Discussion questions

Work through these *before* reading §5. Several have no single right answer;
the goal is to make the trade-off explicit, not to land on the same conclusion.

1. Should `shorten()` be idempotent — should shortening the *same* URL twice
   return the *same* code? What breaks if it does? *(Consider per-link
   analytics, and what index the write path would now need.)*
2. Two users request the custom alias `promo` at the same instant. Which
   component is responsible for rejecting one of them, and what does it need in
   order to do that correctly?
3. The read/write ratio is 100:1. Name three design decisions that would change
   if it were 1:100.
4. Is it acceptable for a redirect to return a URL that was edited 5 seconds
   ago? 5 minutes ago? Where do you draw the line — and what mechanism actually
   enforces the line you drew?
5. Estimate storage after 5 years at 100M writes/month. Does it fit on one
   machine? *(Keep your answer; it is the opening question of Tier 6.)*

---

## 5. What this document buys us

Every later tier traces back to a line above. When a tier introduces a piece of
technology, the honest justification is always a requirement, never the
technology's own merits.

| Requirement | Cashed out in |
|---|---|
| Unique, non-guessable codes (§3.4) | Tier 2 — counter + base62 vs. hashing |
| Durability across restarts (§2) | Tier 3 — Postgres |
| 100:1 reads, <10 ms latency (§3.1, §3.2) | Tier 4 — Redis cache-aside |
| Editable destinations (§2, optional) | Tier 4 — invalidation, thundering herd |
| More than one app server (§3.1) | Tier 5 — distributed ID generation |
| Availability over consistency (§3.3) | Tier 6 — replication, partitioning |
| Redirect latency budget (§3.2) | Tier 7 — async analytics, rate limiting |

If at any point in the course you cannot name the row of this table that a piece
of infrastructure is paying for, that is a sign to stop and go back to it.

**Next:** [Tier 1 — Naive Single-Server Solution](../tier-1-naive/README.md)
