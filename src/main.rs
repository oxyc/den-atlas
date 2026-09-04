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

/// A `TrendingSource` that answers nothing, so a test can never reach the network by accident.
/// Every method fails the way a real upstream failure does, which is a state the catalog already
/// degrades through.
#[cfg(test)]
struct OfflineSource;

#[cfg(test)]
#[async_trait::async_trait]
impl justwatch::TrendingSource for OfflineSource {
    async fn popular(
        &self,
        _p: &str,
        _o: justwatch::ObjectType,
        _c: &str,
        _s: &str,
    ) -> Result<Vec<justwatch::TrendingItem>, ()> {
        Err(())
    }
    async fn new_titles(
        &self,
        _p: &str,
        _o: justwatch::ObjectType,
        _c: &str,
    ) -> Result<Vec<justwatch::TrendingItem>, ()> {
        Err(())
    }
    async fn packages(&self, _c: &str) -> Result<Vec<(i64, String)>, ()> {
        Err(())
    }
}

#[cfg(test)]
impl AppState {
    /// Minimal state for a route test: no embed proxy, no origin override, so the descriptor's URLs
    /// come from the request headers — which is the case the origin `Vary` exists for.
    pub fn for_test(dataset: Option<dataset::Dataset>) -> Self {
        AppState {
            dataset,
            public_base: None,
            // An OFFLINE source, not a real client. `JustWatchClient::new()` here pointed every
            // route test at apis.justwatch.com: any test that touched /catalog would have made
            // live requests without naming the host anywhere, which is plausibly how an audit
            // probe once earned this host a 403. Tests that need catalog data pass their own fake
            // through `for_test_with_source`.
            catalog: catalog::CatalogState::new(
                std::sync::Arc::new(OfflineSource),
                std::time::Duration::from_secs(3600),
            ),
            default_country: "US".to_owned(),
            embed: None,
        }
    }

    /// Same, but with the catalog's upstream replaced. Needed because a server-side request deadline
    /// lives in `handle`, so only a test that goes through `handle` with a slow upstream can see it —
    /// a catalog-level burst test cannot, which is how a 20s deadline shipped with zero coverage.
    pub fn for_test_with_source(source: std::sync::Arc<dyn justwatch::TrendingSource>) -> Self {
        AppState {
            catalog: catalog::CatalogState::new(source, std::time::Duration::from_secs(3600)),
            ..AppState::for_test(None)
        }
    }
}

/// Search query-embed proxy. den-atlas never runs the model — it forwards to the single den-embed authority
/// so a search query embeds through the SAME bge-m3 + int8 quantizer that built the corpus (the alignment
/// rule). den-embed stays internal; the app only ever talks to den-atlas.
pub struct EmbedProxy {
    pub client: reqwest::Client,
    pub base: String,
    /// The same bound the JustWatch path has, for the same reason: this is an unauthenticated public
    /// POST that fans out 1:1 to the internal model service, and each call holds a 10s timeout.
    /// Without it, N concurrent requests are N concurrent model invocations.
    pub inflight: std::sync::Arc<tokio::sync::Semaphore>,
}

/// Concurrent embeds allowed through to den-embed. A search query is interactive, so a short queue
/// with a deadline beats an unbounded one.
pub const MAX_EMBED_INFLIGHT: usize = 4;
pub const EMBED_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

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
            Ok(client) => Some(EmbedProxy {
                client,
                base: base.trim_end_matches('/').to_owned(),
                inflight: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_EMBED_INFLIGHT)),
            }),
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

    let app = axum::Router::new().fallback(handler::handle).with_state(Arc::clone(&state));

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap_or_else(|e| {
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
    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = serve.await {
        eprintln!("den-atlas: serve error: {e}");
        std::process::exit(1);
    }
    eprintln!("den-atlas: shut down cleanly");
}

/// Resolves when the process is asked to stop.
///
/// Without this the binary is PID 1 in a `scratch` image, and PID 1 gets no default terminate
/// action — SIGTERM is simply ignored, so `podman restart` waited its full StopTimeout and then
/// SIGKILLed: a measured 10.03s of downtime on every dataset refresh, every auto-update and every
/// reboot, with each in-flight response cut mid-body. Blob downloads legitimately run past a
/// minute on a slow link, so that is a real truncation, not a theoretical one.
///
/// SIGINT as well, so a foreground `docker run` in a terminal behaves the same way.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    // A handler that cannot be registered must never resolve: returning immediately would shut the
    // server down the moment it started. Never resolving is the old behaviour — a hard kill — which
    // is bad but not self-inflicted.
    let (mut term, mut int) = match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
        (Ok(t), Ok(i)) => (t, i),
        (t, i) => {
            let e = t.err().or_else(|| i.err()).map(|e| e.to_string()).unwrap_or_default();
            eprintln!("den-atlas: signal handlers unavailable ({e}); shutdown will be a hard kill");
            return std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = term.recv() => eprintln!("den-atlas: SIGTERM — draining in-flight requests"),
        _ = int.recv() => eprintln!("den-atlas: SIGINT — draining in-flight requests"),
    }
}
