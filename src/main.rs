//! den-atlas — the Rust serving layer (RUST-1). Streams the derived dataset from disk with the full caching
//! layer (ETag / Range / gzip / conditional). Data is mounted at `ATLAS_DATA_DIR` (default `data/`).

mod dataset;
mod descriptor;
mod handler;
mod http;
mod manifest;
mod util;

use std::sync::Arc;

pub struct AppState {
    pub dataset: dataset::Dataset,
    /// Override the origin used in descriptor blob URLs (else derived from X-Forwarded-* / Host).
    pub public_base: Option<String>,
}

#[tokio::main]
async fn main() {
    let dir = std::env::var("ATLAS_DATA_DIR").unwrap_or_else(|_| "data".to_owned());
    let dataset = match dataset::Dataset::load(std::path::Path::new(&dir)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("den-atlas: failed to load dataset from {dir}: {e}");
            std::process::exit(1);
        }
    };
    let count = dataset.meta.count;
    let em = dataset.meta.embedding_model.clone();
    let tv = dataset.meta.taxonomy_version.clone();

    let state = Arc::new(AppState {
        dataset,
        public_base: std::env::var("PUBLIC_BASE_URL").ok(),
    });

    let app = axum::Router::new()
        .fallback(handler::handle)
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| {
            eprintln!("den-atlas: bind :{port} failed: {e}");
            std::process::exit(1);
        });
    eprintln!("den-atlas listening on :{port} — {count} titles ({em}/{tv})");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("den-atlas: serve error: {e}");
        std::process::exit(1);
    }
}
