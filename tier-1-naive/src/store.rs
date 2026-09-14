//! The naive store: a `HashMap<code, url>` plus a counter, in one process.
//!
//! # What a hash map actually is
//!
//! This is the data structure the whole class is built on, so it is worth
//! being precise about it. A hash map is:
//!
//! * an **array of buckets** (Rust's `HashMap` uses one contiguous allocation,
//!   the SwissTable layout);
//! * a **hash function** `h(key) -> u64` that spreads keys over that array;
//! * a rule for **index = h(key) mod n_buckets**;
//! * a strategy for **collisions** — two keys landing in the same bucket.
//!   Textbook options: *chaining* (each bucket holds a linked list) and *open
//!   addressing* (probe the next bucket). Rust's `std::collections::HashMap`
//!   uses open addressing with SIMD-accelerated group probing.
//!
//! **Load factor** = `entries / buckets`. As it rises, collisions become
//! frequent and lookups degrade toward O(n). So when it crosses a threshold
//! (~87.5% for Rust's implementation) the map **resizes**: allocate a bigger
//! array, rehash every key into it, free the old one. That single resize is
//! O(n) — but it happens rarely enough (each resize doubles capacity) that the
//! cost spread over all insertions is constant. That is what **amortized
//! O(1)** means: individual operations are O(1) *on average over a sequence*,
//! even though one occasional operation is O(n).
//!
//! Keep this picture: in Tier 4 we replace this map with Redis, which is
//! conceptually the *same structure* — a hash table — just living in another
//! process, reachable over the network, and shared by every app server.
//!
//! # Why this tier is "naive"
//!
//! Three fatal problems:
//!
//! 1. **Volatile.** Restart the process and every short link ever created is
//!    gone. -> fixed in Tier 3 (Postgres).
//! 2. **Single process.** A second app server has its own map and its own
//!    counter, so both would hand out the code `"5"` for different URLs.
//!    Left as an open problem -- the class ends at Tier 4.4.
//! 3. **Bounded by RAM.** 50 GB/month of URLs (Tier 0's estimate) does not
//!    fit. Also left as an open problem.

use std::collections::HashMap;

use crate::base62;

/// The entire state of the Tier 1 service.
///
/// Note there is no `Mutex` in here. Making a data structure thread-safe is a
/// *separate* concern from what it stores, so we keep `Store` plain and let
/// `main.rs` wrap it in `Mutex<Store>`. This is a deliberate Rust idiom: types
/// stay single-threaded and callers add synchronisation where they need it.
pub struct Store {
    /// code -> long URL. The hash map described above.
    urls: HashMap<String, String>,

    /// The next counter value to hand out.
    ///
    /// This is "Path A" from Tier 2, in embryo: codes come from a monotonic
    /// counter, so they are unique *by construction*. Note also that the code
    /// has no relationship to the URL's content — which is exactly the property
    /// that makes a `PATCH` (edit the destination, keep the code) possible in
    /// Tier 4.
    next_id: u64,
}

impl Store {
    /// Create an empty store whose first issued code will be `"0"`.
    pub fn new() -> Self {
        Self {
            urls: HashMap::new(),
            next_id: 0,
        }
    }

    /// Store a long URL and return the freshly minted short code.
    pub fn shorten(&mut self, url: String) -> String {
        let code = base62::encode(self.next_id);
        self.next_id += 1;
        self.urls.insert(code.clone(), url);
        code
    }

    /// Look up the long URL for a code.
    pub fn resolve(&self, code: &str) -> Option<String> {
        self.urls.get(code).cloned()
    }

    /// How many links exist. Used by the `/stats` endpoint and by tests.
    pub fn len(&self) -> usize {
        self.urls.len()
    }

    /// Clippy insists that any type with `len` also has `is_empty`, and it is
    /// right: it is what callers reach for first.
    #[allow(dead_code)] // used by tests / later tiers, not by main.rs
    pub fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }
}

/// `Default` is the conventional companion to a no-argument `new()`. Deriving
/// it is not possible here (we want `next_id` to start at 0 *and* to document
/// that choice in one place), so we delegate.
impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_sequential_codes() {
        let mut store = Store::new();
        assert_eq!(store.shorten("https://a.example".into()), "0");
        assert_eq!(store.shorten("https://b.example".into()), "1");
        assert_eq!(store.shorten("https://c.example".into()), "2");
    }

    #[test]
    fn resolves_what_it_stored() {
        let mut store = Store::new();
        let code = store.shorten("https://example.com/long/path".into());
        assert_eq!(
            store.resolve(&code),
            Some("https://example.com/long/path".to_string())
        );
    }

    #[test]
    fn unknown_code_resolves_to_none() {
        let store = Store::new();
        assert_eq!(store.resolve("nope"), None);
    }

    /// The same URL submitted twice gets two *different* codes. That is a
    /// design decision, not an accident — see Tier 0, discussion question 1.
    #[test]
    fn duplicate_urls_get_distinct_codes() {
        let mut store = Store::new();
        let a = store.shorten("https://same.example".into());
        let b = store.shorten("https://same.example".into());
        assert_ne!(a, b);
        assert_eq!(store.len(), 2);
    }

    /// Uniqueness is guaranteed by the counter, not hoped for. Ten thousand
    /// codes, zero collisions — and this would hold for ten billion.
    #[test]
    fn codes_never_collide() {
        let mut store = Store::new();
        for i in 0..10_000 {
            store.shorten(format!("https://example.com/{i}"));
        }
        assert_eq!(store.len(), 10_000);
    }
}
