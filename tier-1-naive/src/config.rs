//! Runtime configuration, read from the environment.
//!
//! # Why the base URL cannot be hardcoded
//!
//! The service returns short URLs to its callers, so it has to know its own
//! public address — and that address is not something the process can discover.
//! It is decided by whatever sits in front of it:
//!
//! * locally, it is `http://localhost:3000`;
//! * in the class demo it might be `http://192.168.1.40:3000`;
//! * behind a reverse proxy or a load balancer it is a real hostname on port
//!   443, e.g. `https://sho.rt`, while the process still listens on :3000.
//!
//! That last case is the important one: **the address the server binds to and
//! the address users type are different things**, and the process only ever
//! knows the first. The second has to be told to it. This is the standard
//! twelve-factor rule — config that varies between deployments lives in the
//! environment, not in the binary.
//!
//! It matters more from Tier 5 onward, where several app servers sit behind one
//! load balancer: each one binds its own port but they must all mint links
//! pointing at the shared public hostname.

/// Everything the service needs to know that is not compiled into it.
///
/// One field today. Later tiers add `database_url`, `redis_url`, `cache_ttl`,
/// which is why this is a struct rather than a bare `String` threaded through
/// the handlers.
#[derive(Debug, Clone)]
pub struct Config {
    /// Public origin used to build short URLs, e.g. `https://sho.rt`.
    ///
    /// Stored **without** a trailing slash, so callers can always write
    /// `format!("{}/{code}", cfg.base_url)` and get exactly one separator.
    /// Normalising once here beats making every call site defensive.
    pub base_url: String,
}

/// Fallback when `BASE_URL` is unset — matches the port `main` binds to, so
/// `cargo run` works with no setup.
const DEFAULT_BASE_URL: &str = "http://localhost:3000";

impl Config {
    /// Read configuration from the process environment.
    ///
    /// ```text
    /// BASE_URL unset               -> "http://localhost:3000"
    /// BASE_URL="https://sho.rt"    -> "https://sho.rt"
    /// BASE_URL="https://sho.rt/"   -> "https://sho.rt"     (slash trimmed)
    /// ```
    pub fn from_env() -> Self {
        let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Build the public short URL for a code.
    ///
    /// A tiny method, but it puts the join in exactly one place: every caller
    /// gets the same separator handling, and when a later tier moves links to a
    /// `/r/{code}` prefix there is one line to change.
    pub fn short_url(&self, code: &str) -> String {
        format!("{}/{}", self.base_url, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Note these tests build `Config` directly rather than calling
    /// `from_env()`. Environment variables are *process-global*, and Rust runs
    /// tests in parallel threads by default, so a test that sets `BASE_URL`
    /// races against every other test in the binary. Testing the pure parts and
    /// leaving `from_env` as thin as possible is the usual way out of that.
    #[test]
    fn joins_code_onto_base_url() {
        let cfg = Config {
            base_url: "https://sho.rt".to_string(),
        };
        assert_eq!(cfg.short_url("3d7"), "https://sho.rt/3d7");
    }

    #[test]
    fn default_matches_the_bound_port() {
        assert_eq!(DEFAULT_BASE_URL, "http://localhost:3000");
        assert!(!DEFAULT_BASE_URL.ends_with('/'), "must be stored unslashed");
    }
}
