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
//! # Quarter 3: PATCH/DELETE, and a lease against the thundering herd
//!
//! `update` and `delete` both write Postgres first (same durability
//! discipline as `shorten`), then invalidate with a plain Redis `DEL` --
//! deliberately the simple, stampede-prone approach from the README, since
//! it's the concrete trigger `resolve`'s lease exists to defend against.
//!
//! A deleted code is never removed from the Bloom filter -- standard Bloom
//! filters have no removal operation. That's harmless, not a bug: `resolve`
//! still falls through to Postgres and correctly gets `None`; the code just
//! permanently loses the filter's fast-path benefit.
//!
//! # Quarter 4: singleflight, a TTL keep-alive, and TTL jitter
//!
//! Three more mitigations layered onto the lease, each addressing a
//! different angle of the same failure:
//!
//! - **Singleflight** (`inflight`): the lease still costs a Redis round trip
//!   per follower to discover they lost the race. Within one process,
//!   concurrent `resolve` misses for the *same* code now collapse into a
//!   single call to `resolve_miss` via a shared `tokio::sync::OnceCell` --
//!   followers await that one call's result directly, never touching Redis
//!   at all. This only coordinates within one process; multiple app
//!   instances still rely on the lease to coordinate with each other.
//! - **TTL keep-alive** (`should_extend_ttl`): a cache *hit* also reads the
//!   key's remaining TTL and, with a probability that rises as expiry
//!   approaches, extends it by a small fixed amount (`EXPIRE`) -- no
//!   Postgres involved. The randomness isn't there to spread out an
//!   expensive operation (`EXPIRE` is free and idempotent); it's a
//!   *hotness filter*: only a key with enough reads inside the window
//!   collects enough rolls for one to land, so a key has to earn its
//!   extension with sustained traffic. Hot keys therefore never reach a
//!   hard expiry and the synchronized miss that comes with it; cold keys
//!   expire normally. This is what the textbook XFetch collapses into once
//!   the recompute is cheap and invalidation is explicit -- see the README
//!   for why a real recompute buys nothing here.
//! - **TTL jitter** (`jittered_ttl`): every `SET ... EX` gets a small random
//!   addition on top of the base TTL, so keys written around the same time
//!   don't also *expire* around the same time -- a different stampede
//!   shape than one hot key's lease: many different keys going cold in
//!   unison.
//!
//! The keep-alive is only sound because `update`/`delete` invalidate with
//! an explicit `DEL`: a cached value is identical to Postgres for as long
//! as it lives, so extending its life never serves anything stale. If
//! anything wrote Postgres *outside* this app, a kept-alive hot key would
//! never notice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use rand::Rng;
use redis::AsyncCommands;
use redis::{ExistenceCheck, SetExpiry, SetOptions};
use sqlx::{PgPool, Row};
use tokio::sync::OnceCell;

use crate::base62;

/// Base TTL for a cached mapping, before jitter. Arbitrary for a teaching
/// build; a real deployment would tune this against how often `PATCH`/
/// `DELETE` make a cached value stale.
const CACHE_TTL_SECONDS: u64 = 300;

/// Upper bound on the random addition to `CACHE_TTL_SECONDS`. Every write
/// gets `CACHE_TTL_SECONDS + [0, CACHE_TTL_JITTER_SECONDS)`, so keys minted
/// close together don't also expire close together.
const CACHE_TTL_JITTER_SECONDS: u64 = 30;

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

/// How close to expiry a key has to be before reads start rolling for an
/// extension. The per-read chance is `e^(-remaining / window)`: negligible
/// while `remaining` is many windows away, ~37% at exactly one window out,
/// climbing toward certainty as `remaining` approaches zero. Wider window =
/// extensions start earlier and a key needs less traffic to earn one.
const TTL_EXTEND_WINDOW_SECONDS: f64 = 5.0;

/// What a successful roll sets the remaining TTL *to* (not adds -- `EXPIRE`
/// fixes the remaining time, it doesn't accumulate). Deliberately much
/// shorter than `CACHE_TTL_SECONDS`: a single lucky read buys a hot key
/// this much more life, not a whole fresh lifetime, so a key's survival
/// tracks its recent traffic closely. The cost: a hot key whose traffic
/// pauses for longer than this expires, and its next burst is a (lease-
/// guarded) miss.
const TTL_EXTENSION_SECONDS: u64 = 30;

/// The keep-alive's roll: extend if `window * -ln(rand)` has already
/// exceeded the time remaining until expiry. `rand` is sampled from
/// `(0, 1)` (never exactly 0, which would make `-ln` infinite), so
/// `-ln(rand)` is exponentially distributed with mean 1 -- which makes the
/// per-read chance `e^(-remaining / window)`: effectively zero far from
/// expiry, climbing sharply as `remaining_ttl_secs` shrinks. See
/// `TTL_EXTEND_WINDOW_SECONDS`.
fn should_extend_ttl(remaining_ttl_secs: f64) -> bool {
    let r: f64 = rand::thread_rng().gen_range(f64::EPSILON..1.0);
    TTL_EXTEND_WINDOW_SECONDS * (-r.ln()) >= remaining_ttl_secs
}

/// A cell shared by every same-process caller coalesced onto one
/// `resolve_miss` call for a given code. The error side of the `Result`
/// (`OnceCell::get_or_try_init`'s `E`) is `Arc<anyhow::Error>`, not
/// `anyhow::Error` directly, because `anyhow::Error` isn't `Clone` and every
/// waiter needs its own copy of the same outcome.
type InflightCell = OnceCell<Option<String>>;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    redis: redis::aio::MultiplexedConnection,
    /// How many times `resolve` has actually queried Postgres -- i.e. how
    /// many times a request became the lease's *leader*. For the
    /// thundering-herd demo's `/metrics` endpoint: this is exactly the
    /// count the lease is supposed to keep near 1 per stampede, versus
    /// Tier 4.1's version where it tracks concurrent request count. `Arc`
    /// so every `Store::clone()` (handlers and tests that `tokio::spawn`
    /// concurrent work) shares the same counter rather than starting a new
    /// one at zero.
    postgres_queries: Arc<AtomicU64>,
    /// See Tier 4.1's `Store` for why this exists: simulates a realistically
    /// slow backend query so a local demo's miss window is wide enough for
    /// concurrent requests to actually land inside it.
    demo_query_delay: Duration,
    /// Singleflight: codes currently being rebuilt by *this* process. See
    /// the module doc's Quarter 4 section.
    inflight: Arc<StdMutex<HashMap<String, Arc<InflightCell>>>>,
}

/// A generic dump of a table's rows, column names discovered at query time
/// rather than hardcoded — so `/stats` keeps working as the schema evolves.
pub struct TableDump {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Store {
    /// Connect to Postgres (running pending migrations) and Redis, and make
    /// sure the codes Bloom filter exists.
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

        let demo_query_delay = Duration::from_millis(
            std::env::var("DEMO_QUERY_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        );

        Ok(Self {
            pool,
            redis,
            postgres_queries: Arc::new(AtomicU64::new(0)),
            demo_query_delay,
            inflight: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    /// Total Postgres queries `resolve` has issued since this process
    /// started. Used only by the `/metrics` endpoint.
    pub fn postgres_query_count(&self) -> u64 {
        self.postgres_queries.load(Ordering::Relaxed)
    }

    /// `CACHE_TTL_SECONDS` plus a random addition -- see the module doc's
    /// TTL jitter note.
    fn jittered_ttl(&self) -> u64 {
        CACHE_TTL_SECONDS + rand::thread_rng().gen_range(0..CACHE_TTL_JITTER_SECONDS)
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
    /// Postgres is written *before* Redis, deliberately: writing the cache
    /// first would make a code externally resolvable before its row is
    /// durably committed, so a failed `INSERT` after a successful `SET`
    /// would leave Redis pointing at a URL Postgres never actually has.
    pub async fn shorten(&self, url: String) -> Result<String, anyhow::Error> {
        let (id,): (i64,) = sqlx::query_as("SELECT nextval('link_ids')")
            .fetch_one(&self.pool)
            .await?;
        let code = base62::encode(id as u64);

        sqlx::query("INSERT INTO links (code, long_url) VALUES ($1, $2)")
            .bind(&code)
            .bind(&url)
            .execute(&self.pool)
            .await?;

        self.bloom_add(&code).await?;

        let mut redis = self.redis.clone();
        redis
            .set_ex::<_, _, ()>(&code, &url, self.jittered_ttl())
            .await?;

        Ok(code)
    }

    /// Change a code's destination. Returns `false` if the code doesn't
    /// exist (caller turns that into a 404).
    ///
    /// Postgres first, same reasoning as `shorten`: only invalidate the
    /// cache once the new value is durably committed. Invalidation here is
    /// a plain `DEL`, not a write-through `SET` -- see the module doc for
    /// why that's the point, not an oversight.
    pub async fn update(&self, code: &str, url: &str) -> Result<bool, anyhow::Error> {
        let result = sqlx::query("UPDATE links SET long_url = $2 WHERE code = $1")
            .bind(code)
            .bind(url)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Ok(false);
        }

        let mut redis = self.redis.clone();
        redis.del::<_, ()>(code).await?;

        Ok(true)
    }

    /// Remove a code entirely. Returns `false` if it didn't exist.
    ///
    /// Without the `DEL` here, a deleted link would keep redirecting
    /// successfully -- serving a cached URL for a row that no longer
    /// exists -- until its TTL happened to lapse on its own.
    pub async fn delete(&self, code: &str) -> Result<bool, anyhow::Error> {
        let result = sqlx::query("DELETE FROM links WHERE code = $1")
            .bind(code)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Ok(false);
        }

        let mut redis = self.redis.clone();
        redis.del::<_, ()>(code).await?;

        Ok(true)
    }

    /// Look up the long URL for a code.
    ///
    /// A hit also runs the keep-alive: alongside the value, `resolve` pulls
    /// the key's remaining TTL (one pipelined, atomic round trip, not two),
    /// and if `should_extend_ttl` rolls true, pushes the expiry out by
    /// `TTL_EXTENSION_SECONDS` inline -- a single `EXPIRE`, cheap enough
    /// that there's nothing to background. A miss falls through to the
    /// Bloom filter check and then the lease-guarded, singleflight-coalesced
    /// rebuild in `resolve_miss`.
    pub async fn resolve(&self, code: &str) -> Result<Option<String>, anyhow::Error> {
        let mut redis = self.redis.clone();

        let (value, ttl): (Option<String>, i64) = redis::pipe()
            .atomic()
            .get(code)
            .ttl(code)
            .query_async(&mut redis)
            .await?;

        if let Some(url) = value {
            if ttl > 0 && should_extend_ttl(ttl as f64) {
                redis
                    .expire::<_, ()>(code, TTL_EXTENSION_SECONDS as i64)
                    .await?;
            }
            return Ok(Some(url));
        }

        if !self.bloom_might_exist(code).await? {
            return Ok(None);
        }

        self.resolve_miss_coalesced(code).await
    }

    /// Singleflight: collapse every concurrent same-process call for this
    /// code onto a single `resolve_miss`, so followers never touch Redis's
    /// lease at all -- they just await the same in-memory result the first
    /// caller is already producing.
    async fn resolve_miss_coalesced(&self, code: &str) -> Result<Option<String>, anyhow::Error> {
        let cell = {
            let mut inflight = self.inflight.lock().expect("inflight mutex poisoned");
            inflight
                .entry(code.to_string())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let outcome = cell
            .get_or_try_init(|| async { self.resolve_miss(code).await.map_err(Arc::new) })
            .await
            .map(Clone::clone);

        // Remove our entry so the *next* stampede on this code (after this
        // one settles) gets a fresh cell -- a `OnceCell` can only ever
        // initialize once, so reusing this one would permanently freeze the
        // code's cached value at whatever this call produced.
        self.inflight
            .lock()
            .expect("inflight mutex poisoned")
            .remove(code);

        outcome.map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// Rebuild a code's cache entry: Tier 4.3's lease loop, unchanged in
    /// substance. Reached only through `resolve_miss_coalesced` now, so at
    /// most one task per process is ever inside this function for a given
    /// code at a time -- everything below it (the Redis lease) is what
    /// coordinates *across* processes, on top of that.
    async fn resolve_miss(&self, code: &str) -> Result<Option<String>, anyhow::Error> {
        let mut redis = self.redis.clone();
        let lease_key = format!("lease:{code}");
        let lease_options = SetOptions::default()
            .conditional_set(ExistenceCheck::NX)
            .with_expiration(SetExpiry::EX(LEASE_TTL_SECONDS));

        loop {
            if let Some(url) = redis.get::<_, Option<String>>(code).await? {
                return Ok(Some(url));
            }

            let acquired: Option<String> =
                redis.set_options(&lease_key, 1, lease_options.clone()).await?;

            if acquired.is_none() {
                tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
                continue;
            }

            // We're the leader now: query Postgres and either populate the
            // cache or, on a real miss, release the lease immediately
            // rather than making everyone else wait out its full TTL for a
            // question that's already been definitively answered.
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

            return match row {
                Some((url,)) => {
                    redis
                        .set_ex::<_, _, ()>(code, &url, self.jittered_ttl())
                        .await?;
                    redis.del::<_, ()>(&lease_key).await?;
                    Ok(Some(url))
                }
                None => {
                    redis.del::<_, ()>(&lease_key).await?;
                    Ok(None)
                }
            };
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
    /// `cargo test -p tier-4-4-caching -- --ignored`
    async fn test_store() -> Store {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://shortener:shortener@localhost:5437/shortener".into());
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6383".into());
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

    /// Quarter 4's property: many concurrent same-process misses for the
    /// same code collapse into one Postgres query, via singleflight, not
    /// just "eventually converge" via the lease's retry loop.
    #[tokio::test]
    #[ignore]
    async fn concurrent_misses_singleflight_to_one_query() {
        let store = test_store().await;
        let code = store
            .shorten("https://singleflight.example".into())
            .await
            .unwrap();

        let mut redis = test_redis().await;
        let _: () = redis.del(&code).await.unwrap();

        let before = store.postgres_query_count();
        let mut handles = Vec::new();
        for _ in 0..20 {
            let store = store.clone();
            let code = code.clone();
            handles.push(tokio::spawn(async move { store.resolve(&code).await }));
        }
        for h in handles {
            assert_eq!(
                h.await.unwrap().unwrap(),
                Some("https://singleflight.example".to_string())
            );
        }
        let after = store.postgres_query_count();

        assert_eq!(after - before, 1, "singleflight should collapse all 20 misses into 1 query");
    }

    /// The keep-alive's positive case: a key inside the extension window
    /// that keeps getting read has its expiry pushed out -- without any
    /// Postgres query. 50 reads at 3s remaining (window 5s => ~55% per read)
    /// leaves a ~1e-17 chance of no extension, so this is deterministic in
    /// practice.
    #[tokio::test]
    #[ignore]
    async fn sustained_reads_near_expiry_extend_ttl() {
        let store = test_store().await;
        let code = store.shorten("https://keepalive.example".into()).await.unwrap();

        let mut redis = test_redis().await;
        let _: () = redis.expire(&code, 3).await.unwrap();

        let before = store.postgres_query_count();
        for _ in 0..50 {
            assert_eq!(
                store.resolve(&code).await.unwrap(),
                Some("https://keepalive.example".to_string())
            );
        }

        let ttl: i64 = redis.ttl(&code).await.unwrap();
        assert!(
            ttl > 3 && ttl <= TTL_EXTENSION_SECONDS as i64,
            "expected TTL extended to ~{TTL_EXTENSION_SECONDS}s, got {ttl}"
        );
        assert_eq!(
            store.postgres_query_count() - before,
            0,
            "a keep-alive must never touch Postgres"
        );
    }

    /// The keep-alive's negative case: reads far from expiry don't extend
    /// anything. At ~300s remaining with a 5s window the per-read chance is
    /// e^-60 -- the TTL must still be its original (jittered) value, not
    /// reset down to `TTL_EXTENSION_SECONDS`.
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
            ttl > TTL_EXTENSION_SECONDS as i64,
            "TTL should still be near the full base value, got {ttl}"
        );
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
}
