//! The second `DEL`: read every committed change to `links` from Postgres's write-ahead log,
//! and delete its code from Redis. The handlers already send one `DEL` and don't retry;
//! this one retries until Redis accepts it, so a lost `DEL` gets repaired. See this tier's
//! README.

use std::time::Duration;

use anyhow::Context;
use pgwire_replication::{Lsn, ReplicationClient, ReplicationConfig, ReplicationEvent, TlsConfig};
use redis::AsyncCommands;
use sqlx::PgPool;

use crate::config::Config;

/// The replication slot: Postgres's record of how far this consumer has confirmed. It keeps
/// the WAL from that point on until we confirm more.
const SLOT: &str = "cache_invalidator";
/// Created by a migration: which tables the slot streams.
const PUBLICATION: &str = "links_pub";
const RETRY: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct Invalidator {
    pool: PgPool,
    redis: redis::aio::ConnectionManager,
    database_url: String,
}

impl Invalidator {
    pub fn new(pool: PgPool, redis: redis::aio::ConnectionManager, config: &Config) -> Self {
        Self {
            pool,
            redis,
            database_url: config.database_url.clone(),
        }
    }

    /// Stream forever, reconnecting after any failure. Only one connection can hold the
    /// slot, so with several app instances the others fail here and retry: the slot works
    /// as the lock.
    pub async fn run(self) {
        loop {
            if let Err(e) = self.stream().await {
                eprintln!("invalidator stopped: {e:#}");
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    async fn stream(&self) -> anyhow::Result<()> {
        // Fails harmlessly if the slot exists, including when two instances race to create it.
        let _ = sqlx::query("SELECT pg_create_logical_replication_slot($1, 'pgoutput')")
            .bind(SLOT)
            .execute(&self.pool)
            .await;
        let confirmed: Option<String> = sqlx::query_scalar(
            "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1",
        )
        .bind(SLOT)
        .fetch_one(&self.pool)
        .await?;
        let start = Lsn::parse(confirmed.as_deref().unwrap_or("0/0"))?;

        let url = url::Url::parse(&self.database_url)?;
        let config = ReplicationConfig::new(
            url.host_str().context("DATABASE_URL has no host")?,
            url.username(),
            url.password().unwrap_or_default(),
            url.path().trim_start_matches('/'),
            SLOT,
            PUBLICATION,
        )
        .with_port(url.port().unwrap_or(5432))
        .with_tls(TlsConfig::disabled())
        .with_start_lsn(start);
        let mut changes = ReplicationClient::connect(config).await?;

        let mut codes = Vec::new();
        while let Some(event) = changes.recv().await? {
            match event {
                ReplicationEvent::Begin { .. } => codes.clear(),
                ReplicationEvent::XLogData { data, .. } => codes.extend(changed_code(&data)),
                ReplicationEvent::Commit { end_lsn, .. } => {
                    self.delete_until_accepted(&codes).await;
                    // Only now may Postgres drop this part of the WAL. Confirming before the
                    // `DEL` lands would lose it if we crashed in between.
                    changes.update_applied_lsn(end_lsn);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `DEL` is idempotent, so retrying, or repeating the handler's `DEL`, is harmless.
    async fn delete_until_accepted(&self, codes: &[String]) {
        if codes.is_empty() {
            return;
        }
        while let Err(e) = self.redis.clone().del::<_, ()>(codes).await {
            eprintln!("invalidator: redis failed, retrying: {e:#}");
            tokio::time::sleep(RETRY).await;
        }
    }
}

/// The code of an updated or deleted row, from one pgoutput message (protocol version 1).
/// Inserts are skipped: nothing is cached before a code's first read.
///
/// shortcut: assumes `code` is the first column of `links`, the only published table. Read
/// the column names from the Relation ('R') message once more tables are published.
fn changed_code(message: &[u8]) -> Option<String> {
    // 'U' or 'D', a 4-byte table id, then a tuple tag: 'K' or 'O' for the old key, 'N' for
    // the new row. Either one starts with `code`.
    if !matches!(message.first(), Some(b'U' | b'D')) {
        return None;
    }
    // The tuple: a 2-byte column count, then per column a kind byte. 't' is text, followed
    // by a 4-byte length and the bytes.
    let tuple = message.get(6..)?;
    if tuple.get(2) != Some(&b't') {
        return None;
    }
    let len = u32::from_be_bytes(tuple.get(3..7)?.try_into().ok()?) as usize;
    String::from_utf8(tuple.get(7..7 + len)?.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u8, tag: u8, code: &str) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 64, 1, tag, 0, 2, b't'];
        m.extend((code.len() as u32).to_be_bytes());
        m.extend(code.as_bytes());
        m.push(b'n');
        m
    }

    use crate::store::Store;

    /// These need the services in `../docker-compose.yml`; see `store.rs` for how to run them.
    async fn setup() -> (Store, redis::aio::MultiplexedConnection) {
        let store = Store::connect(&Config::from_env()).await.unwrap();
        let mut admin = redis::Client::open(Config::from_env().redis_url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        // An `admin` user keeps the power to give `default`, the app's user, its commands back.
        let _: () = redis::cmd("ACL")
            .arg(&["SETUSER", "admin", "on", "nopass", "~*", "+@all"])
            .query_async(&mut admin)
            .await
            .unwrap();
        let _: () = redis::cmd("AUTH")
            .arg(&["admin", "any"])
            .query_async(&mut admin)
            .await
            .unwrap();
        // The slot must exist before a change for that change to be streamed.
        let _ = sqlx::query("SELECT pg_create_logical_replication_slot($1, 'pgoutput')")
            .bind(SLOT)
            .execute(&store.invalidator.pool)
            .await;
        (store, admin)
    }

    async fn acl(admin: &mut redis::aio::MultiplexedConnection, rule: &str) {
        let _: () = redis::cmd("ACL")
            .arg(&["SETUSER", "default", rule])
            .query_async(admin)
            .await
            .unwrap();
    }

    /// Wait for the invalidator to remove `code`, or fail after 10 seconds.
    async fn assert_removed(admin: &mut redis::aio::MultiplexedConnection, code: &str) {
        for _ in 0..100 {
            let exists: bool = admin.exists(code).await.unwrap();
            if !exists {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{code} is still cached");
    }

    /// The handler's `DEL` fails and the invalidator isn't running. Once it starts, it replays
    /// the change from its slot and removes the stale URL.
    #[tokio::test]
    #[ignore]
    async fn a_lost_del_is_repaired_from_the_wal() {
        let (store, mut admin) = setup().await;
        let code = store.shorten("https://old.example".into()).await.unwrap();
        store.resolve(&code).await.unwrap();

        acl(&mut admin, "-del").await;
        assert!(store.update(&code, "https://new.example").await.unwrap());
        acl(&mut admin, "+del").await;
        let cached: String = admin.get(&code).await.unwrap();
        assert_eq!(cached, "https://old.example", "the handler's DEL was lost");

        let invalidator = tokio::spawn(store.invalidator.clone().run());
        assert_removed(&mut admin, &code).await;
        invalidator.abort();
        assert_eq!(
            store.resolve(&code).await.unwrap().as_deref(),
            Some("https://new.example")
        );
    }

    /// While Redis rejects `DEL`, the invalidator keeps retrying instead of confirming.
    #[tokio::test]
    #[ignore]
    async fn the_invalidator_retries_until_redis_accepts() {
        let (store, mut admin) = setup().await;
        let code = store.shorten("https://old.example".into()).await.unwrap();
        store.resolve(&code).await.unwrap();
        let invalidator = tokio::spawn(store.invalidator.clone().run());

        acl(&mut admin, "-del").await;
        store.update(&code, "https://new.example").await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let cached: String = admin.get(&code).await.unwrap();
        assert_eq!(cached, "https://old.example");

        acl(&mut admin, "+del").await;
        assert_removed(&mut admin, &code).await;
        invalidator.abort();
    }

    /// Writes that skip the app, like a manual fix in `psql`, are invalidated too.
    #[tokio::test]
    #[ignore]
    async fn manual_edits_are_invalidated() {
        let (store, mut admin) = setup().await;
        let code = store.shorten("https://old.example".into()).await.unwrap();
        store.resolve(&code).await.unwrap();
        let invalidator = tokio::spawn(store.invalidator.clone().run());

        sqlx::query("UPDATE links SET long_url = 'https://new.example' WHERE code = $1")
            .bind(&code)
            .execute(&store.invalidator.pool)
            .await
            .unwrap();
        assert_removed(&mut admin, &code).await;
        invalidator.abort();
    }

    #[test]
    fn reads_the_code_of_updates_and_deletes() {
        assert_eq!(
            changed_code(&message(b'U', b'N', "abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            changed_code(&message(b'D', b'K', "xyz")).as_deref(),
            Some("xyz")
        );
        assert_eq!(changed_code(&message(b'I', b'N', "abc")), None);
        assert_eq!(changed_code(b"U"), None);
    }
}
