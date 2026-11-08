//! The cache-aside store: Redis in front of Tier 3's Postgres `links` table. See
//! this tier's README, including for the races it leaves open on purpose.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redis::AsyncCommands;
use sqlx::PgPool;

use crate::base62;

/// How long a cached mapping lives, which is also the longest a wrong value can
/// survive. Shorter means less staleness and more Postgres queries. 5 minutes is
/// the value AWS uses in its lazy-loading example. Every tier uses the same one.
const CACHE_TTL_SECONDS: u64 = 300;

fn log_redis_error(e: impl std::fmt::Display) {
    eprintln!("redis failed: {e:#}");
}

pub struct Store {
    pool: PgPool,
    /// Cheap to clone per use: every clone shares one connection, which
    /// reconnects by itself if Redis restarts.
    redis: redis::aio::ConnectionManager,
    /// How many times `resolve` has queried Postgres, for `/metrics`. This is
    /// the exact thing a stampede spikes.
    postgres_queries: AtomicU64,
    /// From `DEMO_QUERY_DELAY_MS`: a real local read is too fast for concurrent
    /// requests to land between its `SELECT` and `SET`, so this widens that gap
    /// for `../thundering-herd-demo/` and the stale-set test.
    demo_query_delay: Duration,
}

impl Store {
    /// Connect to Postgres (running pending migrations) and Redis.
    pub async fn connect(database_url: &str, redis_url: &str) -> anyhow::Result<Self> {
        let pool = PgPool::connect(database_url).await?;
        sqlx::migrate!().run(&pool).await?;

        let redis = redis::Client::open(redis_url)?
            .get_connection_manager_with_config(
                // Retrying inside a request would stall it for seconds while Redis
                // is down. Fail fast instead: the next command reconnects.
                redis::aio::ConnectionManagerConfig::new().set_number_of_retries(1),
            )
            .await?;

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

    /// Store a long URL and return the freshly minted short code. It reaches the
    /// cache on its first read.
    pub async fn shorten(&self, url: String) -> anyhow::Result<String> {
        let id: i64 = sqlx::query_scalar("SELECT nextval('link_ids')")
            .fetch_one(&self.pool)
            .await?;
        let code = base62::encode(id as u64);

        sqlx::query("INSERT INTO links (code, long_url) VALUES ($1, $2)")
            .bind(&code)
            .bind(&url)
            .execute(&self.pool)
            .await?;

        Ok(code)
    }

    /// Change a code's destination. `false` means no such code.
    pub async fn update(&self, code: &str, url: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE links SET long_url = $2 WHERE code = $1")
            .bind(code)
            .bind(url)
            .execute(&self.pool)
            .await?;
        self.invalidate(code).await;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a code entirely. `false` means no such code.
    pub async fn delete(&self, code: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM links WHERE code = $1")
            .bind(code)
            .execute(&self.pool)
            .await?;
        self.invalidate(code).await;
        Ok(result.rows_affected() > 0)
    }

    /// Runs after the database write has committed, so failing here can't undo
    /// it. The value stays stale until its TTL runs out.
    async fn invalidate(&self, code: &str) {
        if let Err(e) = self.redis.clone().del::<_, ()>(code).await {
            log_redis_error(e);
        }
    }

    /// Look up the long URL for a code. If Redis fails, that counts as a miss,
    /// because Postgres can still answer.
    pub async fn resolve(&self, code: &str) -> anyhow::Result<Option<String>> {
        let cached = self
            .redis
            .clone()
            .get::<_, Option<String>>(code)
            .await
            .unwrap_or_else(|e| {
                log_redis_error(e);
                None
            });
        if cached.is_some() {
            return Ok(cached);
        }

        self.postgres_queries.fetch_add(1, Ordering::Relaxed);
        let url: Option<String> = sqlx::query_scalar(
            "SELECT long_url FROM links WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        tokio::time::sleep(self.demo_query_delay).await;

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
    use crate::config::Config;

    /// These tests need the services in `../docker-compose.yml` and are marked
    /// `#[ignore]` so `cargo test` stays usable without them. Run them with:
    /// `cargo test -p tier-4-1-caching -- --ignored --test-threads=1`
    async fn test_store() -> Store {
        let config = Config::from_env();
        Store::connect(&config.database_url, &config.redis_url)
            .await
            .expect("connect to test services")
    }

    async fn admin_redis() -> redis::aio::MultiplexedConnection {
        redis::Client::open(Config::from_env().redis_url)
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
    async fn unknown_code_resolves_to_none() {
        let store = test_store().await;
        assert_eq!(store.resolve("doesNotExist").await.unwrap(), None);
    }

    /// The whole point of the tier: only the first read reaches Postgres, and
    /// it caches the value with a TTL.
    #[tokio::test]
    #[ignore]
    async fn the_first_read_fills_the_cache() {
        let store = test_store().await;
        let code = store.shorten("https://a.example".into()).await.unwrap();

        for _ in 0..3 {
            assert_eq!(
                store.resolve(&code).await.unwrap().as_deref(),
                Some("https://a.example")
            );
        }

        assert_eq!(store.postgres_query_count(), 1);
        let ttl: i64 = admin_redis().await.ttl(&code).await.unwrap();
        assert!(ttl > 0 && ttl <= CACHE_TTL_SECONDS as i64, "ttl was {ttl}");
    }

    #[tokio::test]
    #[ignore]
    async fn writes_invalidate_the_cache() {
        let store = test_store().await;
        let code = store.shorten("https://old.example".into()).await.unwrap();
        store.resolve(&code).await.unwrap();

        assert!(store.update(&code, "https://new.example").await.unwrap());
        assert_eq!(
            store.resolve(&code).await.unwrap().as_deref(),
            Some("https://new.example")
        );

        assert!(store.delete(&code).await.unwrap());
        assert_eq!(store.resolve(&code).await.unwrap(), None);
        assert!(!store.delete(&code).await.unwrap());
    }

    /// Race 2 in `docs/race-conditions.md`, left open on purpose: a read that
    /// started before a write caches the old value after the write's `DEL`. Only
    /// the TTL fixes it.
    #[tokio::test]
    #[ignore]
    async fn a_slow_read_can_cache_a_stale_value() {
        let mut store = test_store().await;
        store.demo_query_delay = Duration::from_millis(300);
        let code = store.shorten("https://old.example".into()).await.unwrap();

        let (read, _) = tokio::join!(store.resolve(&code), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            store.update(&code, "https://new.example").await.unwrap()
        });
        assert_eq!(read.unwrap().as_deref(), Some("https://old.example"));

        assert_eq!(
            store.resolve(&code).await.unwrap().as_deref(),
            Some("https://old.example"),
            "Postgres has the new URL, but the cache kept the old one"
        );
        let ttl: i64 = admin_redis().await.ttl(&code).await.unwrap();
        assert!(ttl > 0 && ttl <= CACHE_TTL_SECONDS as i64, "ttl was {ttl}");
    }

    /// A dropped connection must heal by itself: restarting Redis shouldn't
    /// mean restarting the app too. The command in flight may fail, so retry.
    #[tokio::test]
    #[ignore]
    async fn recovers_after_the_connection_drops() {
        let store = test_store().await;
        let code = store.shorten("https://back.example".into()).await.unwrap();
        store.resolve(&code).await.unwrap();

        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("TYPE")
            .arg("normal")
            .arg("SKIPME")
            .arg("yes")
            .query_async(&mut admin_redis().await)
            .await
            .unwrap();
        assert!(killed > 0);

        store.resolve(&code).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _: () = store.redis.clone().del(&code).await.unwrap();
    }

    /// A failing cache must cost speed, not availability. Revoking commands from
    /// the app's user makes them fail on demand; `admin` keeps the power to
    /// restore them.
    #[tokio::test]
    #[ignore]
    async fn reads_and_writes_survive_redis_failures() {
        let store = test_store().await;
        let code = store.shorten("https://db.example".into()).await.unwrap();

        let mut admin = admin_redis().await;
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
        let acl = |rule: &'static str| {
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
        };

        for (revoke, restore) in [("-@all", "+@all"), ("-set", "+set")] {
            acl(revoke).await;
            let resolved = store.resolve(&code).await;
            let updated = store.update(&code, "https://db.example").await;
            acl(restore).await;
            assert_eq!(
                resolved.unwrap().as_deref(),
                Some("https://db.example"),
                "with {revoke}"
            );
            assert!(updated.unwrap(), "with {revoke}");
        }
    }
}
