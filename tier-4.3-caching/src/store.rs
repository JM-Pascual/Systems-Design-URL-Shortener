//! Tier 4.2's store plus `PATCH`/`DELETE` invalidation, and a lease that
//! serializes every write to a code's cache entry. See this tier's README.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redis::AsyncCommands;
use sqlx::postgres::PgArguments;
use sqlx::query::Query;
use sqlx::{PgPool, Postgres};

use crate::base62;
use crate::bloom::Bloom;
use crate::config::Config;

/// How long a cached mapping lives. Arbitrary for a teaching build; a real
/// deployment would tune it against how often links change.
const CACHE_TTL_SECONDS: u64 = 300;

/// Upper bound on how long a crashed lease holder can block everyone else.
const LEASE_TTL_SECONDS: u64 = 5;

/// How long a request waits before trying for the lease again.
const LEASE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

fn lease_key(code: &str) -> String {
    format!("lease:{code}")
}

/// shortcut: logs every failed request, so an outage is noisy. The gate tier's
/// breaker can replace this with a single state change.
fn log_redis_error(e: impl std::fmt::Display) {
    eprintln!("redis failed, using Postgres: {e:#}");
}

pub struct Store {
    pool: PgPool,
    /// Cheap to clone per use: every clone shares one connection, which
    /// reconnects by itself if Redis restarts.
    redis: redis::aio::ConnectionManager,
    /// How many times `resolve` has queried Postgres, for `/metrics`. This is
    /// the exact thing a stampede spikes.
    postgres_queries: AtomicU64,
    /// From `DEMO_QUERY_DELAY_MS`: a real local query is too fast for a demo's
    /// concurrent requests to land inside the miss window, so
    /// `../thundering-herd-demo/` simulates a slow one.
    demo_query_delay: Duration,
    pub bloom: Bloom,
}

impl Store {
    /// Connect to Postgres (running pending migrations) and Redis. The Bloom
    /// filter fills in the background: until it's ready, `resolve` skips it.
    pub async fn connect(config: &Config) -> anyhow::Result<Self> {
        let pool = PgPool::connect(&config.database_url).await?;
        sqlx::migrate!().run(&pool).await?;

        let redis = redis::Client::open(config.redis_url.as_str())?
            .get_connection_manager_with_config(
                // Retrying inside a request would stall it for seconds while Redis
                // is down. Fail fast instead: the next command reconnects.
                redis::aio::ConnectionManagerConfig::new().set_number_of_retries(1),
            )
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
            .set_ex::<_, _, ()>(&code, &url, CACHE_TTL_SECONDS)
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

    /// Look up the long URL for a code: Redis first, then the Bloom filter,
    /// then the lease-guarded rebuild.
    pub async fn resolve(&self, code: &str) -> anyhow::Result<Option<String>> {
        if !base62::is_valid(code) {
            return Ok(None);
        }

        match self.redis.clone().get::<_, Option<String>>(code).await {
            Ok(Some(url)) => return Ok(Some(url)),
            Ok(None) => {}
            Err(e) => {
                log_redis_error(e);
                return self.read_postgres(code).await;
            }
        }

        match self.bloom.might_exist(code).await {
            Ok(false) => return Ok(None),
            Ok(true) => {}
            Err(e) => {
                log_redis_error(e);
                return self.read_postgres(code).await;
            }
        }

        self.resolve_miss(code).await
    }

    /// Whoever gets the lease rebuilds; everyone else sleeps and tries again,
    /// for the lease itself and not just the key. If the holder crashes, its
    /// lease expires and the next waiter takes over, so while Redis works no
    /// request queries Postgres unguarded and none is blocked forever.
    async fn resolve_miss(&self, code: &str) -> anyhow::Result<Option<String>> {
        let mut redis = self.redis.clone();

        loop {
            match redis.get::<_, Option<String>>(code).await {
                Ok(Some(url)) => return Ok(Some(url)),
                Ok(None) => {}
                Err(e) => {
                    log_redis_error(e);
                    return self.read_postgres(code).await;
                }
            }

            match self.try_acquire_lease(code).await {
                Ok(true) => {
                    // Released on every outcome, so a failed query doesn't stall
                    // every waiter for the full lease TTL. If the release fails,
                    // that TTL still ends it.
                    let outcome = self.rebuild(code).await;
                    if let Err(e) = redis.del::<_, ()>(lease_key(code)).await {
                        log_redis_error(e);
                    }
                    return outcome;
                }
                Ok(false) => tokio::time::sleep(LEASE_RETRY_INTERVAL).await,
                Err(e) => {
                    log_redis_error(e);
                    return self.read_postgres(code).await;
                }
            }
        }
    }

    /// Read Postgres without touching the cache. Used alone when Redis has failed:
    /// with no working lease to guard it, this path must not write a value back.
    async fn read_postgres(&self, code: &str) -> anyhow::Result<Option<String>> {
        self.postgres_queries.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.demo_query_delay).await;

        Ok(sqlx::query_scalar(
            "SELECT long_url FROM links WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Query Postgres and, on a hit, populate the cache.
    async fn rebuild(&self, code: &str) -> anyhow::Result<Option<String>> {
        let url = self.read_postgres(code).await?;

        if let Some(url) = &url
            && let Err(e) = self
                .redis
                .clone()
                .set_ex::<_, _, ()>(code, url, CACHE_TTL_SECONDS)
                .await
        {
            log_redis_error(e);
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
    /// `cargo test -p tier-4-3-caching -- --ignored --test-threads=1`
    async fn test_store() -> Store {
        Store::connect(&Config::from_env())
            .await
            .expect("connect to test services")
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

    /// A dropped connection must heal by itself: restarting Redis shouldn't
    /// mean restarting the app too. The command in flight may fail, so retry.
    #[tokio::test]
    #[ignore]
    async fn recovers_after_the_connection_drops() {
        let store = test_store().await;
        let code = store.shorten("https://back.example".into()).await.unwrap();

        let redis_url = crate::config::Config::from_env().redis_url;
        let mut admin = redis::Client::open(redis_url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("TYPE")
            .arg("normal")
            .arg("SKIPME")
            .arg("yes")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert!(killed > 0);

        let mut resolved = None;
        for _ in 0..50 {
            if let Ok(url) = store.resolve(&code).await {
                resolved = url;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(resolved.as_deref(), Some("https://back.example"));
    }

    /// A failing cache must cost speed, not availability. Covers a failed `GET`, filter check and lease.
    #[tokio::test]
    #[ignore]
    async fn reads_survive_redis_failures() {
        let store = test_store().await;
        let code = store.shorten("https://db.example".into()).await.unwrap();

        // Revoking commands from the app's user makes them fail on demand. This
        // separate user keeps the power to delete the key and restore them.
        let redis_url = crate::config::Config::from_env().redis_url;
        let mut admin = redis::Client::open(redis_url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg("admin")
            .arg("on")
            .arg("nopass")
            .arg("~*")
            .arg("+@all")
            .query_async(&mut admin)
            .await
            .unwrap();
        let _: () = redis::cmd("AUTH")
            .arg("admin")
            .arg("any")
            .query_async(&mut admin)
            .await
            .unwrap();

        let acl = {
            let admin = admin.clone();
            move |rule: &'static str| {
                let mut admin = admin.clone();
                async move {
                    let _: () = redis::cmd("ACL")
                        .arg("SETUSER")
                        .arg("default")
                        .arg(rule)
                        .query_async(&mut admin)
                        .await
                        .unwrap();
                }
            }
        };

        for (revoke, restore) in [
            ("-@all", "+@all"),
            ("-bf.exists", "+bf.exists"),
            ("-set", "+set"),
        ] {
            let _: () = admin.del(&code).await.unwrap();
            acl(revoke).await;
            let resolved = store.resolve(&code).await;
            acl(restore).await;
            assert_eq!(
                resolved.unwrap().as_deref(),
                Some("https://db.example"),
                "with {revoke}"
            );
        }
    }
}
