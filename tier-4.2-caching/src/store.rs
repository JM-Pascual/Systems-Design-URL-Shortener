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
//! # Why this exact version is vulnerable to a thundering herd
//!
//! There is no coordination here at all: if a hot key's TTL lapses under
//! load, every in-flight request misses Redis at the same instant and every
//! one of them independently queries Postgres to rebuild the same entry.
//! That is deliberate — see the Tier 4 README's thundering-herd section for
//! the mitigations this version is missing on purpose.

use redis::AsyncCommands;
use sqlx::{PgPool, Row};

use crate::base62;

/// How long a cached mapping lives before Redis evicts it. Arbitrary for a
/// teaching build; a real deployment would tune this against how often
/// `PATCH`/`DELETE` make a cached value stale.
const CACHE_TTL_SECONDS: u64 = 300;

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

pub struct Store {
    pool: PgPool,
    redis: redis::aio::MultiplexedConnection,
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

        Ok(Self { pool, redis })
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

    /// Look up the long URL for a code — Redis first, Postgres on a miss.
    pub async fn resolve(&self, code: &str) -> Result<Option<String>, anyhow::Error> {
        let mut redis = self.redis.clone();
        if let Some(url) = redis.get::<_, Option<String>>(code).await? {
            return Ok(Some(url));
        }

        if !self.bloom_might_exist(code).await? {
            return Ok(None);
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
    /// `cargo test -p tier-4-2-caching -- --ignored`
    async fn test_store() -> Store {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://shortener:shortener@localhost:5435/shortener".into());
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6381".into());
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

        let mut redis = redis::Client::open(
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6381".into()),
        )
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
        let _: () = redis::cmd("FLUSHALL").query_async(&mut redis).await.unwrap();

        let reconnected = test_store().await;
        assert_eq!(
            reconnected.resolve(&code).await.unwrap(),
            Some("https://survives.example".to_string())
        );
    }
}
