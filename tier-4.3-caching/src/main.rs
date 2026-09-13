//! Tier 4 — caching. Same two endpoints, with Redis sitting in front of
//! Tier 3's Postgres as a cache-aside layer. Run it with:
//!
//! ```text
//! docker compose -f tier-4.3-caching/docker-compose.yml up -d
//! cargo run -p tier-4-3-caching
//! ```

mod base62;
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
/// a `MultiplexedConnection`, both already safe to share across concurrent
/// requests. See `store.rs` for why.
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

/// `GET /{code}` — the redirect. Cache-aside: Redis first, Postgres on a miss.
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

/// `GET /stats` — a literal `SELECT * FROM links`, rendered as an ASCII
/// table. Postgres only — it doesn't reflect what's currently cached.
async fn stats(State(state): State<Arc<AppState>>) -> Result<String, StatusCode> {
    let dump = state
        .store
        .dump_links()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(render_ascii_table(&dump.columns, &dump.rows))
}

/// Renders a column list and row values (already all `text`, `None` for SQL
/// `NULL`) as a `psql`-style bordered table.
fn render_ascii_table(columns: &[String], rows: &[Vec<Option<String>>]) -> String {
    let cell = |v: &Option<String>| v.clone().unwrap_or_else(|| "NULL".to_string());

    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell(v).len());
        }
    }

    let write_row = |out: &mut String, cells: &[String]| {
        out.push('|');
        for (i, c) in cells.iter().enumerate() {
            out.push_str(&format!(" {:<width$} |", c, width = widths[i]));
        }
        out.push('\n');
    };

    let mut out = String::new();
    write_row(&mut out, columns);

    out.push('+');
    for w in &widths {
        out.push_str(&"-".repeat(w + 2));
        out.push('+');
    }
    out.push('\n');

    for row in rows {
        let cells: Vec<String> = row.iter().map(cell).collect();
        write_row(&mut out, &cells);
    }

    out.push_str(&format!("({} row{})\n", rows.len(), if rows.len() == 1 { "" } else { "s" }));
    out
}

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    let store = Store::connect(&config.database_url, &config.redis_url)
        .await
        .expect("failed to connect to Postgres/Redis");

    let state = Arc::new(AppState { store, config });

    let app = Router::new()
        .route("/shorten", post(shorten))
        .route("/stats", get(stats))
        .route(
            "/{code}",
            get(redirect).patch(patch_link).delete(delete_link),
        )
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("port 3000 already in use?");

    println!("tier-4.3-caching listening on 0.0.0.0:3000");
    println!("minting short URLs as {}/{{code}}", state.config.base_url);
    println!();
    println!("  curl -X POST localhost:3000/shorten -H 'content-type: application/json' \\");
    println!("       -d '{{\"url\":\"https://example.com\"}}'");
    println!("  curl -i localhost:3000/0");

    axum::serve(listener, app).await.expect("server error");
}
