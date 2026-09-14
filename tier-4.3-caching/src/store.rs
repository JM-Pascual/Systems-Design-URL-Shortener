//! The cache-aside store: Redis in front of the Postgres `links` table from
//! Tier 3.
//!
//! # Cache-aside, concretely
//!
//! `resolve` (the redirect path, ~99% of traffic per Tier 0) does:
//! 1. `GET` the code from Redis.
//! 2. On a hit, return it — no Postgres involved at all.
//! 3. On a miss, `SELECT` from Postgres (Tier 3's query, unchanged), then
//!    `SET` the result into Redis with a TTL before returning it.
//!
//! `shorten` writes through: after the `INSERT` (Tier 3's, unchanged), it
//! also `SET`s the new mapping into Redis, so a code's *first* redirect is
//! already a cache hit instead of paying one guaranteed miss.
//!
//! # Quarter 2: a Bloom filter for the negative case
//!
//! Before this, a `resolve` for a code that never existed cost a full
//! Postgres query *every single time* — there was no way to cache "this
//! definitely doesn't exist." A Bloom filter fixes that cheaply: `shorten`
//! adds every minted code to it, and `resolve` checks it on a Redis miss,
//! *before* touching Postgres. A Bloom filter can never false-negative (if
//! it says absent, the code truly was never minted) but can rarely
//! false-positive (says "maybe present" for a code that isn't) — which is
//! exactly the safe direction to be wrong in: a false positive costs one
//! wasted Postgres query, a false negative would incorrectly 404 a real
//! link.
//!
//! That guarantee only holds if the filter actually contains every code.
//! Two things protect it: `shorten` adds to the filter *before* the
//! `INSERT` (so a partial failure can only produce a harmless false
//! positive, never a false negative), and `connect` backfills the filter
//! from Postgres on every start (so a Redis restart or flush can't turn
//! every existing link into a 404).
//!
//! # Quarter 3: PATCH/DELETE, and a lease against the thundering herd
//!
//! `update` and `delete` both write Postgres first (same durability
//! discipline as `shorten`), then invalidate with a plain Redis `DEL` --
//! deliberately the simple, stampede-prone approach from the README, since
//! it's the concrete trigger `resolve`'s lease exists to defend against.
//! They also *hold* that lease while they do it: every write to a code's
//! cache entry, rebuild or invalidation, serializes on `lease:{code}`, so
//! an invalidation can't land in the middle of a rebuild and get its
//! `DEL` overwritten by a `SET` of the value it just made stale.
//!
//! A deleted code is never removed from the Bloom filter -- standard Bloom
//! filters have no removal operation. That's harmless, not a bug: `resolve`
//! still falls through to Postgres and correctly gets `None`; the code just
//! permanently loses the filter's fast-path benefit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redis::AsyncCommands;
use redis::{ExistenceCheck, SetExpiry, SetOptions};
use sqlx::{PgPool, Row};

use crate::base62;

/// How long a cached mapping lives before Redis evicts it. Arbitrary for a
/// teaching build; a real deployment would tune this against how often
/// `PATCH`/`DELETE` make a cached value stale.
const CACHE_TTL_SECONDS: u64 = 300;

/// How long a resolve lease is held before another request is allowed to
/// try acquiring it -- an upper bound on how long a crashed leader can block
/// everyone else from ever rebuilding this key.
const LEASE_TTL_SECONDS: u64 = 5;

/// How long a lease loser waits before its next attempt -- either the real
/// key will be populated by then, or the current lease will have expired
/// and this attempt becomes the new leader.
const LEASE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// The RedisBloom key holding every code that's ever been minted.
const CODES_BLOOM_KEY: &str = "codes_bloom";

/// Target false-positive rate and initial capacity for `BF.RESERVE`.
/// RedisBloom scales the filter automatically past this capacity, at the
/// cost of a slightly higher false-positive rate for the overflow — fine
/// for a teaching build; a production one would size this from Tier 0's
/// actual key-space estimate instead of a round number.
const BLOOM_ERROR_RATE: f64 = 0.01;
const BLOOM_CAPACITY: i64 = 1_000_000;

/// How many codes go into one `BF.MADD` during the startup backfill.
const BLOOM_BACKFILL_CHUNK: usize = 1_000;

/// The Redis key guarding all writes to a code's cache entry -- the miss
/// path's rebuild and `update`/`delete`'s invalidation alike.
fn lease_key(code: &str) -> String {
    format!("lease:{code}")
}

pub struct Store {
    pool: PgPool,
    redis: redis::aio::MultiplexedConnection,
    /// How many times `resolve` has actually queried Postgres -- i.e. how
    /// many times a request became the lease's *leader*. For the
    /// thundering-herd demo's `/metrics` endpoint: this is exactly the
    /// count the lease is supposed to keep near 1 per stampede, versus
    /// Tier 4.1's version where it tracks concurrent request count.
    postgres_queries: AtomicU64,
    /// See Tier 4.1's `Store` for why this exists: simulates a realistically
    /// slow backend query so a local demo's miss window is wide enough for
    /// concurrent requests to actually land inside it.
    demo_query_delay: Duration,
}

/// A generic dump of a table's rows, column names discovered at query time
/// rather than hardcoded — so `/stats` keeps working as the schema evolves.
pub struct TableDump {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Store {
    /// Connect to Postgres (running pending migrations) and Redis, and make
    /// sure the codes Bloom filter exists *and reflects every code Postgres
    /// already has*.
    ///
    /// The backfill is what keeps the filter honest across a Redis restart
    /// or flush. Without it, an empty filter would answer "definitely not
    /// minted" for every link Postgres still holds -- turning a cache
    /// outage into every existing link 404ing. `BF.ADD` is idempotent, so
    /// re-adding codes an existing filter already has is harmless.
    pub async fn connect(database_url: &str, redis_url: &str) -> Result<Self, anyhow::Error> {
        let pool = PgPool::connect(database_url).await?;
        sqlx::migrate!().run(&pool).await?;

        let client = redis::Client::open(redis_url)?;
        let mut redis = client.get_multiplexed_async_connection().await?;

        // `BF.RESERVE` errors if the filter already exists (from a previous
        // run against the same Redis) -- that's expected on every restart
        // but the first one, so it's the one error we swallow here.
        match redis::cmd("BF.RESERVE")
            .arg(CODES_BLOOM_KEY)
            .arg(BLOOM_ERROR_RATE)
            .arg(BLOOM_CAPACITY)
            .query_async::<()>(&mut redis)
            .await
        {
            Ok(()) => {}
            Err(e) if e.to_string().contains("item exists") => {}
            Err(e) => return Err(e.into()),
        }

        let codes: Vec<(String,)> = sqlx::query_as("SELECT code FROM links")
            .fetch_all(&pool)
            .await?;
        for chunk in codes.chunks(BLOOM_BACKFILL_CHUNK) {
            let mut cmd = redis::cmd("BF.MADD");
            cmd.arg(CODES_BLOOM_KEY);
            for (code,) in chunk {
                cmd.arg(code);
            }
            cmd.query_async::<Vec<i64>>(&mut redis).await?;
        }

        let demo_query_delay = Duration::from_millis(
            std::env::var("DEMO_QUERY_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        );

        Ok(Self {
            pool,
            redis,
            postgres_queries: AtomicU64::new(0),
            demo_query_delay,
        })
    }

    /// Total Postgres queries `resolve` has issued since this process
    /// started. Used only by the `/metrics` endpoint.
    pub fn postgres_query_count(&self) -> u64 {
        self.postgres_queries.load(Ordering::Relaxed)
    }

    /// Add a code to the Bloom filter. Called once, when `shorten` mints it.
    async fn bloom_add(&self, code: &str) -> Result<(), anyhow::Error> {
        let mut redis = self.redis.clone();
        redis::cmd("BF.ADD")
            .arg(CODES_BLOOM_KEY)
            .arg(code)
            .query_async::<i64>(&mut redis)
            .await?;
        Ok(())
    }

    /// `false` means the code was *never* minted -- safe to skip Postgres
    /// entirely. `true` means "probably", and still needs the real check.
    async fn bloom_might_exist(&self, code: &str) -> Result<bool, anyhow::Error> {
        let mut redis = self.redis.clone();
        let exists: i64 = redis::cmd("BF.EXISTS")
            .arg(CODES_BLOOM_KEY)
            .arg(code)
            .query_async(&mut redis)
            .await?;
        Ok(exists == 1)
    }

    /// Store a long URL and return the freshly minted short code.
    ///
    /// `self.redis.clone()` below isn't a network call — `MultiplexedConnection`
    /// is designed to be cloned per use, the same way `PgPool` hands out a
    /// connection per query. That's what lets `shorten`/`resolve` take `&self`
    /// instead of `&mut self`, so `Store` stays shareable across concurrent
    /// requests without a `Mutex`, exactly like Tier 3.
    ///
    /// Two Redis writes here, and they sit on *opposite* sides of the
    /// `INSERT` on purpose, because their failure modes point in opposite
    /// directions:
    ///
    /// - The Bloom filter is written **before** the `INSERT`. If the filter
    ///   gains a code whose `INSERT` then fails, that's a false positive --
    ///   one wasted Postgres query on a future miss, harmless. If instead
    ///   the `INSERT` committed and the `BF.ADD` then failed, that's a false
    ///   negative -- the filter says "never minted" and `resolve` 404s a
    ///   real link. So the filter goes first.
    /// - The value cache is written **after** the `INSERT`. Writing it first
    ///   would make a code externally resolvable before its row is durably
    ///   committed, so a failed `INSERT` after a successful `SET` would leave
    ///   Redis pointing at a URL Postgres never actually has.
    pub async fn shorten(&self, url: String) -> Result<String, anyhow::Error> {
        let (id,): (i64,) = sqlx::query_as("SELECT nextval('link_ids')")
            .fetch_one(&self.pool)
            .await?;
        let code = base62::encode(id as u64);

        self.bloom_add(&code).await?;

        sqlx::query("INSERT INTO links (code, long_url) VALUES ($1, $2)")
            .bind(&code)
            .bind(&url)
            .execute(&self.pool)
            .await?;

        let mut redis = self.redis.clone();
        redis.set_ex::<_, _, ()>(&code, &url, CACHE_TTL_SECONDS).await?;

        Ok(code)
    }

    /// One `SET lease:{code} 1 NX EX` attempt. `true` means this caller now
    /// holds the lease; `false` means someone else does.
    async fn try_acquire_lease(
        redis: &mut redis::aio::MultiplexedConnection,
        lease_key: &str,
    ) -> Result<bool, anyhow::Error> {
        let options = SetOptions::default()
            .conditional_set(ExistenceCheck::NX)
            .with_expiration(SetExpiry::EX(LEASE_TTL_SECONDS));
        let acquired: Option<String> = redis.set_options(lease_key, 1, options).await?;
        Ok(acquired.is_some())
    }

    /// Block until the lease is ours. Used by the invalidating writes,
    /// which -- unlike the miss path -- have no cached value to return
    /// early with, so they just wait their turn.
    async fn acquire_lease(
        redis: &mut redis::aio::MultiplexedConnection,
        lease_key: &str,
    ) -> Result<(), anyhow::Error> {
        while !Self::try_acquire_lease(redis, lease_key).await? {
            tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
        }
        Ok(())
    }

    /// Change a code's destination. Returns `false` if the code doesn't
    /// exist (caller turns that into a 404).
    ///
    /// Takes the same lease the miss path does. Without it there's a
    /// classic cache-aside race: a `resolve` leader `SELECT`s the *old* URL,
    /// this `UPDATE` + `DEL` land in between, and the leader then `SET`s the
    /// old URL back -- stale for a full TTL. Holding the lease across the
    /// `UPDATE` and the `DEL` means a rebuild can't be mid-flight while the
    /// row changes underneath it.
    ///
    /// Within the lease: Postgres first, same reasoning as `shorten` -- only
    /// invalidate the cache once the new value is durably committed.
    /// Invalidation is a plain `DEL`, not a write-through `SET`; see the
    /// module doc for why that's the point, not an oversight.
    pub async fn update(&self, code: &str, url: &str) -> Result<bool, anyhow::Error> {
        let mut redis = self.redis.clone();
        let lease_key = lease_key(code);
        Self::acquire_lease(&mut redis, &lease_key).await?;

        let outcome = async {
            let result = sqlx::query("UPDATE links SET long_url = $2 WHERE code = $1")
                .bind(code)
                .bind(url)
                .execute(&self.pool)
                .await?;
            if result.rows_affected() == 0 {
                return Ok(false);
            }
            redis.del::<_, ()>(code).await?;
            Ok(true)
        }
        .await;

        redis.del::<_, ()>(&lease_key).await?;
        outcome
    }

    /// Remove a code entirely. Returns `false` if it didn't exist.
    ///
    /// Without the `DEL` here, a deleted link would keep redirecting
    /// successfully -- serving a cached URL for a row that no longer
    /// exists -- until its TTL happened to lapse on its own. Lease-guarded
    /// for the same reason as `update`.
    pub async fn delete(&self, code: &str) -> Result<bool, anyhow::Error> {
        let mut redis = self.redis.clone();
        let lease_key = lease_key(code);
        Self::acquire_lease(&mut redis, &lease_key).await?;

        let outcome = async {
            let result = sqlx::query("DELETE FROM links WHERE code = $1")
                .bind(code)
                .execute(&self.pool)
                .await?;
            if result.rows_affected() == 0 {
                return Ok(false);
            }
            redis.del::<_, ()>(code).await?;
            Ok(true)
        }
        .await;

        redis.del::<_, ()>(&lease_key).await?;
        outcome
    }

    /// Look up the long URL for a code — Redis first, Postgres on a miss,
    /// with a lease guarding the miss path so only one request at a time
    /// rebuilds a given key.
    ///
    /// On a miss, every request loops: check the real key (maybe someone
    /// else just finished), else try to *become* the leader by acquiring
    /// `lease:{code}` (`SET ... NX EX`). Whoever gets it runs the Postgres
    /// query and populates the cache; everyone else sleeps and loops back —
    /// critically, back to *trying to acquire the lease themselves*, not
    /// just waiting on the real key. That's what makes this a real lease
    /// instead of a timeout-and-give-up mitigation: if the leader crashes
    /// before populating the cache, its lease expires on its own `EX`, and
    /// the very next waiter's retry becomes the new leader immediately —
    /// no request ever falls through to querying Postgres unguarded.
    pub async fn resolve(&self, code: &str) -> Result<Option<String>, anyhow::Error> {
        let mut redis = self.redis.clone();
        if let Some(url) = redis.get::<_, Option<String>>(code).await? {
            return Ok(Some(url));
        }

        if !self.bloom_might_exist(code).await? {
            return Ok(None);
        }

        let lease_key = lease_key(code);

        loop {
            // Check if another worker did a cache SET
            if let Some(url) = redis.get::<_, Option<String>>(code).await? {
                return Ok(Some(url));
            }

            // Attempt to acquire the lease
            if !Self::try_acquire_lease(&mut redis, &lease_key).await? {
                tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
                continue;
            }

            // We're the leader now. The lease is released on every way out
            // of the block below -- a populated cache, a genuine miss, *or*
            // a Postgres error -- rather than left to time out, so a failed
            // query doesn't stall every waiter for the full lease TTL.
            let outcome = self.rebuild(code, &mut redis).await;
            redis.del::<_, ()>(&lease_key).await?;
            return outcome;
        }
    }

    /// The leader's half of a miss: query Postgres and, on a hit, populate
    /// the cache. Only ever called while holding the code's lease.
    async fn rebuild(
        &self,
        code: &str,
        redis: &mut redis::aio::MultiplexedConnection,
    ) -> Result<Option<String>, anyhow::Error> {
        self.postgres_queries.fetch_add(1, Ordering::Relaxed);
        if !self.demo_query_delay.is_zero() {
            tokio::time::sleep(self.demo_query_delay).await;
        }
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT long_url FROM links WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((url,)) => {
                redis.set_ex::<_, _, ()>(code, &url, CACHE_TTL_SECONDS).await?;
                Ok(Some(url))
            }
            None => Ok(None),
        }
    }

    /// A literal `SELECT * FROM links` for the `/stats` endpoint. Unchanged
    /// from Tier 3 — this tier's new idea is the read path, not `/stats`.
    pub async fn dump_links(&self) -> Result<TableDump, sqlx::Error> {
        let columns: Vec<(String,)> = sqlx::query_as(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'links' \
             ORDER BY ordinal_position",
        )
        .fetch_all(&self.pool)
        .await?;
        let columns: Vec<String> = columns.into_iter().map(|(c,)| c).collect();

        let select_list = columns
            .iter()
            .map(|c| format!("{c}::text"))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!("SELECT {select_list} FROM links ORDER BY code");

        // `query` is built from `information_schema` column names, never
        // from caller input, so this dynamic string can't carry an
        // injection — hence the explicit `AssertSqlSafe` opt-in.
        let pg_rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&self.pool)
            .await?;
        let rows = pg_rows
            .iter()
            .map(|row| {
                (0..columns.len())
                    .map(|i| row.get::<Option<String>, _>(i))
                    .collect()
            })
            .collect();

        Ok(TableDump { columns, rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests need real Postgres and Redis reachable at `DATABASE_URL`
    /// / `REDIS_URL` (see `../docker-compose.yml`) and are marked `#[ignore]`
    /// so `cargo test` stays usable without either. Run them explicitly:
    /// `cargo test -p tier-4-3-caching -- --ignored`
    async fn test_store() -> Store {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://shortener:shortener@localhost:5436/shortener".into());
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6382".into());
        Store::connect(&database_url, &redis_url)
            .await
            .expect("connect to test database/redis")
    }

    #[tokio::test]
    #[ignore]
    async fn issues_sequential_codes() {
        let store = test_store().await;
        let a = store.shorten("https://a.example".into()).await.unwrap();
        let b = store.shorten("https://b.example".into()).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    #[ignore]
    async fn resolves_what_it_stored() {
        let store = test_store().await;
        let code = store
            .shorten("https://example.com/long/path".into())
            .await
            .unwrap();
        assert_eq!(
            store.resolve(&code).await.unwrap(),
            Some("https://example.com/long/path".to_string())
        );
    }

    #[tokio::test]
    #[ignore]
    async fn unknown_code_resolves_to_none() {
        let store = test_store().await;
        assert_eq!(store.resolve("does-not-exist").await.unwrap(), None);
    }

    /// The property this tier adds: a code resolves correctly even if it
    /// was never touched again after `shorten` wrote it through to Redis —
    /// i.e. resolving doesn't depend on a prior cache miss populating it.
    #[tokio::test]
    #[ignore]
    async fn resolve_hits_cache_without_a_prior_miss() {
        let store = test_store().await;
        let code = store.shorten("https://cached.example".into()).await.unwrap();
        // If `shorten` writes through correctly, this is a cache hit, not a
        // Postgres query -- there is no way to assert that from here without
        // instrumentation, but the point stands for a demo with redis-cli.
        assert_eq!(
            store.resolve(&code).await.unwrap(),
            Some("https://cached.example".to_string())
        );
    }

    /// A Redis wipe must not turn existing links into 404s: a fresh
    /// `connect` backfills the Bloom filter from Postgres, so a code minted
    /// before the wipe still passes the filter and resolves afterward.
    #[tokio::test]
    #[ignore]
    async fn bloom_filter_survives_a_redis_flush() {
        let store = test_store().await;
        let code = store.shorten("https://survives.example".into()).await.unwrap();

        let mut redis = test_redis().await;
        let _: () = redis::cmd("FLUSHALL").query_async(&mut redis).await.unwrap();

        let reconnected = test_store().await;
        assert_eq!(
            reconnected.resolve(&code).await.unwrap(),
            Some("https://survives.example".to_string())
        );
    }

    /// `update` must wait for the miss path's lease rather than racing it.
    /// A lease held by "someone else" (set by hand with nothing behind it)
    /// has to expire before the update can proceed, so the call takes at
    /// least that long.
    #[tokio::test]
    #[ignore]
    async fn update_waits_for_an_outstanding_lease() {
        let store = test_store().await;
        let code = store.shorten("https://before.example".into()).await.unwrap();

        let mut redis = test_redis().await;
        let opts = SetOptions::default()
            .conditional_set(ExistenceCheck::NX)
            .with_expiration(SetExpiry::EX(2));
        let _: Option<String> = redis.set_options(lease_key(&code), 1, opts).await.unwrap();

        let started = std::time::Instant::now();
        assert!(store.update(&code, "https://after.example").await.unwrap());
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "update should have waited for the 2s lease, took {:?}",
            started.elapsed()
        );

        assert_eq!(
            store.resolve(&code).await.unwrap(),
            Some("https://after.example".to_string())
        );
    }

    async fn test_redis() -> redis::aio::MultiplexedConnection {
        redis::Client::open(
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6382".into()),
        )
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
    }
}
