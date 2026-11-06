//! Tier 4.3's store plus singleflight, a TTL keep-alive and TTL jitter. See
//! this tier's README.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use redis::AsyncCommands;
use sqlx::postgres::PgArguments;
use sqlx::query::Query;
use sqlx::{PgPool, Postgres};
use tokio::sync::OnceCell;

use crate::base62;
use crate::bloom::Bloom;
use crate::config::Config;

/// Base TTL for a cached mapping, before jitter. Arbitrary for a teaching
/// build; a real deployment would tune it against how often links change.
const CACHE_TTL_SECONDS: u64 = 300;

/// Upper bound on the random addition to `CACHE_TTL_SECONDS`, so keys minted
/// close together don't also expire close together.
const CACHE_TTL_JITTER_SECONDS: u64 = 30;

/// Upper bound on how long a crashed lease holder can block everyone else.
const LEASE_TTL_SECONDS: u64 = 5;

/// How long a request waits before trying for the lease again.
const LEASE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// How close to expiry a key has to be before reads start rolling for an
/// extension. See `should_extend_ttl`.
const TTL_EXTEND_WINDOW_SECONDS: f64 = 5.0;

/// What a successful roll sets the remaining TTL *to*. Much shorter than
/// `CACHE_TTL_SECONDS`, so a key's survival tracks its recent traffic.
const TTL_EXTENSION_SECONDS: i64 = 30;

fn lease_key(code: &str) -> String {
    format!("lease:{code}")
}

fn jittered_ttl() -> u64 {
    CACHE_TTL_SECONDS + rand::thread_rng().gen_range(0..CACHE_TTL_JITTER_SECONDS)
}

/// The keep-alive's roll. `-ln(rand)` is exponentially distributed with mean
/// 1, so the per-read chance is `e^(-remaining / window)`: effectively zero
/// far from expiry, climbing sharply as the TTL shrinks. `rand` is never
/// exactly 0, which would make `-ln` infinite.
fn should_extend_ttl(remaining_ttl_secs: f64) -> bool {
    let r: f64 = rand::thread_rng().gen_range(f64::EPSILON..1.0);
    TTL_EXTEND_WINDOW_SECONDS * (-r.ln()) >= remaining_ttl_secs
}

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
    /// Singleflight: codes currently being rebuilt by *this* process.
    inflight: Mutex<HashMap<String, Arc<OnceCell<Option<String>>>>>,
    pub bloom: Bloom,
}

impl Store {
    /// Connect to Postgres (running pending migrations) and Redis. The Bloom
    /// filter fills in the background: until it's ready, `resolve` skips it.
    pub async fn connect(config: &Config) -> anyhow::Result<Self> {
        let pool = PgPool::connect(&config.database_url).await?;
        sqlx::migrate!().run(&pool).await?;

        let redis = redis::Client::open(config.redis_url.as_str())?
            .get_multiplexed_async_connection()
            .await?;
        let bloom = Bloom::new(pool.clone(), redis.clone(), config)?;

        let demo_query_delay_ms = std::env::var("DEMO_QUERY_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        Ok(Self {
            pool,
            redis,
            postgres_queries: AtomicU64::new(0),
            demo_query_delay: Duration::from_millis(demo_query_delay_ms),
            bloom,
            inflight: Mutex::new(HashMap::new()),
        })
    }

    pub fn postgres_query_count(&self) -> u64 {
        self.postgres_queries.load(Ordering::Relaxed)
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

        self.bloom.add(&code).await?;

        sqlx::query("INSERT INTO links (code, long_url) VALUES ($1, $2)")
            .bind(&code)
            .bind(&url)
            .execute(&self.pool)
            .await?;

        redis
            .set_ex::<_, _, ()>(&code, &url, jittered_ttl())
            .await?;

        Ok(code)
    }

    /// One `SET lease:{code} 1 NX EX ttl` attempt. `true` means this caller
    /// now holds the lease.
    async fn try_acquire_lease(&self, code: &str) -> anyhow::Result<bool> {
        let acquired: Option<String> = redis::cmd("SET")
            .arg(lease_key(code))
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(LEASE_TTL_SECONDS)
            .query_async(&mut self.redis.clone())
            .await?;
        Ok(acquired.is_some())
    }

    /// Change a code's destination. `false` means no such code.
    pub async fn update(&self, code: &str, url: &str) -> anyhow::Result<bool> {
        let query = sqlx::query("UPDATE links SET long_url = $2 WHERE code = $1")
            .bind(code)
            .bind(url);
        self.write_and_invalidate(code, query).await
    }

    /// Remove a code entirely. `false` means no such code.
    pub async fn delete(&self, code: &str) -> anyhow::Result<bool> {
        let query = sqlx::query("DELETE FROM links WHERE code = $1").bind(code);
        self.write_and_invalidate(code, query).await
    }

    /// Run a write, then drop the cached value, holding the lease across both.
    /// Without it, a rebuild that `SELECT`ed the old URL could `SET` it back
    /// right after this `DEL`, stale for a full TTL.
    async fn write_and_invalidate(
        &self,
        code: &str,
        query: Query<'_, Postgres, PgArguments>,
    ) -> anyhow::Result<bool> {
        while !self.try_acquire_lease(code).await? {
            tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
        }

        let mut redis = self.redis.clone();
        let outcome = async {
            let found = query.execute(&self.pool).await?.rows_affected() > 0;
            if found {
                redis.del::<_, ()>(code).await?;
            }
            Ok(found)
        }
        .await;

        redis.del::<_, ()>(lease_key(code)).await?;
        outcome
    }

    /// Look up the long URL for a code. A hit also rolls for the keep-alive;
    /// a miss goes through the Bloom filter, then the coalesced rebuild.
    pub async fn resolve(&self, code: &str) -> anyhow::Result<Option<String>> {
        if !base62::is_valid(code) {
            return Ok(None);
        }

        let mut redis = self.redis.clone();

        let (value, ttl): (Option<String>, i64) = redis::pipe()
            .atomic()
            .get(code)
            .ttl(code)
            .query_async(&mut redis)
            .await?;

        if let Some(url) = value {
            if ttl > 0 && should_extend_ttl(ttl as f64) {
                redis.expire::<_, ()>(code, TTL_EXTENSION_SECONDS).await?;
            }
            return Ok(Some(url));
        }

        if !self.bloom.might_exist(code).await? {
            return Ok(None);
        }

        self.resolve_miss_coalesced(code).await
    }

    /// Singleflight: every concurrent same-process call for this code shares
    /// one `resolve_miss`, so followers never touch Redis's lease at all.
    async fn resolve_miss_coalesced(&self, code: &str) -> anyhow::Result<Option<String>> {
        let cell = self
            .inflight
            .lock()
            .expect("inflight mutex poisoned")
            .entry(code.to_string())
            .or_default()
            .clone();

        let outcome = cell
            .get_or_try_init(|| self.resolve_miss(code))
            .await
            .cloned();

        // A `OnceCell` only initializes once, so keeping it would freeze this
        // code's value forever. The next stampede gets a fresh one.
        self.inflight
            .lock()
            .expect("inflight mutex poisoned")
            .remove(code);

        outcome
    }

    /// Whoever gets the lease rebuilds; everyone else sleeps and tries again,
    /// for the lease itself and not just the key. If the holder crashes, its
    /// lease expires and the next waiter takes over, so no request ever queries
    /// Postgres unguarded and none is blocked forever.
    async fn resolve_miss(&self, code: &str) -> anyhow::Result<Option<String>> {
        let mut redis = self.redis.clone();

        loop {
            if let Some(url) = redis.get::<_, Option<String>>(code).await? {
                return Ok(Some(url));
            }

            if self.try_acquire_lease(code).await? {
                // Released on every outcome, so a failed query doesn't stall
                // every waiter for the full lease TTL.
                let outcome = self.rebuild(code).await;
                redis.del::<_, ()>(lease_key(code)).await?;
                return outcome;
            }

            tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
        }
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
                .set_ex::<_, _, ()>(code, url, jittered_ttl())
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

    /// These tests need the services in `../docker-compose.yml` and are marked
    /// `#[ignore]` so `cargo test` stays usable without them. Run them with:
    /// `cargo test -p tier-4-4-caching -- --ignored --test-threads=1`
    async fn test_store() -> Store {
        Store::connect(&Config::from_env())
            .await
            .expect("connect to test services")
    }

    async fn test_redis() -> redis::aio::MultiplexedConnection {
        redis::Client::open(
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6383".into()),
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

    /// Keys that aren't codes mustn't be read back as if they were.
    #[tokio::test]
    #[ignore]
    async fn redis_keys_are_not_resolvable() {
        let store = test_store().await;
        assert_eq!(store.resolve("codes_bloom").await.unwrap(), None);
        assert_eq!(store.resolve("bloom:ready").await.unwrap(), None);
    }

    /// `update` must wait for the miss path's lease rather than racing it. A
    /// lease held by "someone else" (set by hand) has to expire first.
    #[tokio::test]
    #[ignore]
    async fn update_waits_for_an_outstanding_lease() {
        let store = test_store().await;
        let code = store
            .shorten("https://before.example".into())
            .await
            .unwrap();

        let mut redis = test_redis().await;
        let _: () = redis::cmd("SET")
            .arg(lease_key(&code))
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(2)
            .query_async(&mut redis)
            .await
            .unwrap();

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

    /// Many concurrent same-process misses collapse into one Postgres query
    /// via singleflight, not just "eventually converge" via the lease's loop.
    #[tokio::test]
    #[ignore]
    async fn concurrent_misses_singleflight_to_one_query() {
        let store = Arc::new(test_store().await);
        let code = store
            .shorten("https://singleflight.example".into())
            .await
            .unwrap();

        let mut redis = test_redis().await;
        let _: () = redis.del(&code).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..20 {
            let store = Arc::clone(&store);
            let code = code.clone();
            handles.push(tokio::spawn(async move { store.resolve(&code).await }));
        }
        for h in handles {
            assert_eq!(
                h.await.unwrap().unwrap(),
                Some("https://singleflight.example".to_string())
            );
        }

        assert_eq!(
            store.postgres_query_count(),
            1,
            "singleflight should collapse all 20 misses into 1 query"
        );
    }

    /// The keep-alive's positive case: a key inside the extension window that
    /// keeps getting read has its expiry pushed out, with no Postgres query.
    /// 50 reads at 3s remaining (~55% per read) leaves a ~1e-17 chance of no
    /// extension.
    #[tokio::test]
    #[ignore]
    async fn sustained_reads_near_expiry_extend_ttl() {
        let store = test_store().await;
        let code = store
            .shorten("https://keepalive.example".into())
            .await
            .unwrap();

        let mut redis = test_redis().await;
        let _: () = redis.expire(&code, 3).await.unwrap();

        for _ in 0..50 {
            assert_eq!(
                store.resolve(&code).await.unwrap(),
                Some("https://keepalive.example".to_string())
            );
        }

        let ttl: i64 = redis.ttl(&code).await.unwrap();
        assert!(
            ttl > 3 && ttl <= TTL_EXTENSION_SECONDS,
            "expected TTL extended to ~{TTL_EXTENSION_SECONDS}s, got {ttl}"
        );
        assert_eq!(
            store.postgres_query_count(),
            0,
            "a keep-alive must never touch Postgres"
        );
    }

    /// The keep-alive's negative case: at ~300s remaining the per-read chance
    /// is e^-60, so the TTL must not be reset down to the extension.
    #[tokio::test]
    #[ignore]
    async fn reads_far_from_expiry_do_not_extend_ttl() {
        let store = test_store().await;
        let code = store.shorten("https://fresh.example".into()).await.unwrap();

        for _ in 0..50 {
            store.resolve(&code).await.unwrap();
        }

        let mut redis = test_redis().await;
        let ttl: i64 = redis.ttl(&code).await.unwrap();
        assert!(
            ttl > TTL_EXTENSION_SECONDS,
            "TTL should still be near the full base value, got {ttl}"
        );
    }
}
