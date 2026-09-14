# Tier 3 — Persistence

**Problem being solved:** Tier 1's store lives entirely in one process's RAM.
`Ctrl-C` the server and every short link ever created is gone. This tier moves
the mapping into Postgres so a restart stops being a data-loss event.

---

## Schema

```sql
CREATE TABLE links (
    code        TEXT PRIMARY KEY,
    long_url    TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ,
    user_id     UUID
);
```

`expires_at` and `user_id` aren't used by anything yet — Tier 0 §2 flagged
both as deferred requirements (TTLs, and eventually accounts), and it's
cheaper to reserve the columns now than to migrate a populated table later.
Both stay nullable so this tier's actual writer only ever fills in `code` and
`long_url`.

`code` is the primary key, not a separate auto-increment id — the code
*is* the natural key here, exactly like Tier 1's `HashMap<code, url>` used it
as the map key. There's no reason to add a surrogate key on top of one that's
already unique, short, and immutable per Tier 2's Path A.

---

## The index that replaces the hash map

Tier 1's `store.rs` spent its whole preamble on how `HashMap` works — buckets,
a hash function, open addressing, amortized O(1). Declaring `code` a
`PRIMARY KEY` in Postgres builds a **B-tree** index on it, and that's a
genuinely different structure, not just "the same idea on disk":

| | `HashMap` (Tier 1) | B-tree PK index (Tier 3) |
|---|---|---|
| Lookup | O(1) average | O(log n) |
| Storage | in-process RAM | on-disk pages, cached by Postgres's buffer pool |
| Ordered iteration | no | yes — an artifact of the tree structure |
| Survives a restart | no | yes |

O(log n) is asymptotically worse than O(1), and that's not an accident of
Postgres being unoptimized — it's the honest cost of durability. A hash index
*does* exist in Postgres and is O(1)-ish for exact-match lookups, but the
primary key defaults to a B-tree because B-trees additionally support range
scans (`WHERE created_at > ...`, `ORDER BY`), are crash-safe via
write-ahead logging in a way hash indexes historically weren't, and their
sequential page layout matches how a disk actually wants to be read. Trading
a constant-factor lookup speed for those properties is the whole point of
this tier — restarts stop losing data specifically *because* the index isn't
just sitting in RAM.

---

## What structurally changes from Tier 1

- **The connection pool replaces `Mutex<Store>`.** Tier 1 serialized every
  request — reads included — behind one global lock, because `HashMap` has no
  concurrency story of its own. Postgres does: a connection pool (e.g.
  `sqlx::PgPool`) lets multiple requests run concurrently, and Postgres's own
  MVCC handles concurrent reads and writes without the app coordinating any
  of it.
- **`shorten` becomes an `INSERT`; `resolve` becomes a `SELECT`.** Both are now
  fallible over the network (connection drop, pool exhaustion, constraint
  violation) in a way an in-memory `HashMap::get` never was — this tier is
  also where error handling has to become real.
- **Local Postgres via `docker-compose.yml`**, per the root README's
  repository layout — every tier from here on ships one.

---

## What breaks here

| Problem | Fixed in |
|---|---|
| Every redirect — Tier 0's ~99% of traffic — now costs a disk-backed query instead of an in-memory lookup. | Tier 4 — Redis cache-aside |

That single line is the entire motivation for Tier 4. Tier 0 §3.2 budgeted
under 10ms server-side for a redirect; a B-tree lookup that has to hit disk
(not the buffer cache) can already threaten that budget on its own, before
network overhead to the database is even counted.

---

## Discussion questions

1. Tier 1's `resolve` cloned the `String` out of the `Mutex` specifically so
   the lock wasn't held during the HTTP response. What's the equivalent
   discipline with a connection pool — what should *not* happen while a
   connection is checked out?
2. `code TEXT PRIMARY KEY` versus a separate `id SERIAL PRIMARY KEY` with a
   `UNIQUE` index on `code` — what does the second buy you, and is it worth
   the extra index?
3. If `INSERT` fails after `Store::shorten`'s in-memory equivalent would have
   already advanced `next_id` (Tier 1, Option B from that exercise) — does
   that scenario still apply once the counter and the row are the same
   `INSERT`?
4. Redirect latency now includes a network round trip to Postgres. Sketch how
   you'd measure whether that round trip, not the B-tree lookup itself, is
   the dominant cost.
5. `expires_at` is nullable and unused by any query yet. What would the first
   query that *reads* it need to look like, and would it need an index of its
   own?

**Previous:** [Tier 2 — Code Generation and Collisions](../tier-2-collisions/README.md) ·
**Next:** [Tier 4 — Caching with Redis](../tier-4.1-caching/README.md)
