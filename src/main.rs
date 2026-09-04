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
    // Before the readiness line below, so nothing can be told we are up while a stop signal would
    // still be a hard kill.
    let shutdown = shutdown_signal();
    // The port it actually BOUND, not the one it was asked for: with PORT=0 the kernel picks one,
    // and reporting the request rather than the result made the line useless in exactly that case.
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    match &state.dataset {
        Some(ds) => eprintln!(
            "den-atlas listening on :{port} — {} titles ({}/{})",
            ds.meta.count, ds.meta.embedding_model, ds.meta.taxonomy_version
        ),
        None => eprintln!("den-atlas listening on :{port} — dataset unavailable (catalog only)"),
    }
    let outcome = serve_until(listener, app, shutdown, DRAIN_GRACE).await;
    eprintln!("den-atlas: {}", outcome.describe());
    let code = outcome.exit_code();
    if code != 0 {
        std::process::exit(code);
    }
}

/// How serving ended. Separate from the message because the exit code differs: a drain that ran out
/// of time is expected, a serve error is not.
#[derive(Debug)]
enum Outcome {
    Drained,
    DeadlineHit(String),
    Failed(String),
}

impl Outcome {
    /// A drain that ran out of time is a DESIGNED outcome, so it exits 0.
    ///
    /// Exiting non-zero put the unit into `failed` with `Result=exit-code`, and any unauthenticated
    /// client can trigger it — thirty bytes of an unterminated request head is enough — so anything
    /// watching unit state or container exit codes could be made to see every routine restart as a
    /// crash. Only a real serve failure is a failure.
    fn exit_code(&self) -> i32 {
        match self {
            Outcome::Drained | Outcome::DeadlineHit(_) => 0,
            Outcome::Failed(_) => 1,
        }
    }

    fn describe(&self) -> String {
        match self {
            Outcome::Drained => "shut down cleanly".to_owned(),
            Outcome::DeadlineHit(why) | Outcome::Failed(why) => why.clone(),
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
) -> Outcome {
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.await;
        let _ = signalled_tx.send(());
    });
    tokio::select! {
        r = serve => match r {
            Ok(()) => Outcome::Drained,
            Err(e) => Outcome::Failed(format!("serve error: {e}")),
        },
        _ = async {
            // The clock starts when the signal ARRIVES, not when the server does. A dropped sender
            // resolves this too, which is only safe because axum holds the shutdown future for the
            // life of the process — if that ever changed, a healthy idle server would start
            // counting down to exit. `assert_serves_while_shutdown_never_fires` pins it.
            let _ = signalled_rx.await;
            tokio::time::sleep(grace).await;
        } => Outcome::DeadlineHit(format!("drain deadline ({grace:?}) reached with requests still in flight")),
    }
}

/// How long in-flight requests get to finish after a stop signal.
///
/// Every real request is milliseconds; the long tail is a blob download, which can legitimately run
/// for minutes on a slow link. Waiting for that tail is not worth it: a restart is rare, blob
/// requests are resumable (`Range` + `If-Range`, both honoured), and the alternative is letting one
/// slow or stuck client decide how long the addon is down.
///
/// Under podman's DEFAULT 10s stop timeout, deliberately. The image auto-updates daily on a tag
/// while the quadlet's own `--stop-timeout` only lands when someone re-runs the provisioner, so for
/// some window this binary runs on a box that still kills at 10s. Sized to fit that window, the
/// drain works everywhere and the container setting is headroom rather than a prerequisite.
const DRAIN_GRACE: Duration = Duration::from_secs(8);

// Its whole reason for being 8 is that it must finish before an external stop timeout kills us, and
// the smallest one that can apply is podman's and docker's default 10s. A test only guards what
// someone remembers to run, and every test here passes its own grace, so raising this to 600 changed
// nothing anywhere. This fails the build.
const _: () = assert!(DRAIN_GRACE.as_secs() < 10);

/// Resolves when the process is asked to stop.
///
/// Without this the binary is PID 1 in a `scratch` image, and PID 1 gets no default terminate
/// action — SIGTERM is simply ignored, so `podman restart` waited its full StopTimeout and then
/// SIGKILLed: a measured 10.03s of downtime on every dataset refresh, every auto-update and every
/// reboot, with each in-flight response cut mid-body. Blob downloads legitimately run past a
/// minute on a slow link, so that is a real truncation, not a theoretical one.
///
/// SIGINT as well, so a foreground `docker run` in a terminal behaves the same way.
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    // Registered NOW, by the caller, not lazily when the future is first polled. Polling starts once
    // the server is already accepting, and until then SIGTERM keeps its default disposition — so a
    // stop arriving in that window killed the process outright rather than draining it. Narrow, but
    // it is exactly the window a supervisor uses when a unit is restarted immediately after start.
    let term = signal(SignalKind::terminate());
    let int = signal(SignalKind::interrupt());
    async move {
        tokio::select! {
            _ = wait_for(term, "SIGTERM") => {}
            _ = wait_for(int, "SIGINT") => {}
        }
        // A SECOND signal ends it now. Both handles above are dropped by here, and tokio does not
        // restore the default disposition when a `Signal` drops — so every later SIGTERM and ^C was
        // caught and discarded, and an operator could not get out of the drain short of SIGKILL.
        // Re-registering makes the usual "press it again" work; exit 0, because asking twice is a
        // deliberate choice, not a failure.
        //
        // Silent, not via `wait_for`: that arm announces "draining in-flight requests", which is the
        // opposite of what this does.
        tokio::spawn(async move {
            tokio::select! {
                _ = quietly(signal(SignalKind::terminate())) => {}
                _ = quietly(signal(SignalKind::interrupt())) => {}
            }
            eprintln!("den-atlas: second signal — exiting without finishing the drain");
            std::process::exit(0);
        });
    }
}

/// Like `wait_for`, but says nothing — for a caller that prints its own, different message.
async fn quietly(registered: std::io::Result<tokio::signal::unix::Signal>) {
    match registered {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

/// Resolve when this signal arrives, or never if it could not be registered.
///
/// Never, specifically: returning immediately would shut the server down the moment it started, so
/// an unregisterable signal has to behave as it did before there was a handler at all — a hard kill.
/// The two signals are registered INDEPENDENTLY by the caller, because handling them as a pair meant
/// one failure discarded the other, and a dropped `Signal` leaves the signal caught-and-discarded
/// rather than restoring the default — so ^C would have stopped working entirely.
async fn wait_for(registered: std::io::Result<tokio::signal::unix::Signal>, name: &str) {
    match registered {
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

        assert!(
            matches!(&outcome, Outcome::DeadlineHit(w) if w.contains("drain deadline")),
            "the deadline did not fire: {outcome:?}"
        );
        assert!(took < grace * 4, "shutdown took {took:?}, far past the {grace:?} grace");
        drop(sock);
    }

    /// Only a real serve failure is a failure. A drain that hit its deadline is a designed outcome,
    /// and an unauthenticated client can cause it at will — exiting non-zero there let anyone make
    /// every restart of the service look like a crash to a monitor.
    #[test]
    fn only_a_serve_error_exits_non_zero() {
        assert_eq!(Outcome::Drained.exit_code(), 0);
        assert_eq!(
            Outcome::DeadlineHit("deadline".into()).exit_code(),
            0,
            "a routine drain timeout looked like a crash"
        );
        assert_eq!(
            Outcome::Failed("bind failed".into()).exit_code(),
            1,
            "a real failure was reported as success"
        );
        // ...and each still says what happened.
        assert!(Outcome::Drained.describe().contains("cleanly"));
        assert_eq!(Outcome::DeadlineHit("deadline".into()).describe(), "deadline");
        assert_eq!(Outcome::Failed("bind failed".into()).describe(), "bind failed");
    }

    /// A signal that cannot be registered must never resolve. Returning immediately instead turns
    /// the server into a crash loop — it would shut down microseconds after boot, every time — and
    /// that mutation passed the entire suite, because nothing exercised this arm at all.
    #[tokio::test]
    async fn an_unregisterable_signal_never_fires() {
        let err = Err(std::io::Error::other("no signal handler here"));
        let fired = tokio::time::timeout(Duration::from_millis(200), wait_for(err, "SIGTERM")).await;
        assert!(
            fired.is_err(),
            "an unregisterable signal resolved, which would shut the server down at boot"
        );
    }

    /// ...and one that CAN be registered resolves when it arrives.
    #[tokio::test]
    async fn a_registered_signal_fires_when_raised() {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGUSR2: nothing else in the process uses it, so raising it cannot disturb the test runner.
        let reg = signal(SignalKind::user_defined2()).expect("registration must work on this platform");
        let waiter = tokio::spawn(async move { wait_for(Ok(reg), "SIGUSR2").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        unsafe { libc_raise() };
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("a registered signal never resolved")
            .unwrap();
    }

    /// `raise(SIGUSR2)` without pulling in a libc dependency for one call.
    unsafe fn libc_raise() {
        extern "C" {
            fn raise(sig: i32) -> i32;
        }
        // SIGUSR2 is 12 on Linux, 31 on macOS/BSD.
        let sig = if cfg!(target_os = "linux") { 12 } else { 31 };
        raise(sig);
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
        assert!(matches!(outcome, Outcome::Drained), "a clean drain reported otherwise: {outcome:?}");
        assert!(started.elapsed() < grace, "an idle drain waited out the grace: {:?}", started.elapsed());
    }
}
