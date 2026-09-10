//! Tier 1 — the naive single-server URL shortener.
//!
//! Everything lives in one process, in RAM. Run it with:
//!
//! ```text
//! cargo run -p tier-1-naive
//! ```
//!

mod base62;
mod config;
mod store;

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use config::Config;
use store::Store;

/// Shared application state.
///
/// Axum clones the state for every request, so it must be cheap to clone —
/// hence `Arc` (an atomically reference-counted pointer; cloning it just bumps
/// a counter). Inside it, `Mutex` provides the mutual exclusion: `Store` has
/// `&mut self` methods, and many requests run concurrently on the tokio thread
/// pool, so exactly one of them may mutate the map at a time.
///
/// `Arc<Mutex<T>>` is *the* Rust idiom for "shared mutable state across
/// threads". Read it as two separate jobs:
///   * `Arc`   — shared ownership (who is allowed to keep a pointer?)
///   * `Mutex` — exclusive access  (who is allowed to touch it right now?)
///
/// Note this single mutex serialises *every* request, readers included. That is
/// fine at Tier 1's scale and is a nice thing to point at when Tier 4 explains
/// why a shared cache is a better answer than a bigger lock. (`RwLock` would
/// let readers proceed in parallel — a good one-line exercise.)
struct AppState {
    store: Mutex<Store>,
    /// Read once at startup and never mutated, so it needs no `Mutex` — the
    /// `Arc` alone is enough to share an immutable value across threads.
    config: Config,
}

/// Request body for `POST /shorten`.
///
/// `#[derive(Deserialize)]` makes serde generate the JSON-parsing code at
/// compile time. Axum's `Json<T>` extractor requires it.
#[derive(Deserialize)]
struct ShortenRequest {
    url: String,
}

/// Response body for `POST /shorten`.
#[derive(Serialize)]
struct ShortenResponse {
    code: String,
    short_url: String,
}

/// `POST /shorten` — create a short code for a URL.
///
/// Axum builds handler arguments by *extraction*: each parameter type knows how
/// to pull itself out of the request. Order matters — the body-consuming
/// extractor (`Json`) must come last, because it takes ownership of the body.
async fn shorten(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ShortenRequest>,
) -> impl IntoResponse {
    // A real service would validate the URL here (scheme is http/https, host is
    // present, not a link to ourselves). Tier 7 adds a blocklist check. Tier 1
    // trusts its input, which is one more reason it is called "naive".
    let code = state
        .store
        .lock()
        .expect("store mutex poisoned")
        .shorten(req.url);
    // ^ The `MutexGuard` is a temporary within this statement, so the lock is
    //   released right here — not held while we build the response below.

    // `short_url` first so that `code` is still borrowable here, and can then
    // be *moved* into the struct on the next line without a clone. Struct field
    // order in a literal does not have to match the declaration.
    let body = ShortenResponse {
        short_url: state.config.short_url(&code),
        code,
    };

    (StatusCode::CREATED, Json(body))
}

/// `GET /{code}` — the redirect. This is the hot path: ~99% of all traffic
/// (Tier 0, §3.1), and the thing every later tier is trying to make faster.
async fn redirect(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<Redirect, StatusCode> {
    let url = state
        .store
        .lock()
        .expect("store mutex poisoned")
        .resolve(&code);

    match url {
        // `Redirect::temporary` is **307**, not 301 (permanent) or 302.
        //   301 — permanent; browsers cache it ~forever. That would make click
        //         analytics impossible and would mean a `PATCH` in Tier 4 never
        //         reaches anyone who already followed the link.
        //   302 — the historical "found"; some clients rewrite POST to GET.
        //   307 — temporary, method-preserving. The modern default.
        // Choosing a temporary redirect costs us one request per click and buys
        // us control over the destination. Real shorteners split the difference:
        // 301 for links they know are immutable, 302/307 otherwise.
        Some(url) => Ok(Redirect::temporary(&url)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// `GET /stats` — how many links exist.
async fn stats(State(state): State<Arc<AppState>>) -> String {
    let n = state.store.lock().expect("store mutex poisoned").len();
    format!("{n} links\n")
}

/// `#[tokio::main]` rewrites this function to start the tokio async runtime and
/// block on the body. Async is here because axum is async; nothing in Tier 1's
/// own logic needs it yet. (Tier 3 will, when the DB round-trip becomes a real
/// await point.)
#[tokio::main]
async fn main() {
    // Read config before binding the socket: a misconfigured process should
    // fail immediately rather than after it has started accepting requests.
    let config = Config::from_env();

    let state = Arc::new(AppState {
        store: Mutex::new(Store::new()),
        config,
    });

    let app = Router::new()
        .route("/shorten", post(shorten))
        .route("/stats", get(stats))
        // Axum's router prefers *static* segments over dynamic ones, so
        // `/stats` above wins over `/{code}` here regardless of declaration
        // order. Worth knowing, but not worth relying on: it is why real
        // shorteners put management endpoints under a prefix like `/api/`.
        .route("/{code}", get(redirect))
        // `with_state` takes the state *by value*, so handing it `state`
        // directly would move it and the `println!` below could no longer read
        // `state.config`. `Arc::clone` hands the router its own handle to the
        // same allocation — it copies a pointer and bumps a refcount, not the
        // `AppState`. Writing `Arc::clone(&state)` rather than `state.clone()`
        // is a common convention: it makes the cheapness explicit at the call
        // site, since `.clone()` on a field could mean a deep copy.
        .with_state(Arc::clone(&state));

    // The *listen* address stays fixed while `BASE_URL` varies. They are
    // genuinely different things: behind a proxy this process listens on
    // 0.0.0.0:3000 while users see `https://sho.rt`. Conflating them is a
    // common source of links that work locally and break in deployment.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("port 3000 already in use?");

    println!("tier-1-naive listening on 0.0.0.0:3000");
    println!("minting short URLs as {}/{{code}}", state.config.base_url);
    println!();
    println!("  curl -X POST localhost:3000/shorten -H 'content-type: application/json' \\");
    println!("       -d '{{\"url\":\"https://example.com\"}}'");
    println!("  curl -i localhost:3000/0");

    axum::serve(listener, app).await.expect("server error");
}
