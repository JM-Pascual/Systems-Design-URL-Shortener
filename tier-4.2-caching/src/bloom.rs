//! The Bloom filter and the background job that keeps it trustworthy. A filter
//! may only answer "never minted" while it holds every code, so `bloom:ready`
//! marks when that's known to be true; without it, `resolve` asks Postgres. See
//! this tier's README.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use sqlx::PgPool;

use crate::config::Config;

/// The RedisBloom key holding every code that's ever been minted.
const FILTER_KEY: &str = "codes_bloom";

/// Exists only while the filter is known to hold every code. A flush removes it
/// along with the filter, which is what makes a wiped Redis safe.
const READY_KEY: &str = "bloom:ready";

const ERROR_RATE: f64 = 0.01;

/// Codes per page when adding from Postgres.
const PAGE_SIZE: i64 = 1_000;

const JOB_INTERVAL: Duration = Duration::from_secs(60);

/// Each job re-adds codes minted this recently, repairing any that a Redis
/// crash lost after acknowledging them. Also the overlap when catching up from
/// a snapshot, since `created_at` isn't commit order.
const OVERLAP: &str = "5 minutes";

const SNAPSHOT_MAX_AGE: &str = "1 day";

/// Serializes the job across app instances. The value is arbitrary.
const JOB_LOCK: i64 = 0x626c_6f6f;

const SNAPSHOT_PREFIX: &str = "bloom";

#[derive(Clone)]
pub struct Bloom {
    pool: PgPool,
    redis: redis::aio::MultiplexedConnection,
    snapshots: AmazonS3,
    capacity: i64,
}

impl Bloom {
    pub fn new(
        pool: PgPool,
        redis: redis::aio::MultiplexedConnection,
        config: &Config,
    ) -> anyhow::Result<Self> {
        // Building the client doesn't connect, so the app starts without the
        // object store and only snapshots suffer if it's down.
        let snapshots = AmazonS3Builder::new()
            .with_endpoint(&config.s3_endpoint)
            .with_allow_http(true)
            .with_bucket_name(&config.s3_bucket)
            .with_access_key_id(&config.s3_access_key)
            .with_secret_access_key(&config.s3_secret_key)
            .with_region("us-east-1")
            .build()?;

        Ok(Self {
            pool,
            redis,
            snapshots,
            capacity: config.bloom_capacity,
        })
    }

    /// Add a code. `BF.INSERT` recreates a missing filter at its real size,
    /// where `BF.ADD` would default to a capacity of 100.
    pub async fn add(&self, code: &str) -> anyhow::Result<()> {
        self.insert(&[code]).await
    }

    async fn insert<T: redis::ToRedisArgs>(&self, codes: &[T]) -> anyhow::Result<()> {
        redis::cmd("BF.INSERT")
            .arg(FILTER_KEY)
            .arg("CAPACITY")
            .arg(self.capacity)
            .arg("ERROR")
            .arg(ERROR_RATE)
            .arg("ITEMS")
            .arg(codes)
            .query_async::<()>(&mut self.redis.clone())
            .await?;
        Ok(())
    }

    /// `false` means the code was *never* minted, so Postgres can be skipped.
    /// Only a filter known to be complete can say that.
    pub async fn might_exist(&self, code: &str) -> anyhow::Result<bool> {
        let (ready, exists): (bool, bool) = redis::pipe()
            .atomic()
            .exists(READY_KEY)
            .cmd("BF.EXISTS")
            .arg(FILTER_KEY)
            .arg(code)
            .query_async(&mut self.redis.clone())
            .await?;
        Ok(!ready || exists)
    }

    /// Run the job forever. A failed run is retried on the next interval.
    pub async fn run(self) {
        let mut interval = tokio::time::interval(JOB_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(e) = self.job().await {
                eprintln!("bloom job failed: {e:#}");
            }
        }
    }

    async fn job(&self) -> anyhow::Result<()> {
        // The lock goes when this transaction ends, however this function
        // exits. It never writes, so staying open doesn't hold back vacuum.
        let mut lock = self.pool.begin().await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(JOB_LOCK)
            .fetch_one(&mut *lock)
            .await?;
        if !acquired {
            return Ok(());
        }

        let ready: bool = redis::cmd("EXISTS")
            .arg(READY_KEY)
            .query_async(&mut self.redis.clone())
            .await?;
        if !ready {
            return self.recover().await;
        }

        self.add_recent().await?;

        let snapshot_due: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (
                SELECT 1 FROM bloom_snapshots WHERE taken_at > now() - $1::interval
            )",
        )
        .bind(SNAPSHOT_MAX_AGE)
        .fetch_one(&self.pool)
        .await?;
        if snapshot_due {
            self.save_snapshot().await?;
        }
        Ok(())
    }

    /// Rebuild the filter, then mark it ready. Starts from the snapshot if it
    /// loads, and otherwise scans all of `links`.
    async fn recover(&self) -> anyhow::Result<()> {
        let id = unique_id();
        let marker = format!("bloom:building:{id}");
        let mut redis = self.redis.clone();
        redis::cmd("SET")
            .arg(&marker)
            .arg(1)
            .query_async::<()>(&mut redis)
            .await?;

        let since = match self.restore_snapshot(&id).await {
            Ok(Some(since)) => since,
            Ok(None) => "-infinity".to_string(),
            Err(e) => {
                eprintln!("bloom snapshot unusable, scanning instead: {e:#}");
                "-infinity".to_string()
            }
        };
        self.add_since(&since).await?;

        self.publish_ready(&marker).await
    }

    /// A flush during the pass takes the marker with it, so this fails and the
    /// half-built filter is never marked ready.
    async fn publish_ready(&self, marker: &str) -> anyhow::Result<()> {
        redis::cmd("RENAME")
            .arg(marker)
            .arg(READY_KEY)
            .query_async::<()>(&mut self.redis.clone())
            .await?;
        Ok(())
    }

    async fn add_recent(&self) -> anyhow::Result<()> {
        let since: String = sqlx::query_scalar("SELECT (now() - $1::interval)::text")
            .bind(OVERLAP)
            .fetch_one(&self.pool)
            .await?;
        self.add_since(&since).await
    }

    /// Add every code created since a Postgres timestamp, a page at a time so
    /// a big table never has to fit in memory.
    async fn add_since(&self, since: &str) -> anyhow::Result<()> {
        let mut last_code = String::new();
        loop {
            let codes: Vec<String> = sqlx::query_scalar(
                "SELECT code FROM links
                 WHERE created_at >= $1::timestamptz AND code > $2
                 ORDER BY code LIMIT $3",
            )
            .bind(since)
            .bind(&last_code)
            .bind(PAGE_SIZE)
            .fetch_all(&self.pool)
            .await?;

            let Some(last) = codes.last() else {
                return Ok(());
            };
            last_code = last.clone();
            self.insert(&codes).await?;
        }
    }

    /// Upload the filter, then point Postgres at it. Failing at any point leaves
    /// the previous snapshot current.
    async fn save_snapshot(&self) -> anyhow::Result<()> {
        // Taken before the dump, so catching up covers anything the dump missed.
        let taken_at: String = sqlx::query_scalar("SELECT now()::text")
            .fetch_one(&self.pool)
            .await?;
        let id = unique_id();

        let mut redis = self.redis.clone();
        let mut iters = Vec::new();
        let mut iter = 0_i64;
        loop {
            let (next, chunk): (i64, Option<Vec<u8>>) = redis::cmd("BF.SCANDUMP")
                .arg(FILTER_KEY)
                .arg(iter)
                .query_async(&mut redis)
                .await?;
            if next == 0 {
                break;
            }
            self.snapshots
                .put(&chunk_path(&id, next), chunk.unwrap_or_default().into())
                .await?;
            iters.push(next);
            iter = next;
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM bloom_snapshots")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO bloom_snapshots (id, taken_at, iters) VALUES ($1, $2::timestamptz, $3)",
        )
        .bind(&id)
        .bind(&taken_at)
        .bind(&iters)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // Also clears whatever a crashed upload left behind. The new snapshot
        // is already safe, so failing here costs only storage.
        if let Err(e) = self.remove_snapshots_except(&id).await {
            eprintln!("bloom snapshot cleanup failed: {e:#}");
        }
        Ok(())
    }

    async fn remove_snapshots_except(&self, id: &str) -> anyhow::Result<()> {
        let snapshots = self
            .snapshots
            .list_with_delimiter(Some(&Path::from(SNAPSHOT_PREFIX)))
            .await?;
        for snapshot in snapshots.common_prefixes {
            if snapshot.filename() == Some(id) {
                continue;
            }
            let chunks = self.snapshots.list_with_delimiter(Some(&snapshot)).await?;
            for chunk in chunks.objects {
                self.snapshots.delete(&chunk.location).await?;
            }
        }
        Ok(())
    }

    /// Load the current snapshot over the filter, returning the timestamp to
    /// catch up from, or `None` if there is no snapshot.
    ///
    /// It loads into a temporary key because `BF.LOADCHUNK` over a filter that
    /// `shorten` has already recreated would lose those codes without an error.
    /// The `RENAME` still replaces it, which is why catching up comes after.
    async fn restore_snapshot(&self, pass: &str) -> anyhow::Result<Option<String>> {
        let snapshot: Option<(String, Vec<i64>, String)> = sqlx::query_as(
            "SELECT id, iters, (taken_at - $1::interval)::text FROM bloom_snapshots",
        )
        .bind(OVERLAP)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, iters, since)) = snapshot else {
            return Ok(None);
        };

        let temp_key = format!("codes_bloom:restoring:{pass}");
        let loaded = self.load_chunks(&id, &iters, &temp_key).await;

        let mut redis = self.redis.clone();
        if let Err(e) = loaded {
            redis::cmd("DEL")
                .arg(&temp_key)
                .query_async::<()>(&mut redis)
                .await?;
            return Err(e);
        }
        redis::cmd("RENAME")
            .arg(&temp_key)
            .arg(FILTER_KEY)
            .query_async::<()>(&mut redis)
            .await?;
        Ok(Some(since))
    }

    async fn load_chunks(&self, id: &str, iters: &[i64], key: &str) -> anyhow::Result<()> {
        let mut redis = self.redis.clone();
        for &iter in iters {
            let chunk = self
                .snapshots
                .get(&chunk_path(id, iter))
                .await?
                .bytes()
                .await?;
            redis::cmd("BF.LOADCHUNK")
                .arg(key)
                .arg(iter)
                .arg(&chunk[..])
                .query_async::<()>(&mut redis)
                .await?;
        }
        Ok(())
    }
}

fn chunk_path(id: &str, iter: i64) -> Path {
    Path::from(format!("{SNAPSHOT_PREFIX}/{id}/{iter}"))
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_nanos();
    nanos.to_string()
}

#[cfg(test)]
mod tests {
    use redis::AsyncCommands;

    use super::*;
    use crate::store::Store;

    /// These need the services in `../docker-compose.yml` and share one Redis,
    /// so run them one at a time:
    /// `cargo test -p <this tier> -- --ignored --test-threads=1`
    async fn test_store() -> Store {
        let store = Store::connect(&Config::from_env())
            .await
            .expect("connect to test services");
        let _: () = redis::cmd("FLUSHALL")
            .query_async(&mut store.bloom.redis.clone())
            .await
            .unwrap();
        sqlx::query("DELETE FROM bloom_snapshots")
            .execute(&store.bloom.pool)
            .await
            .unwrap();
        store
    }

    async fn flush(bloom: &Bloom) {
        let _: () = redis::cmd("FLUSHALL")
            .query_async(&mut bloom.redis.clone())
            .await
            .unwrap();
    }

    async fn in_filter(bloom: &Bloom, code: &str) -> bool {
        redis::cmd("BF.EXISTS")
            .arg(FILTER_KEY)
            .arg(code)
            .query_async(&mut bloom.redis.clone())
            .await
            .unwrap()
    }

    async fn is_ready(bloom: &Bloom) -> bool {
        bloom.redis.clone().exists(READY_KEY).await.unwrap()
    }

    /// Removes a row behind the filter's back, so the code being in the filter
    /// afterward proves it came from the snapshot and not from a scan.
    async fn delete_row(bloom: &Bloom, code: &str) {
        sqlx::query("DELETE FROM links WHERE code = $1")
            .bind(code)
            .execute(&bloom.pool)
            .await
            .unwrap();
    }

    /// The filter a `shorten` recreates after a flush is nearly empty, and old
    /// links must keep resolving while it's in that state.
    #[tokio::test]
    #[ignore]
    async fn links_survive_a_flush_while_the_app_runs() {
        let store = test_store().await;
        let bloom = &store.bloom;
        bloom.job().await.unwrap();
        let old = store.shorten("https://old.example".into()).await.unwrap();

        flush(bloom).await;
        let new = store.shorten("https://new.example".into()).await.unwrap();

        assert!(!is_ready(bloom).await);
        assert!(!in_filter(bloom, &old).await);
        assert_eq!(
            store.resolve(&old).await.unwrap().as_deref(),
            Some("https://old.example")
        );
        assert_eq!(
            store.resolve(&new).await.unwrap().as_deref(),
            Some("https://new.example")
        );

        bloom.job().await.unwrap();
        assert!(is_ready(bloom).await);
        assert!(in_filter(bloom, &old).await);
    }

    #[tokio::test]
    #[ignore]
    async fn a_recreated_filter_keeps_its_real_capacity() {
        let store = test_store().await;
        store.shorten("https://sized.example".into()).await.unwrap();

        let capacity: i64 = redis::cmd("BF.INFO")
            .arg(FILTER_KEY)
            .arg("CAPACITY")
            .query_async::<Vec<i64>>(&mut store.bloom.redis.clone())
            .await
            .unwrap()[0];
        assert_eq!(capacity, store.bloom.capacity);
    }

    #[tokio::test]
    #[ignore]
    async fn a_flush_mid_pass_never_marks_the_filter_ready() {
        let store = test_store().await;
        let bloom = &store.bloom;
        let marker = "bloom:building:test";
        let _: () = bloom.redis.clone().set(marker, 1).await.unwrap();

        flush(bloom).await;

        assert!(bloom.publish_ready(marker).await.is_err());
        assert!(!is_ready(bloom).await);
    }

    /// A code Redis lost after acknowledging it: the filter exists and is marked
    /// ready, so nothing but the job's recent pass can put the code back.
    #[tokio::test]
    #[ignore]
    async fn the_job_repairs_recently_lost_codes() {
        let store = test_store().await;
        let bloom = &store.bloom;
        bloom.job().await.unwrap();
        let code = store.shorten("https://lost.example".into()).await.unwrap();

        let _: () = bloom.redis.clone().del(FILTER_KEY).await.unwrap();
        bloom.job().await.unwrap();

        assert!(in_filter(bloom, &code).await);
    }

    #[tokio::test]
    #[ignore]
    async fn recovery_restores_the_snapshot_and_catches_up() {
        let store = test_store().await;
        let bloom = &store.bloom;
        bloom.job().await.unwrap();
        let before = store
            .shorten("https://before.example".into())
            .await
            .unwrap();
        bloom.save_snapshot().await.unwrap();
        delete_row(bloom, &before).await;
        let after = store.shorten("https://after.example".into()).await.unwrap();

        flush(bloom).await;
        bloom.job().await.unwrap();

        assert!(is_ready(bloom).await);
        assert!(in_filter(bloom, &before).await, "from the snapshot");
        assert!(in_filter(bloom, &after).await, "from catching up");
    }

    #[tokio::test]
    #[ignore]
    async fn an_unusable_snapshot_falls_back_to_a_scan() {
        let store = test_store().await;
        let bloom = &store.bloom;
        bloom.job().await.unwrap();
        let gone = store.shorten("https://gone.example".into()).await.unwrap();
        let kept = store.shorten("https://kept.example".into()).await.unwrap();
        bloom.save_snapshot().await.unwrap();
        delete_row(bloom, &gone).await;
        sqlx::query("UPDATE bloom_snapshots SET iters = iters || 999::bigint")
            .execute(&bloom.pool)
            .await
            .unwrap();

        flush(bloom).await;
        bloom.job().await.unwrap();

        assert!(is_ready(bloom).await);
        assert!(!in_filter(bloom, &gone).await, "the snapshot was discarded");
        assert!(in_filter(bloom, &kept).await, "the scan found it");
        let leftovers: Vec<String> = bloom.redis.clone().keys("codes_bloom:*").await.unwrap();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn saving_a_snapshot_removes_the_previous_one() {
        let store = test_store().await;
        let bloom = &store.bloom;
        bloom.job().await.unwrap();

        bloom.save_snapshot().await.unwrap();
        bloom.save_snapshot().await.unwrap();

        let snapshots = bloom
            .snapshots
            .list_with_delimiter(Some(&Path::from(SNAPSHOT_PREFIX)))
            .await
            .unwrap();
        assert_eq!(snapshots.common_prefixes.len(), 1);
    }
}
