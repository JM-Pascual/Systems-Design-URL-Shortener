//! Tier 4.1's store plus a Bloom filter of every minted code, so a code that
//! never existed costs no Postgres query. See this tier's README.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redis::AsyncCommands;
use sqlx::PgPool;

use crate::base62;

/// How long a cached mapping lives. Arbitrary for a teaching build; a real
/// deployment would tune it against how often links change.
const CACHE_TTL_SECONDS: u64 = 300;

/// The RedisBloom key holding every code that's ever been minted.
const CODES_BLOOM_KEY: &str = "codes_bloom";

/// Target false-positive rate and initial capacity for `BF.RESERVE`.
/// RedisBloom grows the filter automatically past this capacity.
const BLOOM_ERROR_RATE: f64 = 0.01;
const BLOOM_CAPACITY: i64 = 1_000_000;

/// How many codes go into one `BF.MADD` during the startup backfill.
const BLOOM_BACKFILL_CHUNK: usize = 1_000;

pub struct Store {
    pool: PgPool,
    /// Cheap to clone per use: every clone multiplexes the same connection.
    redis: redis::aio::MultiplexedConnection,
    /// How many times `resolve` has queried Postgres, for `/metrics`. This is
    /// the exact thing a stampede spikes.
    postgres_queries: AtomicU64,
    /// From `DEMO_QUERY_DELAY_MS`: a real local query is too fast for a demo's
    /// concurrent requests to land inside the miss window, so
    /// `../thundering-herd-demo/` simulates a slow one.
    demo_query_delay: Duration,
}

impl Store {
    /// Connect to Postgres (running pending migrations) and Redis, and make
    /// sure the Bloom filter exists and holds every code Postgres has. Without
    /// the backfill, a wiped Redis would turn every existing link into a 404.
    pub async fn connect(database_url: &str, redis_url: &str) -> anyhow::Result<Self> {
        let pool = PgPool::connect(database_url).await?;
        sqlx::migrate!().run(&pool).await?;

        let mut redis = redis::Client::open(redis_url)?
            .get_multiplexed_async_connection()
            .await?;

        // `BF.RESERVE` errors if the filter survives from a previous run,
        // which is expected on every start but the first.
        if let Err(e) = redis::cmd("BF.RESERVE")
            .arg(CODES_BLOOM_KEY)
            .arg(BLOOM_ERROR_RATE)
            .arg(BLOOM_CAPACITY)
            .query_async::<()>(&mut redis)
            .await
            && !e.to_string().contains("item exists")
        {
            return Err(e.into());
        }

        let codes: Vec<String> = sqlx::query_scalar("SELECT code FROM links")
            .fetch_all(&pool)
            .await?;
        for chunk in codes.chunks(BLOOM_BACKFILL_CHUNK) {
            redis::cmd("BF.MADD")
                .arg(CODES_BLOOM_KEY)
                .arg(chunk)
                .query_async::<()>(&mut redis)
                .await?;
        }

        let demo_query_delay_ms = std::env::var("DEMO_QUERY_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        Ok(Self {
            pool,
            redis,
            postgres_queries: AtomicU64::new(0),
            demo_query_delay: Duration::from_millis(demo_query_delay_ms),
        })
    }

    pub fn postgres_query_count(&self) -> u64 {
        self.postgres_queries.load(Ordering::Relaxed)
    }

    /// `false` means the code was *never* minted, so Postgres can be skipped.
    /// `true` means "probably", and still needs the real check.
    async fn bloom_might_exist(&self, code: &str) -> anyhow::Result<bool> {
        Ok(redis::cmd("BF.EXISTS")
            .arg(CODES_BLOOM_KEY)
            .arg(code)
            .query_async(&mut self.redis.clone())
            .await?)
    }

    /// Store a long URL and return the freshly minted short code.
    ///
    /// The two Redis writes sit on opposite sides of the `INSERT` on purpose:
    /// a code in the filter whose `INSERT` fails is a harmless false positive,
    /// while a cached value whose `INSERT` fails would resolve a link Postgres
    /// never had.
    pub async fn shorten(&self, url: String) -> anyhow::Result<String> {
        let id: i64 = sqlx::query_scalar("SELECT nextval('link_ids')")
            .fetch_one(&self.pool)
            .await?;
        let code = base62::encode(id as u64);
        let mut redis = self.redis.clone();

        redis::cmd("BF.ADD")
            .arg(CODES_BLOOM_KEY)
            .arg(&code)
            .query_async::<()>(&mut redis)
            .await?;

        sqlx::query("INSERT INTO links (code, long_url) VALUES ($1, $2)")
            .bind(&code)
            .bind(&url)
            .execute(&self.pool)
            .await?;

        redis
            .set_ex::<_, _, ()>(&code, &url, CACHE_TTL_SECONDS)
            .await?;

        Ok(code)
    }

    /// Look up the long URL for a code: Redis first, then the Bloom filter,
    /// then Postgres.
    pub async fn resolve(&self, code: &str) -> anyhow::Result<Option<String>> {
        let mut redis = self.redis.clone();
        if let Some(url) = redis.get::<_, Option<String>>(code).await? {
            return Ok(Some(url));
        }

        if !self.bloom_might_exist(code).await? {
            return Ok(None);
        }

        self.rebuild(code).await
    }

    /// Query Postgres and, on a hit, populate the cache.
    async fn rebuild(&self, code: &str) -> anyhow::Result<Option<String>> {
        self.postgres_queries.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.demo_query_delay).await;

        let url: Option<String> = sqlx::query_scalar(
            "SELECT long_url FROM links WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(url) = &url {
            self.redis
                .clone()
                .set_ex::<_, _, ()>(code, url, CACHE_TTL_SECONDS)
                .await?;
        }

        Ok(url)
    }

    /// Every link, for the `/stats` endpoint. Postgres only: it doesn't reflect
    /// what's currently cached.
    pub async fn dump_links(&self) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT code, long_url FROM links ORDER BY code")
            .fetch_all(&self.pool)
            .await
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

    async fn test_redis() -> redis::aio::MultiplexedConnection {
        redis::Client::open(
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6381".into()),
        )
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
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

    /// The property this tier adds: `shorten` writes through, so the first
    /// `resolve` is a cache hit and never queries Postgres.
    #[tokio::test]
    #[ignore]
    async fn resolve_hits_cache_without_a_prior_miss() {
        let store = test_store().await;
        let code = store
            .shorten("https://cached.example".into())
            .await
            .unwrap();
        assert_eq!(
            store.resolve(&code).await.unwrap(),
            Some("https://cached.example".to_string())
        );
        assert_eq!(store.postgres_query_count(), 0);
    }

    /// A Redis wipe must not turn existing links into 404s: a fresh `connect`
    /// backfills the Bloom filter from Postgres.
    #[tokio::test]
    #[ignore]
    async fn bloom_filter_survives_a_redis_flush() {
        let store = test_store().await;
        let code = store
            .shorten("https://survives.example".into())
            .await
            .unwrap();

        let mut redis = test_redis().await;
        let _: () = redis::cmd("FLUSHALL")
            .query_async(&mut redis)
            .await
            .unwrap();

        let reconnected = test_store().await;
        assert_eq!(
            reconnected.resolve(&code).await.unwrap(),
            Some("https://survives.example".to_string())
        );
    }
}
