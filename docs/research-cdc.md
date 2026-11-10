# CDC and cache consistency research notes

Personal notes on keeping the cache in sync with the database after writes, for the tier
after 4.3. Not part of the course. The cache itself is in
[research-cache.md](research-cache.md).

**The problem:** in Tier 4.3 a writer commits to Postgres, then sends `DEL`. If that `DEL` is
lost to a Redis blip, an outage or an app crash, the old URL stays cached for up to one TTL.
The fix is to tie the invalidation to the commit, so it's retried until it lands.

---

## 1. Approaches to cache consistency

| Approach | Example | Notes |
|---|---|---|
| Invalidate from the app after commit | Tiers 4.1 to 4.3 | Simple. A failed `DEL` leaves a stale value until the TTL expires. |
| **CDC** from the database log | Facebook's mcsqueal; Uber's Flux; Debezium | Invalidations can't be lost or overtake replication. The current form of mcsqueal. |
| Versions checked in the cache | Uber's CacheFront ([blog](https://www.uber.com/en-IN/blog/how-uber-serves-over-150-million-reads/)) | Lua scripts reject writes with older row versions. |
| Read-your-writes on replicas | Compare the write's LSN with `pg_last_wal_replay_lsn()` | Postgres 19's `WAIT FOR LSN` is reported to build this in ([post](https://rednafi.com/system/wait-for-lsn/)). Not verified against the release. |
| Consistency checker | Facebook's Polaris | Samples and measures instead of preventing. |

**Facebook:**
- **mcsqueal**, from the 2013 paper, reads the MySQL commit log in each region and sends
  invalidations. Because they come from the local log, they can't overtake replication.
- **Polaris** ([Cache made consistent](https://engineering.fb.com/2022/06/08/core-infra/cache-made-consistent/), 2022) samples invalidations and checks that the cache agrees with the database. It
  reports results as "N nines of consistency within T seconds". It measures, it doesn't
  deliver: CDC makes invalidations reliable, a checker shows whether they worked.

---

## 2. CDC options, under the hood

**Postgres logical decoding** ([docs](https://www.postgresql.org/docs/current/logicaldecoding-explanation.html)):
- With `wal_level = logical`, Postgres can decode its write-ahead log into row changes.
- A **replication slot** remembers how far a consumer has read, survives crashes, and keeps
  WAL until the consumer confirms it. One consumer per slot.
- An **output plugin** formats the changes. `pgoutput` is built in (since 10), `test_decoding`
  is built in but meant for testing, and `wal2json` is an extension. A **publication** picks
  which tables `pgoutput` sends.
- Changes arrive per committed transaction, in commit order (general knowledge, not on that
  page). After a crash the slot can go back to an earlier position, so delivery is
  **at least once**. `DEL` is idempotent, so that's fine.
- Deletes carry the old primary key by default (`REPLICA IDENTITY DEFAULT`). `code` is the
  primary key, so a `DELETE` tells us which key to invalidate.
- `pg_logical_emit_message(true, prefix, payload)` writes a transactional message into the
  WAL with no table, so the app can say "invalidate this code" exactly at commit.
- **Main risk:** a stalled consumer makes WAL pile up until the disk fills.
  `max_slot_wal_keep_size` (Postgres 13) caps it by invalidating the slot, which means lost
  invalidations again. Slots also didn't survive a primary failover until Postgres 17's
  failover slots ([Morling](https://www.morling.dev/blog/failover-replication-slots-with-postgres-17/)).

**Tools built on it:**

| Tool | How | Fit for us |
|---|---|---|
| [Debezium](https://debezium.io/documentation/reference/stable/connectors/postgresql.html) | Java. `pgoutput`, an initial snapshot, then streaming; the LSN is the offset. Runs in Kafka Connect, as Debezium Server, or embedded. | Debezium Server can write to Redis Streams and keep its offsets in Redis ([Redis docs](https://redis.io/docs/latest/integrate/redis-data-integration/installation/debezium-server-configuration/)), but a consumer still has to turn events into `DEL`s. A JVM container for one table. |
| [Sequin](https://github.com/sequinstream/sequin) | Elixir. A slot plus its own state tables. Sinks include Redis Streams and Redis strings (`SET`). | Simpler to run than Debezium; still another service. |
| [Supabase ETL](https://github.com/supabase/etl) | Rust framework, formerly `pg_replicate`. Publication plus slot. | Aimed at analytics destinations; distributed through git, not crates.io. |
| [pgwire-replication](https://docs.rs/crate/pgwire-replication/latest) | Rust, tokio. Speaks the replication protocol directly; supports logical messages. Doesn't decode `pgoutput` into rows. | Small worker of our own, a direct mcsqueal analogue. Young (0.3.x). |
| `pg_walstream` | Rust. Logical replication protocol versions 1 to 4. | Not checked in detail. |

**Without a slot:**
- **Transactional outbox:** insert `code` into an invalidations table in the same transaction.
  A worker reads it (`FOR UPDATE SKIP LOCKED`), sends `DEL`, deletes the row. Only plain SQL.
- **Triggers** can fill the outbox, which also catches manual edits.
- **`LISTEN/NOTIFY`** is sent at commit, but only to listeners connected at that moment. Not durable.

**Historical:** Facebook's mcsqueal and Uber's Flux read the MySQL binlog. Alibaba's Canal does
the same and is widely used for cache invalidation. Confluent's Bottled Water (2015) streamed
Postgres to Kafka and preceded Debezium's Postgres connector.

---

## 3. Who sends the `DEL`

**Why a late `DEL` is always safe:** a `DEL` can't make the cache wrong, only empty, and an
empty cache falls through to Postgres. The 4.3 ordering argument only needs the `DEL` to
come after the commit, and CDC keeps that, just later. Updating the cache from the WAL
(`SET` instead of `DEL`) would be different: out-of-order or late events could write an
older value, which is why Uber's CacheFront checks row versions.

**Side effects of a consumer-only `DEL`:**

- **No read-your-writes.** A client that `PATCH`es and immediately follows its link can get
  the old URL until the consumer catches up.
- **Extra misses.** A reader can rebuild the key between the commit and the consumer's
  `DEL`, loading the correct new URL, and the late `DEL` then throws it away. It costs one
  extra rebuild per write; nothing wrong is served.
- **Delivery depends on the consumer's retry policy.** CDC only guarantees the `DEL` if
  the consumer withholds its confirmation until Redis accepts it, so Postgres resends it.
  Confirming after a failed attempt drops it for good. It's also lost if the slot is
  invalidated by `max_slot_wal_keep_size`, or doesn't survive a failover. Either way the TTL
  is the backstop again.
- **Old values stay visible for the consumer's lag.** A URL cached before the write is
  served until the `DEL` arrives. If the consumer stalls, that window grows with the stall,
  still capped by the TTL.

**The three options:**

| | Handler only (Tier 4.3 today) | Consumer only | Handler + consumer (Facebook) |
|---|---|---|---|
| **Read-your-writes** | Yes, while the `DEL` succeeds | No, delayed by the consumer's lag | Yes, while the handler's `DEL` succeeds |
| **A lost `DEL`** | Stale for up to one TTL (310 s) | Repaired, **only if the consumer retries**: it confirms a position after Redis accepts the `DEL`. A consumer that tries once and confirms anyway loses it, like the handler. | The handler tries once and leaves failures to the consumer, which repairs them after its lag, under the same condition |
| **Manual edits in `psql`** | Never invalidated, stale for one TTL | Invalidated | Invalidated |
| **Extra misses per write** | None | About one | About one, from the consumer's second `DEL` |
| **Write path** | Each `PATCH`/`DELETE` waits on Redis | Never touches Redis | Waits on Redis |
| **Redis traffic from writes** | One `DEL` | One `DEL` | Two `DEL`s, trivial at about 40 writes/s |
| **Moving parts** | None extra | Worker, slot, publication, lag monitoring | Same as consumer only, plus the handler path |
| **Main risk** | Silent staleness after blips | Stalled consumer: WAL grows, invalidations wait | **A broken consumer goes unnoticed**, because the handler's `DEL` hides it until a blip happens. Alert on slot lag. |
| **Ease of reasoning** | One path | One path | Two paths doing the same job; it's tempting to remove either one by mistake |

**Why two `DEL`s are fine: `DEL` is idempotent.** Running it once or ten times leaves the
same state, the key absent. So a duplicate from the handler and the consumer, or a resend
after a consumer crash, can't change the outcome. Two caveats:

- It's idempotent in the cache's *state*, not in *cost*. If a reader rebuilds the key between
  the two `DEL`s, the second one removes a correct value, costing one extra miss.
- Its order relative to *writes into the cache* still matters. A `DEL` only helps if it
  comes after the commit, which both paths guarantee.

**Decision: handler + consumer.** The handler sends one `DEL` and doesn't retry. The
consumer retries until Redis accepts, then confirms the position.

**Facebook ran handler + consumer.** mcsqueal sends every invalidation from the commit log,
and "as an optimization, a web server that modifies data also sends invalidations to its own
cluster" for read-after-write (paper, §4.1). For this course, consumer only shows the idea
most clearly, and adding the handler's `DEL` back is a good discussion question.

---

## Sources

- [Postgres: logical decoding concepts](https://www.postgresql.org/docs/current/logicaldecoding-explanation.html)
- [Debezium Postgres connector](https://debezium.io/documentation/reference/stable/connectors/postgresql.html) and [Debezium Server with Redis](https://redis.io/docs/latest/integrate/redis-data-integration/installation/debezium-server-configuration/)
- [Failover replication slots with Postgres 17](https://www.morling.dev/blog/failover-replication-slots-with-postgres-17/)
- [Sequin](https://github.com/sequinstream/sequin), [Supabase ETL](https://github.com/supabase/etl), [pgwire-replication](https://docs.rs/crate/pgwire-replication/latest)
- [Meta: Cache made consistent](https://engineering.fb.com/2022/06/08/core-infra/cache-made-consistent/)
- [Uber: How Uber serves over 150 million reads](https://www.uber.com/en-IN/blog/how-uber-serves-over-150-million-reads/)
- [Reading your own writes with WAIT FOR LSN](https://rednafi.com/system/wait-for-lsn/)
- [IdentityCache README](https://github.com/Shopify/identity_cache): lists lost invalidations as a known cause of stale data
