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
    match serve_until(listener, app, shutdown_signal(), DRAIN_GRACE).await {
        None => eprintln!("den-atlas: shut down cleanly"),
        Some(why) => {
            eprintln!("den-atlas: {why}");
            std::process::exit(1);
        }
    }
}

/// Serve until `shutdown` resolves, then drain for at most `grace`. `None` on a clean drain, or the
/// reason it ended otherwise.
///
/// The bound is the point. `with_graceful_shutdown` waits for every connection task, and hyper waits
/// on a connection that is mid-request — but there is no header-read timeout and no request deadline
/// anywhere, so a client that opens a socket and sends half a request head holds the process open
/// for as long as it likes. Measured before this: an unterminated
/// `GET /health HTTP/1.1\r\nHost: x\r\n` was still holding shutdown at 60 seconds. The listener is
/// released the moment the signal arrives, so that is pure downtime, and it was decided by an
/// arbitrary client rather than by us — worse than the fixed 10s hard kill it replaced.
///
/// `shutdown` is a parameter rather than a direct call to `shutdown_signal()` so a test can trigger
/// the drain without signalling the test runner itself.
async fn serve_until(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> Option<String> {
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.await;
        let _ = signalled_tx.send(());
    });
    tokio::select! {
        r = serve => r.err().map(|e| format!("serve error: {e}")),
        _ = async {
            // The clock starts when the signal arrives, not when the server does.
            let _ = signalled_rx.await;
            tokio::time::sleep(grace).await;
        } => Some(format!("drain deadline ({grace:?}) reached with requests still in flight")),
    }
}

/// How long in-flight requests get to finish after a stop signal.
///
/// Every real request is milliseconds; the long tail is a blob download, which can legitimately run
/// for minutes on a slow link. Waiting for that tail is not worth it: a restart is rare, blob
/// requests are resumable (`Range` + `If-Range`, both honoured), and the alternative is letting one
/// slow or stuck client decide how long the addon is down. The container's stop timeout sits above
/// this so podman never preempts the drain — but this, not podman, is what bounds it.
const DRAIN_GRACE: Duration = Duration::from_secs(15);

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

    /// One signal, or a future that never resolves if it could not be registered — returning
    /// immediately would shut the server down the moment it started.
    ///
    /// Registered INDEPENDENTLY. Handling them as a pair meant one failure discarded the other, and
    /// tokio does not restore the default disposition when a `Signal` is dropped — so a registered
    /// SIGINT would have become caught-and-discarded, and ^C on a foreground `docker run` would
    /// stop working. Whichever one registers still does its job.
    async fn on(kind: SignalKind, name: &str) {
        match signal(kind) {
            Ok(mut sig) => {
                sig.recv().await;
                eprintln!("den-atlas: {name} — draining in-flight requests");
            }
            Err(e) => {
                eprintln!("den-atlas: {name} handler unavailable ({e}); it will be a hard kill");
                std::future::pending::<()>().await
            }
        }
    }

    tokio::select! {
        _ = on(SignalKind::terminate(), "SIGTERM") => {}
        _ = on(SignalKind::interrupt(), "SIGINT") => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        (l, a)
    }

    fn app() -> axum::Router {
        axum::Router::new().fallback(handler::handle).with_state(Arc::new(AppState::for_test(None)))
    }

    /// A client that opens a socket and sends HALF a request head held the whole process open —
    /// measured still running at 60s — because nothing in the stack times out a partial head. The
    /// listener is released immediately, so every second of that is downtime chosen by whoever
    /// opened the socket, and the container stop timeout was the only bound.
    #[tokio::test]
    async fn a_client_holding_a_partial_request_cannot_hold_shutdown_open() {
        let (l, addr) = listener().await;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let grace = Duration::from_millis(300);
        let server =
            tokio::spawn(async move { serve_until(l, app(), async { rx.await.unwrap_or(()) }, grace).await });

        // Head deliberately unterminated: no blank line, so hyper is still waiting for the rest.
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n").await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let _ = tx.send(());
        let outcome = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown never returned — the drain is unbounded again")
            .unwrap();
        let took = started.elapsed();

        assert!(outcome.is_some_and(|w| w.contains("drain deadline")), "the deadline did not fire");
        assert!(took < grace * 4, "shutdown took {took:?}, far past the {grace:?} grace");
        drop(sock);
    }

    /// The grace clock starts at the SIGNAL, not at boot, and costs nothing when there is nothing to
    /// drain.
    ///
    /// Both halves matter. A clock started at boot would end the process one grace period into
    /// ordinary uptime — so this server is deliberately left running for longer than its own grace
    /// before anything is asked of it, and is expected to answer normally afterwards.
    #[tokio::test]
    async fn the_grace_starts_at_the_signal_and_costs_nothing_when_idle() {
        let (l, addr) = listener().await;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let grace = Duration::from_millis(200);
        let server =
            tokio::spawn(async move { serve_until(l, app(), async { rx.await.unwrap_or(()) }, grace).await });

        // Well past the grace, with no signal sent. The server must still be serving.
        tokio::time::sleep(grace * 5).await;
        assert!(
            !server.is_finished(),
            "the server exited during ordinary uptime — the grace clock started at boot"
        );

        // One completed request, so a connection has actually been handled.
        let (mut sock, _) = (tokio::net::TcpStream::connect(addr).await.unwrap(), ());
        sock.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut sock, &mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("200 OK"));

        let started = std::time::Instant::now();
        let _ = tx.send(());
        let outcome = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("an idle server never shut down")
            .unwrap();
        assert!(outcome.is_none(), "a clean drain reported a failure: {outcome:?}");
        assert!(started.elapsed() < grace, "an idle drain waited out the grace: {:?}", started.elapsed());
    }
}
