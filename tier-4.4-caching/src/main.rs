//! Tier 4.4 — Redis in front of Tier 3's Postgres. Run it with:
//!
//! ```text
//! docker compose -f tier-4.4-caching/docker-compose.yml up -d
//! cargo run -p tier-4-4-caching
//! ```

mod base62;
mod bloom;
mod config;
mod store;

use std::sync::Arc;

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

/// Shared application state. No `Mutex` here — `Store` wraps a `PgPool` and
/// a `ConnectionManager`, both already safe to share across requests.
struct AppState {
    store: Store,
    config: Config,
}

#[derive(Deserialize)]
struct ShortenRequest {
    url: String,
}

#[derive(Serialize)]
struct ShortenResponse {
    code: String,
    short_url: String,
}

#[derive(Deserialize)]
struct PatchRequest {
    url: String,
}

/// `POST /shorten` — create a short code for a URL.
async fn shorten(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ShortenRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let code = state
        .store
        .shorten(req.url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let body = ShortenResponse {
        short_url: state.config.short_url(&code),
        code,
    };

    Ok((StatusCode::CREATED, Json(body)))
}

/// `GET /{code}` — the redirect.
async fn redirect(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<Redirect, StatusCode> {
    let url = state
        .store
        .resolve(&code)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match url {
        Some(url) => Ok(Redirect::temporary(&url)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// `PATCH /{code}` — change a code's destination.
async fn patch_link(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
    Json(req): Json<PatchRequest>,
) -> Result<StatusCode, StatusCode> {
    let updated = state
        .store
        .update(&code, &req.url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// `DELETE /{code}` — remove a code entirely.
async fn delete_link(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let deleted = state
        .store
        .delete(&code)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// `GET /metrics` — how many times `resolve` has queried Postgres, in
/// Prometheus format, for `../thundering-herd-demo/`. A stampede shows up in
/// this counter's *rate*.
async fn metrics(State(state): State<Arc<AppState>>) -> String {
    format!(
        "# HELP postgres_queries_total Total resolve() calls that queried Postgres.\n\
         # TYPE postgres_queries_total counter\n\
         postgres_queries_total {}\n",
        state.store.postgres_query_count()
    )
}

/// `GET /stats` — every link, one per line. For watching the table fill up
/// during a demo, not an API a client would parse.
async fn stats(State(state): State<Arc<AppState>>) -> Result<String, StatusCode> {
    let links = state
        .store
        .dump_links()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(links
        .iter()
        .map(|(code, url)| format!("{code}  {url}\n"))
        .collect())
}

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    let store = Store::connect(&config)
        .await
        .expect("failed to connect to Postgres/Redis");
    tokio::spawn(store.bloom.clone().run());

    let state = Arc::new(AppState { store, config });

    let app = Router::new()
        .route("/shorten", post(shorten))
        .route("/stats", get(stats))
        .route("/metrics", get(metrics))
        .route(
            "/{code}",
            get(redirect).patch(patch_link).delete(delete_link),
        )
        .with_state(Arc::clone(&state));

    // Configurable so the thundering-herd demo can run several tiers side by
    // side on one machine.
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|_| panic!("port {port} already in use?"));

    println!("tier-4.4-caching listening on {addr}");
    println!("minting short URLs as {}/{{code}}", state.config.base_url);
    println!();
    println!("  curl -X POST localhost:{port}/shorten -H 'content-type: application/json' \\");
    println!("       -d '{{\"url\":\"https://example.com\"}}'");
    println!("  curl -i localhost:{port}/1");

    axum::serve(listener, app).await.expect("server error");
}
