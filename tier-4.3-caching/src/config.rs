//! Runtime configuration, read from the environment.
//!
//! `base_url` and `database_url` are unchanged from Tier 3. `redis_url` arrived
//! with Tier 4.1, the S3 settings and `bloom_capacity` with Tier 4.2.

/// Everything the service needs to know that is not compiled into it.
pub struct Config {
    /// Public origin used to build short URLs, e.g. `https://sho.rt`.
    /// Stored without a trailing slash.
    pub base_url: String,

    /// Postgres connection string.
    pub database_url: String,

    /// Redis connection string, e.g. `redis://localhost:6382`.
    pub redis_url: String,

    /// S3-compatible object store holding the Bloom filter's snapshots.
    pub s3_endpoint: String,
    pub s3_bucket: String,
    pub s3_access_key: String,
    pub s3_secret_key: String,

    /// How many codes the Bloom filter is sized for before it starts growing.
    /// Allocated up front: the default suits a laptop, not 100M links a month.
    pub bloom_capacity: i64,
}

const DEFAULT_BASE_URL: &str = "http://localhost:3000";

/// Matches `docker-compose.yml`'s credentials and port, so `cargo run`
/// works against the bundled local Postgres with no setup beyond
/// `docker compose up -d`.
const DEFAULT_DATABASE_URL: &str = "postgres://shortener:shortener@localhost:5436/shortener";

/// Matches `docker-compose.yml`'s Redis port.
const DEFAULT_REDIS_URL: &str = "redis://localhost:6382";

/// Match `docker-compose.yml`'s MinIO service.
const DEFAULT_S3_ENDPOINT: &str = "http://localhost:9003";
const DEFAULT_S3_BUCKET: &str = "bloom-snapshots";
const DEFAULT_S3_ACCESS_KEY: &str = "shortener";
const DEFAULT_S3_SECRET_KEY: &str = "shortener-secret";

const DEFAULT_BLOOM_CAPACITY: i64 = 1_000_000;

impl Config {
    /// Read configuration from the process environment.
    pub fn from_env() -> Self {
        let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let base_url = base_url.trim_end_matches('/').to_string();

        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string());

        let env_or =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());

        Self {
            base_url,
            database_url,
            redis_url,
            s3_endpoint: env_or("S3_ENDPOINT", DEFAULT_S3_ENDPOINT),
            s3_bucket: env_or("S3_BUCKET", DEFAULT_S3_BUCKET),
            s3_access_key: env_or("S3_ACCESS_KEY", DEFAULT_S3_ACCESS_KEY),
            s3_secret_key: env_or("S3_SECRET_KEY", DEFAULT_S3_SECRET_KEY),
            bloom_capacity: std::env::var("BLOOM_CAPACITY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_BLOOM_CAPACITY),
        }
    }

    /// Build the public short URL for a code.
    pub fn short_url(&self, code: &str) -> String {
        format!("{}/{}", self.base_url, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_code_onto_base_url() {
        let cfg = Config {
            base_url: "https://sho.rt".to_string(),
            database_url: DEFAULT_DATABASE_URL.to_string(),
            redis_url: DEFAULT_REDIS_URL.to_string(),
            s3_endpoint: DEFAULT_S3_ENDPOINT.to_string(),
            s3_bucket: DEFAULT_S3_BUCKET.to_string(),
            s3_access_key: DEFAULT_S3_ACCESS_KEY.to_string(),
            s3_secret_key: DEFAULT_S3_SECRET_KEY.to_string(),
            bloom_capacity: DEFAULT_BLOOM_CAPACITY,
        };
        assert_eq!(cfg.short_url("3d7"), "https://sho.rt/3d7");
    }
}
