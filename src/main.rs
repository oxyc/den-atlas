//! den-atlas — the Rust serving layer (RUST-1). Streams the derived dataset from disk with the full caching
//! layer (ETag / Range / gzip / conditional). Data is mounted at `ATLAS_DATA_DIR` (default `data/`).

mod cache;
mod catalog;
mod config;
mod dataset;
mod descriptor;
mod handler;
mod http;
mod justwatch;
mod manifest;
mod util;

use std::sync::Arc;
use std::time::Duration;

pub struct AppState {
    /// `None` when the dataset couldn't be loaded (old/missing meta) — the addon still serves manifest +
    /// catalog and returns 503 on the dataset routes.
    pub dataset: Option<dataset::Dataset>,
    /// Override the origin used in descriptor blob URLs (else derived from X-Forwarded-* / Host).
    pub public_base: Option<String>,
    /// JustWatch "most popular" catalog rows (public, isolated from the dataset resource).
    pub catalog: catalog::CatalogState,
    /// The operator-default country (env `JW_COUNTRY`) — used when an `auto` install forwards no
    /// `country` extra. A fixed-country install ignores it.
    pub default_country: String,
    /// Upstream den-embed for the `/embed` search proxy (env `DEN_EMBED_URL`). `None` disables search embeds.
    pub embed: Option<EmbedProxy>,
}

#[cfg(test)]
impl AppState {
    /// Minimal state for a route test: no embed proxy, no origin override, so the descriptor's URLs
    /// come from the request headers — which is the case the origin `Vary` exists for.
    pub fn for_test(dataset: Option<dataset::Dataset>) -> Self {
        AppState {
            dataset,
            public_base: None,
            catalog: catalog::CatalogState::new(
                std::sync::Arc::new(justwatch::JustWatchClient::new()),
                std::time::Duration::from_secs(3600),
            ),
            default_country: "US".to_owned(),
            embed: None,
        }
    }
}

/// Search query-embed proxy. den-atlas never runs the model — it forwards to the single den-embed authority
/// so a search query embeds through the SAME bge-m3 + int8 quantizer that built the corpus (the alignment
/// rule). den-embed stays internal; the app only ever talks to den-atlas.
pub struct EmbedProxy {
    pub client: reqwest::Client,
    pub base: String,
}

#[tokio::main]
async fn main() {
    let dir = std::env::var("ATLAS_DATA_DIR").unwrap_or_else(|_| "data".to_owned());
    // Fail-soft: the manifest + catalog resources don't need the dataset, so a missing/old-format
    // dataset.meta.json must not crash-loop the addon. Keep serving; the dataset routes report 503 and
    // the app surfaces a real reason instead of a connection refusal.
    let dataset = match dataset::Dataset::load(std::path::Path::new(&dir)) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("den-atlas: dataset unavailable ({e}) — serving manifest + catalog only until the data is refreshed (scripts/fetch-dataset.sh)");
            None
        }
    };

    let default_country = std::env::var("JW_COUNTRY").unwrap_or_else(|_| "US".to_owned());
    let ttl = Duration::from_secs(
        std::env::var("JW_CACHE_TTL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(21_600),
    );
    let catalog = catalog::CatalogState::new(Arc::new(justwatch::JustWatchClient::new()), ttl);

    // Optional query-embed proxy → den-embed. Absent env ⇒ search embeds are disabled (503), dataset serving
    // is unaffected. A short timeout: a query embed is a fast single call, not the slow corpus build.
    let embed = std::env::var("DEN_EMBED_URL").ok().and_then(|base| {
        match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
            Ok(client) => Some(EmbedProxy { client, base: base.trim_end_matches('/').to_owned() }),
            Err(e) => {
                eprintln!("den-atlas: embed proxy disabled (reqwest build failed: {e})");
                None
            }
        }
    });

    let state = Arc::new(AppState {
        dataset,
        public_base: std::env::var("PUBLIC_BASE_URL").ok(),
        catalog,
        default_country,
        embed,
    });

    let app = axum::Router::new()
        .fallback(handler::handle)
        .with_state(Arc::clone(&state));

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
    match &state.dataset {
        Some(ds) => eprintln!(
            "den-atlas listening on :{port} — {} titles ({}/{})",
            ds.meta.count, ds.meta.embedding_model, ds.meta.taxonomy_version
        ),
        None => eprintln!("den-atlas listening on :{port} — dataset unavailable (catalog only)"),
    }
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("den-atlas: serve error: {e}");
        std::process::exit(1);
    }
}
