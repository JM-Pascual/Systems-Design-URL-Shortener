//! The Postgres-backed store: `links` table plus a `nextval()`-driven id
//! sequence, shared across as many processes as connect to it.
//!
//! # What replaces the `Mutex<Store>` from Tier 1
//!
//! Tier 1 wrapped a plain `HashMap` in a `Mutex` because `HashMap` has no
//! concurrency story of its own — every request, reads included, serialised
//! behind one lock. `PgPool` is different: it *is* the concurrency story.
//! Checking out a connection, running a query, and returning it to the pool
//! is safe to do from many requests at once; Postgres's own MVCC handles
//! concurrent reads and writes without the app coordinating any of it. That
//! is why `Store` below holds a `PgPool` directly, not a `Mutex<PgPool>`.
//!
//! # Why `shorten` and `resolve` return `Result`, not a bare value
//!
//! Tier 1's `HashMap::get`/`insert` could not fail. A network call to
//! Postgres genuinely can — a dropped connection, a full pool, a constraint
//! violation. Callers (`main.rs`) now have to decide what an HTTP client
//! sees when that happens.

use sqlx::{PgPool, Row};

use crate::base62;

pub struct Store {
    pool: PgPool,
}

/// A generic dump of a table's rows, column names discovered at query time
/// rather than hardcoded — so `/stats` keeps working as the schema evolves.
pub struct TableDump {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Store {
    /// Connect to Postgres and run pending migrations.
    ///
    /// `sqlx::migrate!()` embeds the SQL files under `migrations/` into the
    /// binary at compile time, so a fresh database is brought up to date
    /// automatically — no separate migration step for `cargo run` to work.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPool::connect(database_url).await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    /// Store a long URL and return the freshly minted short code.
    ///
    /// The id comes from Postgres's `link_ids` sequence (`SELECT
    /// nextval('link_ids')`), then gets base62-encoded exactly as Tier 1
    /// encoded its in-memory counter — same algorithm, different source of
    /// truth for "what's next."
    pub async fn shorten(&self, url: String) -> Result<String, sqlx::Error> {
        let (id,): (i64,) = sqlx::query_as("SELECT nextval('link_ids')")
            .fetch_one(&self.pool)
            .await?;
        let code = base62::encode(id as u64);

        sqlx::query("INSERT INTO links (code, long_url) VALUES ($1, $2)")
            .bind(&code)
            .bind(url)
            .execute(&self.pool)
            .await?;

        Ok(code)
    }

    /// Look up the long URL for a code, `None` if it doesn't exist (or has
    /// expired — see the README's discussion question on that query).
    pub async fn resolve(&self, code: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT long_url FROM links WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(url,)| url))
    }

    /// A literal `SELECT * FROM links` for the `/stats` endpoint, with every
    /// column cast to `text` so heterogeneous types (a `UUID`, a
    /// `TIMESTAMPTZ`, a `NULL`) all come back as one uniform shape the
    /// caller can print without knowing the schema ahead of time.
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

    /// These tests need a real Postgres reachable at `DATABASE_URL` (see
    /// `../docker-compose.yml`) and are marked `#[ignore]` so `cargo test`
    /// stays usable without one. Run them explicitly:
    /// `cargo test -p tier-3-persistence -- --ignored`
    async fn test_store() -> Store {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://shortener:shortener@localhost:5433/shortener".into());
        Store::connect(&database_url)
            .await
            .expect("connect to test database")
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
}
