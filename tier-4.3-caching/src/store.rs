//! The cache-aside store: Redis in front of Tier 3's Postgres `links` table. A
//! miss leaves a note in the key, so one reader rebuilds it and a write's `DEL`
//! cancels that rebuild. See this tier's README.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rand::Rng;
use redis::AsyncCommands;
use sqlx::PgPool;

use crate::base62;
use crate::bloom::Bloom;
use crate::config::Config;

/// How long a cached mapping lives, which is also the longest a wrong value can
/// survive. Shorter means less staleness and more Postgres queries. 5 minutes is
/// the value AWS uses in its lazy-loading example. Every tier uses the same one.
const CACHE_TTL_SECONDS: u64 = 300;
/// Spreads out expiries, so keys cached together don't all miss together.
const CACHE_TTL_JITTER_SECONDS: u64 = 10;

/// A note marks a key someone is rebuilding. Postgres `TEXT` can't hold a NUL
/// byte, so no URL starts with one.
const NOTE_PREFIX: char = '\0';
/// Long enough for one `SELECT`. A rebuilder that crashes frees the key after this.
const NOTE_TTL_MS: u64 = 2_000;
/// How often a reader that found a note checks again, and when it stops waiting.
const WAIT_STEP: Duration = Duration::from_millis(50);
const WAIT_LIMIT: Duration = Duration::from_millis(200);

fn jittered_ttl() -> u64 {
    CACHE_TTL_SECONDS + rand::thread_rng().gen_range(0..=CACHE_TTL_JITTER_SECONDS)
}

fn new_note() -> String {
    format!("{NOTE_PREFIX}{:x}", rand::random::<u64>())
}

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
    /// The filter goes first: a code in it whose `INSERT` fails is a harmless false
    /// positive, while a committed row missing from it could 404.
    pub async fn shorten(&self, url: String) -> anyhow::Result<String> {
        let id: i64 = sqlx::query_scalar("SELECT nextval('link_ids')")
            .fetch_one(&self.pool)
            .await?;
        let code = base62::encode(id as u64);

        self.bloom.add(&code).await?;

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
    /// it. The value stays stale until its TTL runs out. If the key holds a note,
    /// this also cancels that rebuild: its `SET ... IFEQ` will find nothing.
    async fn invalidate(&self, code: &str) {
        if let Err(e) = self.redis.clone().del::<_, ()>(code).await {
            log_redis_error(e);
        }
    }

    /// Look up the long URL for a code. If Redis fails, read Postgres without
    /// caching, because Postgres can still answer.
    ///
    /// On a miss, the first reader to `SET` a note rebuilds the key. Readers that
    /// find a note wait for the value, and after `WAIT_LIMIT` read Postgres
    /// without caching.
    pub async fn resolve(&self, code: &str) -> anyhow::Result<Option<String>> {
        if !base62::is_valid(code) {
            return Ok(None);
        }

        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            let cached = match self.redis.clone().get::<_, Option<String>>(code).await {
                Ok(cached) => cached,
                Err(e) => {
                    log_redis_error(e);
                    return self.read_postgres(code).await;
                }
            };
            match cached {
                Some(note) if note.starts_with(NOTE_PREFIX) => {
                    if Instant::now() >= deadline {
                        return self.read_postgres(code).await;
                    }
                    tokio::time::sleep(WAIT_STEP).await;
                    continue;
                }
                Some(url) => return Ok(Some(url)),
                None => {}
            }

            // A failed check means "maybe": only Postgres can say a code doesn't exist.
            match self.bloom.might_exist(code).await {
                Ok(false) => return Ok(None),
                Ok(true) => {}
                Err(e) => log_redis_error(e),
            }

            let note = new_note();
            let won: Result<Option<String>, _> = redis::cmd("SET")
                .arg(code)
                .arg(&note)
                .arg("NX")
                .arg("PX")
                .arg(NOTE_TTL_MS)
                .query_async(&mut self.redis.clone())
                .await;
            match won {
                Ok(Some(_)) => return self.rebuild(code, &note).await,
                // Another reader left its note first: go back and wait for it.
                Ok(None) => continue,
                Err(e) => {
                    log_redis_error(e);
                    return self.read_postgres(code).await;
                }
            }
        }
    }

    /// Fill a key this reader holds the note for. Each write only lands if the
    /// note is still there: a `DEL` in between means the value read is stale.
    async fn rebuild(&self, code: &str, note: &str) -> anyhow::Result<Option<String>> {
        let url = self.read_postgres(code).await;
        let cmd = match &url {
            Ok(Some(url)) => {
                let mut cmd = redis::cmd("SET");
                cmd.arg(code)
                    .arg(url)
                    .arg("IFEQ")
                    .arg(note)
                    .arg("EX")
                    .arg(jittered_ttl());
                cmd
            }
            // Free the key now instead of making readers wait out the note.
            _ => {
                let mut cmd = redis::cmd("DELEX");
                cmd.arg(code).arg("IFEQ").arg(note);
                cmd
            }
        };
        if let Err(e) = cmd.query_async::<()>(&mut self.redis.clone()).await {
            log_redis_error(e);
        }
        url
    }

    async fn read_postgres(&self, code: &str) -> anyhow::Result<Option<String>> {
        self.postgres_queries.fetch_add(1, Ordering::Relaxed);
        let url = sqlx::query_scalar(
            "SELECT long_url FROM links WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        tokio::time::sleep(self.demo_query_delay).await;
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
        let max = (CACHE_TTL_SECONDS + CACHE_TTL_JITTER_SECONDS) as i64;
        assert!(ttl > 0 && ttl <= max, "ttl was {ttl}");
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

    /// Race 2 in `docs/race-conditions.md`, closed: the write's `DEL` removes
    /// the slow reader's note, so its `SET ... IFEQ` is refused.
    #[tokio::test]
    #[ignore]
    async fn a_slow_read_cannot_cache_a_stale_value() {
        let mut store = test_store().await;
        store.demo_query_delay = Duration::from_millis(300);
        let code = store.shorten("https://old.example".into()).await.unwrap();

        let (read, _) = tokio::join!(store.resolve(&code), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            store.update(&code, "https://new.example").await.unwrap()
        });
        assert_eq!(read.unwrap().as_deref(), Some("https://old.example"));

        let cached: Option<String> = admin_redis().await.get(&code).await.unwrap();
        assert_eq!(cached, None, "the stale fill must be refused");
        assert_eq!(
            store.resolve(&code).await.unwrap().as_deref(),
            Some("https://new.example")
        );
    }

    /// Any interleaving of reads and writes, as long as every Redis command
    /// succeeds, leaves the cache empty or equal to Postgres. Only a lost `DEL`
    /// can leave it stale.
    #[tokio::test]
    #[ignore]
    async fn mixed_reads_and_writes_never_leave_a_stale_value() {
        let mut store = test_store().await;
        store.demo_query_delay = Duration::from_millis(20);
        let store = std::sync::Arc::new(store);
        let code = store.shorten("https://v0.example".into()).await.unwrap();

        for round in 1..=50 {
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..3 {
                let (store, code) = (store.clone(), code.clone());
                tasks.spawn(async move {
                    store.resolve(&code).await.unwrap();
                });
            }
            let (store2, code2) = (store.clone(), code.clone());
            tasks.spawn(async move {
                let wait = (round * 7) % 30;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                let url = format!("https://v{round}.example");
                store2.update(&code2, &url).await.unwrap();
            });
            tasks.join_all().await;

            let cached: Option<String> = admin_redis().await.get(&code).await.unwrap();
            let current = format!("https://v{round}.example");
            assert!(
                cached.is_none() || cached.as_deref() == Some(current.as_str()),
                "round {round}: cache has {cached:?}, Postgres has {current}"
            );
        }
    }

    /// The stampede: many readers miss one key at once, and only one of them
    /// queries Postgres.
    #[tokio::test]
    #[ignore]
    async fn concurrent_misses_query_postgres_once() {
        let mut store = test_store().await;
        store.demo_query_delay = Duration::from_millis(100);
        let store = std::sync::Arc::new(store);
        let code = store.shorten("https://hot.example".into()).await.unwrap();

        let mut readers = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let (store, code) = (store.clone(), code.clone());
            readers.spawn(async move { store.resolve(&code).await.unwrap() });
        }
        for url in readers.join_all().await {
            assert_eq!(url.as_deref(), Some("https://hot.example"));
        }
        assert_eq!(store.postgres_query_count(), 1);
    }

    /// A rebuilder that dies leaves its note behind. Readers don't wait on it
    /// forever, and once it expires the next reader rebuilds.
    #[tokio::test]
    #[ignore]
    async fn readers_get_past_an_abandoned_note() {
        let store = test_store().await;
        let code = store.shorten("https://a.example".into()).await.unwrap();
        let mut admin = admin_redis().await;
        let _: () = admin.pset_ex(&code, new_note(), NOTE_TTL_MS).await.unwrap();

        let started = Instant::now();
        assert_eq!(
            store.resolve(&code).await.unwrap().as_deref(),
            Some("https://a.example")
        );
        assert!(started.elapsed() < WAIT_LIMIT + WAIT_STEP * 2);
        let cached: String = admin.get(&code).await.unwrap();
        assert!(cached.starts_with(NOTE_PREFIX), "a waiter mustn't cache");

        let _: () = admin.pexpire(&code, 1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        store.resolve(&code).await.unwrap();
        let cached: String = admin.get(&code).await.unwrap();
        assert_eq!(cached, "https://a.example");
    }

    /// A deleted code frees its key right away, instead of leaving a note
    /// readers would wait on.
    #[tokio::test]
    #[ignore]
    async fn a_missing_row_frees_the_key() {
        let store = test_store().await;
        let code = store.shorten("https://gone.example".into()).await.unwrap();
        store.delete(&code).await.unwrap();

        assert_eq!(store.resolve(&code).await.unwrap(), None);
        let exists: bool = admin_redis().await.exists(&code).await.unwrap();
        assert!(!exists);
    }

    #[test]
    fn ttls_include_jitter() {
        let ttls: std::collections::HashSet<u64> = (0..1000).map(|_| jittered_ttl()).collect();
        assert!(ttls.len() > 1);
        assert!(ttls.iter().all(|t| {
            (CACHE_TTL_SECONDS..=CACHE_TTL_SECONDS + CACHE_TTL_JITTER_SECONDS).contains(t)
        }));
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

        for (revoke, restore) in [
            ("-@all", "+@all"),
            ("-bf.exists", "+bf.exists"),
            ("-set", "+set"),
        ] {
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

    /// Keys that aren't codes mustn't be read back as if they were.
    #[tokio::test]
    #[ignore]
    async fn redis_keys_are_not_resolvable() {
        let store = test_store().await;
        assert_eq!(store.resolve("codes_bloom").await.unwrap(), None);
        assert_eq!(store.resolve("bloom:ready").await.unwrap(), None);
    }
}
