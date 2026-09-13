//! Runtime configuration, read from the environment.
//!
//! `base_url` and `database_url` are unchanged from Tier 3. `redis_url` is
//! this tier's new piece.

/// Everything the service needs to know that is not compiled into it.
#[derive(Debug, Clone)]
pub struct Config {
    /// Public origin used to build short URLs, e.g. `https://sho.rt`.
    /// Stored without a trailing slash.
    pub base_url: String,

    /// Postgres connection string.
    pub database_url: String,

    /// Redis connection string, e.g. `redis://localhost:6382`.
    pub redis_url: String,
}

const DEFAULT_BASE_URL: &str = "http://localhost:3000";

/// Matches `docker-compose.yml`'s credentials and port, so `cargo run`
/// works against the bundled local Postgres with no setup beyond
/// `docker compose up -d`.
const DEFAULT_DATABASE_URL: &str = "postgres://shortener:shortener@localhost:5436/shortener";

/// Matches `docker-compose.yml`'s Redis port.
const DEFAULT_REDIS_URL: &str = "redis://localhost:6382";

impl Config {
    /// Read configuration from the process environment.
    pub fn from_env() -> Self {
        let base_url =
            std::env::var("BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let base_url = base_url.trim_end_matches('/').to_string();

        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string());

        Self {
            base_url,
            database_url,
            redis_url,
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
        };
        assert_eq!(cfg.short_url("3d7"), "https://sho.rt/3d7");
    }
}
