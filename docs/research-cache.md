# Caching research notes

Personal notes collected while redesigning Tiers 4.1 to 4.3. Not part of the course.
They're grouped by topic, each going from historical practice to current practice.
Anything I couldn't verify is marked as such. Keeping the cache in sync with the database
after writes, including CDC, is in [research-cdc.md](research-cdc.md).

---

## 1. Where the cache sits

| Pattern | Who talks to the database | Notes |
|---|---|---|
| **Cache-aside (look-aside)** | The app, on a miss | Facebook's memcache design and the AWS default. A broken cache costs speed, not availability: the app can always ask the database. |
| **Read-through** | The cache itself, through a loader | Common in libraries such as Caffeine, NCache and Hazelcast. The cache becomes a dependency on the read path. |
| **Write-through** | The app writes both, on every write | Never stale while it works, but it fills the cache with data nobody reads, and a new node starts empty. |
| **Write-behind** | The cache writes to the database later | Fast writes, with a risk of losing them. Not for a source of truth. |

**AWS's guidance** ([ElastiCache caching strategies](https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/Strategies.html)):
- Lazy loading plus a TTL "keeps data from getting too stale", and gets most of the benefits
  of both lazy loading and write-through.
- Its examples use a TTL of 300 seconds. That's an example, not a rule.
- When a node fails and is replaced, lazy loading keeps working, only with more latency.

**Decision taken:** cache-aside from Tier 4.1 on.
- A failed Redis read counts as a miss and goes to Postgres.
- Writes go to Postgres first, then `DEL` the key.
- Deleting is better than updating the key: a delete can be repeated safely, and the next
  read fetches the current value.

---

## 2. The Facebook paper (NSDI 2013)

[*Scaling Memcache at Facebook*](https://www.usenix.org/system/files/conference/nsdi13/nsdi13-final170_update.pdf)

**Ideas we used or discussed:**

- **Look-aside, with deletes instead of updates.** Deletes are idempotent.
- **Leases** (§3.2.1). On a miss, memcached hands the client a 64-bit token. A set only
  succeeds if the token is still valid, and a delete invalidates it. This solves two things:
  - **Stale sets** (Race 2). A slow reader can't overwrite a newer invalidation.
  - **Thundering herds.** The server issues at most one token per key every 10 seconds.
    Other clients wait briefly and retry, or accept a slightly stale value.
- **Stale values during a lease.** After a delete, the old value is kept briefly, and
  clients that can tolerate it get it instead of waiting.
- **mcsqueal.** Invalidations sent from the database's commit log. See
  [research-cdc.md](research-cdc.md).
- **Remote markers** (§5, Race 1). Before writing from a non-primary region, set `r_k`.
  Readers that miss and see `r_k` read from the primary region instead of a lagging replica.
- **Gutter pool.** A small set of spare servers takes over a failed server's keys, with
  short TTLs, so its load doesn't spill onto the database.
- **Pools and replication within a pool**, by access pattern.

**The two races, in our notation** (full tables in [race-conditions.md](race-conditions.md)):

| Race | What happens | Their fix |
|---|---|---|
| 1, across regions | An invalidation lands before replication does, so a reader refills the cache from a stale replica | Remote marker, plus invalidations driven by the commit log |
| 2, stale set | A reader's `SELECT` happens before a write, and its `SET` happens after the write's `DEL` | Lease token checked at set time |


---

## 3. Stampede (thundering herd) defenses

| Technique | Where it lives | What it stops | Notes |
|---|---|---|---|
| **Leases or tokens** (Facebook) | Cache server | Herd and stale set | Needs server support. |
| **Memcached meta commands** (`N`, `W`, `Z`, `I` flags) | Cache server | Herd and stale set | `N` creates an empty placeholder on a miss. The first client gets `W` (it rebuilds), and the others get `Z` (wait). `I` marks the value stale instead of deleting it, so one client rebuilds while the others keep serving the old value. [Docs](https://docs.memcached.org/protocols/meta/) |
| **Placeholder in the same key** (Redis) | App plus Redis | Herd and stale set | `SET key note NX PX`, then `SET key value IFEQ note`. **Tier 4.3.** |
| **Separate lock key** (`lock:{key}`) | App plus Redis | Herd only | Common in tutorials and Redisson-style locks. A `DEL` of the value doesn't cancel the rebuild, so the stale set remains. The two keys can drift apart, and in a cluster they need a hash tag such as `{key}` to land on the same node. |
| **Singleflight / request coalescing** | One process | Herd within one process | Go's `singleflight`; Nginx `proxy_cache_lock`; Varnish request coalescing. Doesn't help across processes. We removed it as confusing before going multi-instance. |
| **TTL jitter** | App | Many keys expiring together | Add a random amount to each TTL, often about 10% of it. **Tier 4.3: 300 s + 0–10 s.** |
| **Probabilistic early refresh (XFetch)** | App | Herd on hot keys | Each reader refreshes early with a probability that rises as expiry approaches. Needs no lock. From Vattani et al., "Optimal Probabilistic Cache Stampede Prevention" (VLDB 2015). |
| **Stale-while-revalidate** | App or HTTP cache | Herd, and latency | Serve the old value while one request refreshes it. The `Cache-Control` extension is from RFC 5861. |
| **Keep-alive / sliding TTL** | App | Hot keys expiring | Extend the TTL of a hot key, with `GETEX` or `EXPIRE ... GT`. The old Tier 4.4 did this; it was dropped with that tier. |
| **Circuit breaker plus load shedding** | App | Database overload during a cache outage | The old Tier 4.5: an `AtomicBool`, a `PING` probe every second, and a semaphore of 10 that returns 503 with `Retry-After`. Dropped with that tier. |
| **Gutter pool** (Facebook) | Infrastructure | Load from a failed cache node | Short TTLs on spare servers. |

---

## 4. Redis primitives that replace hand-written code

| Command | Since | Replaces |
|---|---|---|
| `SET key value NX PX ms` | 2.6.12 | `SETNX` plus `PEXPIRE`, which weren't atomic together |
| `SET key value GET` | 6.2 | `GETSET` |
| `SET ... NX GET` | 7.0 | Lua that tried to acquire and returned the current holder (rueidisaside uses it) |
| `SET key value IFEQ old` | **8.4** | The Lua compare-and-set script: `if GET == old then SET` |
| `DELEX key IFEQ old` | **8.4** | The classic Lua unlock script: `if GET == token then DEL` |
| `GETEX key EX s` | 6.2 | `GET` plus `EXPIRE` |
| `EXPIRE key s GT` | 7.0 | Lua that only extended a TTL |

Docs: [SET](https://redis.io/docs/latest/commands/set/) and [DELEX](https://redis.io/docs/latest/commands/delex/).

**Why Lua existed:** an atomic check-then-act. `MULTI`/`EXEC` queues commands and runs them
together, but a later command can't depend on what an earlier one returned. `WATCH` can do
it, with retries. Lua runs the whole script atomically on the server. Redis 8.4 turned the
two most common scripts into built-in commands.

**Our "note"** isn't a Redis feature. It's an ordinary string, `"\0" + random hex`. Redis
only provides the generic compare-and-set; the meaning of the note lives in our code. The
NUL prefix works because Postgres `TEXT` can't store a NUL byte, so no URL can start with
one.

**Verified by hand on Redis 8.4.8:**
- `SET IFEQ` refuses a stale fill after a `DEL`.
- `DELEX IFEQ` only deletes the owner's key.
- `BF.*` commands are built into `redis:8.4`.

---

## 5. Libraries that package cache-aside with stampede protection

| Library | Language | How it works |
|---|---|---|
| [rueidisaside](https://github.com/redis/rueidis/tree/main/rueidisaside) | Go, by Redis | Placeholder `rueidisid:<random>` in the same key, set with `SET NX PX`. Lua scripts for compare-and-set and compare-and-delete. Waiters are notified through client-side caching invalidations instead of polling. **Tier 4.3 is this design, using `IFEQ` instead of Lua.** |
| [rockscache](https://github.com/dtm-labs/rockscache) | Go | One Redis hash per key, with fields `value`, `lockOwner` and `lockUntil`, all through Lua. Deletes mark a key deleted (`TagAsDeleted`) and let it expire after a delay, so stale values can still be served. Also jitters TTLs. |
| [IdentityCache](https://github.com/Shopify/identity_cache) | Ruby, by Shopify, on memcached | A fill lock `[FILL_LOCKED, client_id, data_version]` in the same key, written and checked with CAS. Deletes write a short-lived `DELETED` marker instead of removing the key, and a fill that finds it gives up. Its README lists lost invalidations and flushes as known causes of stale data, our risk #1. |
| Redis docs ([redis-rs cache-aside guide](https://redis.io/docs/latest/develop/use-cases/cache-aside/rust/), [stampede page](https://redis.antirez.com/fundamental/cache-stampede-prevention.html)) | Any | A **separate** `lock:{key}` with `SET NX PX` and a Lua compare-and-delete. It stops stampedes but not the stale set. |
| Redisson | Java | Distributed locks on separate keys. Not checked in detail. |
| Memcached meta protocol | Any | Placeholders and stale serving built into the server. |

I didn't find an equivalent library for Rust.

---

## 6. Eviction

- `maxmemory` is **unlimited by default** on 64-bit Redis, so nothing is evicted until the
  machine runs out of memory. Set it explicitly.
- Policies ([docs](https://redis.io/docs/latest/develop/reference/eviction/)):
  - `noeviction` (the default): writes fail when memory is full.
  - `allkeys-lru` and `allkeys-lfu`: any key can be evicted.
  - `volatile-lru`, `volatile-lfu`, `volatile-ttl` and `volatile-random`: only keys with a TTL.
  - Newer versions add LRM policies (8.6, not checked).
- **LRU vs. LFU:** LRU evicts the key read longest ago. LFU evicts the key read least often,
  using a counter that decays over time.
- Redis's general advice is `allkeys-lru` for a pure cache, with data that must survive on a
  separate instance. The `volatile-*` policies let one instance hold both.
- **Our choice:** `volatile-lfu` with `maxmemory 8mb` (small on purpose, for demos).
  - The Bloom filter and `bloom:ready` have no TTL, so they're never evicted.
  - Checked by hand: under pressure `volatile-lfu` kept the filter, while `allkeys-lfu`
    evicted it.
  - **Risk:** if keys without a TTL fill memory, nothing is left to evict, and writes fail.

---

## 7. Bloom filters

**How they work:**
- A bit array of size `m` and `k` hash functions. Adding an item sets `k` bits; checking it
  tests them.
- "No" is certain. "Yes" can be a false positive.
- Real implementations don't use `k` independent hashes. They use **double hashing**: two
  hashes `h1` and `h2`, with bit `i` at `h1 + i·h2 mod m` (Kirsch and Mitzenmacher, 2006).
  RedisBloom does this.
- Size: about **9.6 bits per item for a 1% false positive rate**, with about 7 hashes. One
  million codes is roughly 1.2 MB.
- Items can't be removed. Cuckoo filters (`CF.*` in Redis) support deletes, and scalable
  Bloom filters add layers as they grow.

**In Redis:**
- RedisBloom ships with Redis 8, so a plain `redis:8.4` image has it.
- The filter is just a key: `codes_bloom`.
- `BF.INSERT key CAPACITY n ERROR e ITEMS ...` creates it with the right size if it's
  missing. `BF.ADD` would create it with a default capacity of 100.
- `BF.SCANDUMP` and `BF.LOADCHUNK` export and import it in chunks.

**Our design (Tier 4.2):**
- **`bloom:ready`** is a key we create and manage. The filter may only say "absent" while
  the flag is set. A flush removes both, so reads fall back to Postgres safely.
- **Write order:** `shorten` adds to the filter **before** the `INSERT`. A failed insert
  then leaves a harmless false positive, never a false negative.
- **A background job** runs every minute, with a Postgres advisory lock so only one instance
  runs it.
  - When the flag is set, it re-adds the last 5 minutes of codes.
  - When the flag is missing, it rebuilds the filter.
- **Rebuilds** use a `bloom:building:{id}` marker plus `RENAME`, so a flush in the middle can
  never mark a half-built filter ready.
- **Snapshots** go to a bucket through the S3 API, with MinIO locally.
  - The bucket holds the chunks. A Postgres row points at the current snapshot.
  - A restore loads into a temporary key, `RENAME`s it into place, then replays codes from
    `taken_at` minus a margin, because `created_at` isn't commit order.
  - Production practice keeps large blobs in object storage, not in database rows.
- **Raft parallel:** restoring a snapshot and replaying only what came after it. The analogy
  breaks because Raft's log has a strict order and our `created_at` doesn't. Adding a code
  twice is harmless, which makes the overlap safe.

---

## 8. Redis failures and connections (redis-rs 1.7)

- Redis is in-memory. A restart without persistence empties everything, including the
  filter. AOF (`appendonly yes`) survives a plain restart.
- `ConnectionManager` reconnects by itself, so the app doesn't need a restart when Redis
  comes back.
  - Its default retries made requests hang about 9 s while Redis was down.
    `set_number_of_retries(1)` makes them fail in about 100 ms.
  - The default response timeout is 500 ms, and the connection timeout is 1 s.
- **Look-aside means a broken cache must not return 500 on reads.** Since Tier 4.1, any
  Redis error on a read falls back to Postgres.
- **Open:** while Redis is down, all reads land on Postgres with no limit. The old Tier 4.5's
  breaker and semaphore addressed this and may come back.

---

## 9. Decisions log

| Topic | Decision | Alternatives considered |
|---|---|---|
| Cache pattern | Cache-aside | Read-through, write-through |
| Writes | Postgres, then `DEL` | Update the cache; write the cache first |
| TTL | 300 s in every tier, plus 0–10 s of jitter in 4.3 | 60 s |
| Rebuild marker | Note in the same key | Separate lock key; Lua-based leases; singleflight |
| Waiting readers | Poll every 50 ms, give up after 200 ms and read Postgres without caching | Pub/Sub notifications; client-side caching invalidation (rueidisaside) |
| Note lifetime | 2 s | Longer leases (the old 4.3 used 5 s) |
| Eviction | `volatile-lfu`, `maxmemory 8mb` | `allkeys-lru` on a separate instance |
| Negative caching | None; the Bloom filter covers unknown codes | Cache "not found" with a short TTL |
| Filter durability | AOF, the repair job, snapshots in a bucket | Full scan on every start |
| Removed | Singleflight, keep-alive, circuit breaker (old Tiers 4.4 and 4.5) | To revisit later |

---

## Sources

- [Scaling Memcache at Facebook](https://www.usenix.org/system/files/conference/nsdi13/nsdi13-final170_update.pdf)
- [AWS ElastiCache: caching strategies](https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/Strategies.html)
- [Redis: SET](https://redis.io/docs/latest/commands/set/), [DELEX](https://redis.io/docs/latest/commands/delex/), [eviction](https://redis.io/docs/latest/develop/reference/eviction/)
- [Memcached meta commands](https://docs.memcached.org/protocols/meta/)
- [rueidisaside](https://github.com/redis/rueidis/tree/main/rueidisaside), [rockscache](https://github.com/dtm-labs/rockscache) and [IdentityCache](https://github.com/Shopify/identity_cache)
- Vattani, Chierichetti, Lowenstein: *Optimal Probabilistic Cache Stampede Prevention*, VLDB 2015
- Kirsch, Mitzenmacher: *Less Hashing, Same Performance: Building a Better Bloom Filter*, 2006
- RFC 5861: HTTP Cache-Control extensions for stale content
