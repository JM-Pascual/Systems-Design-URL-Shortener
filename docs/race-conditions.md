# Cache race conditions

Two races from [*Scaling Memcache at Facebook*](https://www.usenix.org/system/files/conference/nsdi13/nsdi13-final170_update.pdf)
(NSDI 2013). Both end the same way: a stale value is written into the cache after the
invalidation that should have removed it, and it stays there until its TTL runs out.

**Notation:** `C_i` is a client (an app process handling a request), and `R_i` is a
region. `R_1` is the main region, which holds the primary database. `R_2` holds a
read-only replica. `k` is a cache key, and `v_old` and `v_new` are its values.

**Assumption:** every write comes with an invalidation of the same key in the cache,
if the key is there.

## Race 1: across regions (paper §5)

`C_1` in `R_2` writes to a key, and another read in `R_2` refills the cache from a
replica that hasn't caught up.

| Step | Actor | Action | Result |
|---|---|---|---|
| 1 | `C_1` in `R_2` | Writes `v_new` to the primary in `R_1` | `R_1` has `v_new`; the replica in `R_2` still has `v_old` |
| 2 | `C_1` in `R_2` | Invalidates `k` in the `R_2` cache | The cache has no `k` |
| 3 | `C_2` in `R_2` | Reads `k`, misses, and queries the `R_2` replica | Gets `v_old`, because replication hasn't arrived |
| 4 | `C_2` in `R_2` | Sets `k = v_old` | **The cache holds `v_old`** |
| 5 | Replication | The `R_2` replica applies the write | The database has `v_new`; the cache is stale |

**The condition that makes it happen:** replication lag. The invalidation in step 2
reaches the cache before the data reaches the replica.

**The paper's fix:** a remote marker `r_k`. `C_1` sets `r_k` in `R_2` before writing,
and the write removes it once applied. A client that misses on `k` checks `r_k`, and if
it's there, reads from `R_1` instead. This costs extra latency on a miss, and in rare
cases concurrent writes to one key can remove a marker too early.

A related case is covered separately in the paper: invalidating `R_2` directly after a
write in `R_1` races the same way. That's why `mcsqueal` runs in each region and
invalidates from the local commit log, so the invalidation can't overtake the data.

## Race 2: inside a region (paper §3.2.1, "stale set")

`C_1` reads the database, `C_2` writes and invalidates, and then `C_1` writes the value
it read earlier.

| Step | Actor | Action | Result |
|---|---|---|---|
| 1 | `C_1` | Reads `k`, misses, and queries the database | Gets `v_old` |
| 2 | `C_2` | Writes `v_new` to the database | The database has `v_new` |
| 3 | `C_2` | Invalidates `k` | The cache has no `k`, so there's nothing to remove yet |
| 4 | `C_1` | Sets `k = v_old` | **The cache holds `v_old`** |

**The condition that makes it happen:** the read and the set aren't atomic, and the
invalidation lands between them.

**The paper's fix:** a lease. On a miss, the cache gives `C_1` a token, and the set only
succeeds if the token is still valid. A delete invalidates the token, so step 4 is
rejected. Limiting how often tokens are issued for a key also handles thundering herds.

## Do we cover them?

| Race | Status | Why |
|---|---|---|
| Race 1 | **Not covered, and can't occur today** | There's one Postgres and one Redis, with no replicas or regions. It would appear as soon as reads went to a lagging replica. |
| Race 1 | **Designed, Tier 5** | Each region's cache sync reads its own replica's WAL, so its `DEL` lands only after the replica has the new value. No code yet. |
| Race 2 | **Open on purpose, Tiers 4.1 and 4.2** | They have no coordination, and `a_slow_read_can_cache_a_stale_value` reproduces it. The 5-minute TTL bounds the damage. |
| Race 2 | **Covered, Tier 4.3** | `C_1` leaves a note in `k` before its `SELECT`. `C_2`'s `DEL` removes it, so `C_1`'s `SET ... IFEQ` is refused. `a_slow_read_cannot_cache_a_stale_value` checks it. |

A stall doesn't reopen it either: if `C_1`'s note expires and another reader leaves its own,
`C_1`'s write still doesn't match and is refused.

**What Tier 4.3 still leaves open: a lost invalidation.** Everything above assumes step 3's
`DEL` reaches the cache. If it fails after the commit, because of a network blip, a Redis
restart or the app crashing in between, `C_1`'s note survives and step 4 succeeds again. A
value cached before the write also stays. Either way, the TTL is the only bound.

The fix is to tie the invalidation to the commit, so it's retried until it lands:

- **Transactional outbox:** write the code to an invalidations table in the same
  transaction, and have a worker send the `DEL` and remove the row.
- **CDC:** read Postgres's logical replication stream and send a `DEL` for each change, as
  `mcsqueal` did with MySQL. Debezium is the common tool.

Either way the `DEL` arrives at least once, possibly late, so staleness shrinks from one TTL
to the delay of that pipeline. [Tier 4.4](../tier-4.4-caching/README.md) uses CDC, and keeps
the handler's `DEL` too for read-your-writes.

## Current techniques

| Technique | Solves | Needs | Notes |
|---|---|---|---|
| **`SET key value IFEQ token`** with a placeholder in the same key | Race 2 | Redis 8.4 | Native compare-and-set. It works like the paper's lease, with no Lua and no second key. Tier 4.3 uses it. |
| **`DELEX key IFEQ token`** | Unsafe lock release | Redis 8.4 | Replaces the Lua unlock script. |
| Version check in Lua | Race 2, and out-of-order refills | A row version | Uber's CacheFront dedupes writes with a Lua script. |
| CDC invalidation | Lost invalidations; Race 1 if run per region | A log reader such as Debezium | The current form of `mcsqueal`. Uber's Flux tails the binlog. |
| Remote marker, or routing by WAL position | Race 1 | Replicas | Compare `pg_last_wal_replay_lsn()` with the write's LSN. Postgres 19's `WAIT FOR` is reported to build this in. |
| Memcached meta commands | Race 2, stampedes | Memcached, not Redis | The win, stale and CAS flags build leases into the protocol. |
| `rockscache`, `rueidisaside` | Race 2, stampedes | Go | Libraries that package this. I didn't find an equivalent for Rust. |
| Polaris-style checker | Detecting all of the above | Sampling | Meta's approach: compare the cache against the database after invalidations. |

### Race 2 with a placeholder

The placeholder sits in the cached key itself, so a plain `DEL` also cancels any rebuild
in progress:

```text
C_1: SET k <token-1> NX PX 2000        → OK, C_1 will rebuild
C_3: SET k <token-3> NX PX 2000        → nil, C_3 waits and retries
C_1: SELECT                            → v_old
C_2: UPDATE, then DEL k                → removes the placeholder, and C_2 never waits
C_1: SET k v_old IFEQ <token-1> EX 300 → nil, rejected
```

The token must be something a stored URL can't equal, because readers have to tell a
placeholder from a value.

[Tier 4.3](../tier-4.3-caching/README.md) implements this, with a 2-second note and a
jittered TTL of 300 to 310 seconds.

## Sources

- [Scaling Memcache at Facebook](https://www.usenix.org/system/files/conference/nsdi13/nsdi13-final170_update.pdf)
- [Redis: SET](https://redis.io/docs/latest/commands/set/) and [DELEX](https://redis.io/docs/latest/commands/delex/)
- [Memcached: meta commands](https://docs.memcached.org/protocols/meta/)
- [Uber: How Uber serves over 150 million reads](https://www.uber.com/en-IN/blog/how-uber-serves-over-150-million-reads/)
- [Meta: Cache made consistent](https://engineering.fb.com/2022/06/08/core-infra/cache-made-consistent/)
- [rockscache](https://github.com/dtm-labs/rockscache) and [rueidisaside](https://github.com/redis/rueidis/tree/main/rueidisaside)
- [Reading your own writes with WAIT FOR LSN in Postgres 19](https://rednafi.com/system/wait-for-lsn/)
