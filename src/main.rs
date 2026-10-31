//! den-atlas — the Rust serving layer (RUST-1). Streams the derived dataset from disk with the full caching
//! layer (ETag / Range / gzip / conditional). Data is mounted at `DATA_DIR` (default `data/`).

mod admission;
mod awards;
mod billboardcheck;
mod billboardeval;
mod billing;
mod cache;
mod catalog;
mod characters;
mod config;
mod dataset;
mod descriptor;
mod facts;
mod filter;
mod fit;
mod franchisecheck;
mod franchises;
mod handler;
mod heading;
mod http;
mod justwatch;
mod kids_home;
mod manifest;
mod maturity;
mod maturity_providers;
mod metrics;
mod motn;
mod playground;
mod plotrows;
mod queries;
mod railab;
mod raileval;
mod ratings;
mod recommend;
mod schema;
mod search;
mod series;
mod store;
mod studios;
mod titles;
mod tmdb;
mod tos;
mod tvmaze;
mod util;
mod versions;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The release the running image carries (`DEN_VERSION`, set from the repo's `VERSION` file at image build).
/// Keeping it out of Cargo.toml means a release commit does not invalidate the Rust build layer merely to
/// change its tag. A developer build or test has no image metadata and falls back to the package version.
pub(crate) fn release_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            std::env::var("DEN_VERSION")
                .ok()
                .filter(|version| !version.is_empty())
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
        })
        .as_str()
}

pub struct AppState {
    /// Separate bounds for synchronous local ranking and POST body aggregation. Catalog cold-fill has its own
    /// admission and deliberately never touches these gates.
    pub admission: admission::Admission,
    /// `None` when the dataset couldn't be loaded (old/missing meta) — the addon still serves manifest +
    /// catalog and returns 503 on the dataset routes.
    pub dataset: Option<dataset::Dataset>,
    /// JustWatch "most popular" catalog rows (public, isolated from the dataset resource).
    pub catalog: catalog::CatalogState,
    /// The operator-default country (env `JW_COUNTRY`) — used when an `auto` install forwards no
    /// `country` extra. A fixed-country install ignores it.
    pub default_country: String,
    /// Upstream den-embed for the `/embed` search proxy (env `EMBED_URL`). `None` disables search embeds.
    pub embed: Option<EmbedProxy>,
    /// Search query vectors den-embed already gave, by query text and embedding space.
    pub embed_memo: cache::EmbedMemo,
    /// Unindexed library-title vectors, persisted under `CACHE_DIR`. Unlike search queries its keys contain
    /// no prose: title identity plus a hash of the embed text, so changed hints naturally miss.
    pub library_embed_memo: cache::EmbedMemo,
    /// Fuzzy title search over TMDB's daily exports (env `TITLE_SEARCH`). `None` — off — declares no
    /// search catalog.
    pub titles: Option<Arc<titles::TitleSearch>>,
    /// Index queries over the dataset (env `INDEX_QUERIES`, and a loaded dataset). `None` — off — 404s the
    /// `/index/…` routes.
    pub index: Option<Arc<queries::IndexQueries>>,
    /// Box-local, refreshable maturity authorities (`MATURITY_PROVIDERS`). Their normalized cache lives only
    /// in `CACHE_DIR`; it is never part of the public dataset or a downloadable atlas artifact.
    pub maturity_providers: Arc<maturity_providers::Providers>,
    /// Movie of the Night's lists (env `MOTN_KEY`): each service's Top 10 and additions, per country. Without a key
    /// it keeps nothing, and rows and billboards read JustWatch alone.
    pub motn: Arc<motn::Motn>,
    /// TVmaze's episode dates for the series households follow (env `TVMAZE`, on unless `0`), served by
    /// `GET /airing/tv/<tmdbId>`.
    pub tvmaze: Arc<tvmaze::Tvmaze>,
    /// Bearer token for `/metrics` (env `METRICS_TOKEN`). `None` — unset or empty — turns the route off.
    pub metrics_token: Option<String>,
    /// One stderr line per request (env `LOG_REQUESTS`: off when unset, empty or `0`, on for anything
    /// else — the rule every den addon uses). Read once at startup, so with it off the request path pays
    /// a single bool check.
    pub log_requests: bool,
    /// Whether `/recommend`'s decision log (`recommend::summary`) names the titles it ranked — env
    /// `LOG_IDENTITY`: on by default, `0`/`false`/`off`/`no` (case-insensitive) turns it off. The
    /// opposite default from `LOG_REQUESTS`, because this trims fields WITHIN a line that is written
    /// either way, not the line itself.
    pub log_identity: bool,
    /// The tuning playground (env `PLAYGROUND`, same on/off rule as `LOG_REQUESTS`; needs `INDEX_QUERIES`).
    /// Off — the default — 404s every `/playground…` path (`playground.rs`).
    pub playground: bool,
    /// The `/health` reason last logged (`ok` when healthy), so a change is logged once rather than by
    /// every request that observes it.
    pub health: std::sync::Mutex<&'static str>,
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
    /// Minimal state for a route test: no embed proxy, no index queries, an offline catalog source.
    pub fn for_test(dataset: Option<dataset::Dataset>) -> Self {
        AppState {
            admission: admission::Admission::new(4, 8, 4, 8, admission::WAIT),
            dataset,
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
            embed_memo: cache::EmbedMemo::new(cache::EMBED_MEMO_TTL, cache::EMBED_MEMO_ENTRIES),
            library_embed_memo: cache::EmbedMemo::new(Duration::MAX, cache::LIBRARY_EMBED_MEMO_ENTRIES),
            titles: None,
            index: None,
            maturity_providers: maturity_providers::Providers::disabled(),
            motn: std::sync::Arc::new(motn::Motn::new(None, None)),
            tvmaze: std::sync::Arc::new(tvmaze::Tvmaze::off()),
            metrics_token: None,
            log_requests: false,
            log_identity: true,
            playground: false,
            health: std::sync::Mutex::new("ok"),
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

/// One embedding authority behind Atlas's existing bounded HTTP-facing facade. The default stays the external
/// den-embed service during migration. An `embedded-model` build may opt into the in-process service, which is
/// built from the same workspace package and the same HTTP router as the compatibility den-embed binary.
#[derive(Clone)]
pub struct EmbedProxy {
    transport: EmbedTransport,
    /// The same bound the JustWatch path has, for the same reason: this is an unauthenticated public
    /// POST that fans out 1:1 to the internal model service, and each call holds a 10s timeout.
    /// Without it, N concurrent requests are N concurrent model invocations.
    pub inflight: std::sync::Arc<tokio::sync::Semaphore>,
    readiness: Arc<std::sync::Mutex<EmbedReadiness>>,
}

#[derive(Clone)]
enum EmbedTransport {
    Remote {
        client: reqwest::Client,
        base: String,
    },
    #[cfg(feature = "embedded-model")]
    Local(den_embed::EmbeddedService),
}

#[derive(Clone, Debug)]
enum EmbedReadiness {
    #[cfg(feature = "embedded-model")]
    Checking,
    Ready,
    #[cfg(feature = "embedded-model")]
    Failed,
}

pub struct EmbedAnswer {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: bytes::Bytes,
}

impl EmbedProxy {
    fn available(&self) -> bool {
        matches!(*crate::util::lock(&self.readiness), EmbedReadiness::Ready)
    }

    fn unready(&self) -> bool {
        !self.available()
    }

    pub async fn call(
        &self,
        path: &str,
        body: bytes::Bytes,
        request_id: Option<&str>,
    ) -> Result<EmbedAnswer, String> {
        if !self.available() {
            return Err("embedding engine is not ready".into());
        }
        self.call_raw(path, body, request_id).await
    }

    async fn call_raw(
        &self,
        path: &str,
        body: bytes::Bytes,
        request_id: Option<&str>,
    ) -> Result<EmbedAnswer, String> {
        match &self.transport {
            EmbedTransport::Remote { client, base } => {
                let mut request = client
                    .post(format!("{base}{path}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(body);
                if let Some(request_id) = request_id {
                    request = request.header("x-request-id", request_id);
                }
                let response = request.send().await.map_err(|error| error.to_string())?;
                let status = axum::http::StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
                let headers = response.headers().clone();
                // Preserve the old proxy's behaviour for an upstream that closes mid-body: its status and
                // headers still reached the caller with an empty JSON body instead of becoming an Atlas 502.
                let body = response.bytes().await.unwrap_or_default();
                Ok(EmbedAnswer { status, headers, body })
            }
            #[cfg(feature = "embedded-model")]
            EmbedTransport::Local(service) => {
                use tower::ServiceExt;

                let mut request = axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri(path)
                    .header(axum::http::header::CONTENT_TYPE, "application/json");
                if let Some(request_id) = request_id {
                    request = request.header("x-request-id", request_id);
                }
                let call = service
                    .router()
                    .oneshot(request.body(axum::body::Body::from(body)).map_err(|error| error.to_string())?);
                let response = tokio::time::timeout(EMBED_CALL_TIMEOUT, call)
                    .await
                    .map_err(|_| "embedded model timed out".to_owned())?
                    .map_err(|error| error.to_string())?;
                let (parts, body) = response.into_parts();
                let body = axum::body::to_bytes(body, MAX_LOCAL_EMBED_RESPONSE_BYTES)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(EmbedAnswer { status: parts.status, headers: parts.headers, body })
            }
        }
    }

    #[cfg(feature = "embedded-model")]
    async fn canary(&self) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct Answer {
            vector: Vec<i8>,
            dims: usize,
            model: String,
        }
        let body = serde_json::to_vec(&serde_json::json!({"text":"Den integrated embedding readiness"}))
            .map_err(|error| error.to_string())?;
        let answer = self.call_raw("/embed", body.into(), None).await?;
        if !answer.status.is_success() {
            return Err(format!("engine answered HTTP {}", answer.status));
        }
        let answer: Answer = serde_json::from_slice(&answer.body).map_err(|error| error.to_string())?;
        if answer.model != "bge-m3" || answer.dims != 1024 || answer.vector.len() != answer.dims {
            return Err("engine canary returned the wrong identity or width".into());
        }
        Ok(())
    }
}

/// Concurrent embeds allowed through to den-embed. A search query is interactive, so a short queue
/// with a deadline beats an unbounded one.
pub const MAX_EMBED_INFLIGHT: usize = 4;
// At the engine's absolute MAX_BATCH=4096, a response of 1024 zero-valued dimensions is about 8 MiB.
// Leave framing/headroom, but do not let an in-process compatibility response allocate an arbitrary body.
#[cfg(feature = "embedded-model")]
const MAX_LOCAL_EMBED_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
pub const EMBED_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const EMBED_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `den-atlas check <dir>` — load a dataset directory exactly as serving would, and say whether it is fit to
/// swap in.
///
/// Two outcomes:
///
///   0  fit to serve.
///   1  BROKEN — no store, a store that will not read, a mixed generation, a store whose facts or facet
///      rows do not read. Never swap this in; whatever is running is better.
///
/// There used to be a third, DEGRADED (2), for a dataset that declared no store: it served, and merely
/// ranked More Like This with the pre-pooled scorer, so `atlas-dataset-sync` could take it rather than
/// stay down through an interrupted swap. That halfway state is gone — the store is the only artifact
/// read, so a dataset without one serves nothing — and 2 is now unreachable. `atlas-dataset-sync` still
/// distinguishes the codes; this simply never returns that one.
///
/// This exists because a dataset can pass every check outside atlas and still be unusable by it. One entity
/// carrying `"aliases": "…"` where `RawEntity.aliases` is `Vec<String>` made a 27 MB facts file unparseable
/// at its first entity; atlas does not partially load one, so it dropped the whole file and served
/// `facts_unusable` — no people search, no imdbId, no countries, no /recommend — for nineteen minutes, and
/// only a log line said so.
///
/// The fetch script already refuses a half-published release. A release that is complete but that atlas
/// cannot read is the same kind of failure one layer down, and the same answer applies: do not swap it in.
/// Checking here rather than in the publisher is deliberate — this uses atlas's OWN deserialiser, so it
/// cannot drift from what serving actually requires the way a mirrored schema can.
fn check_dataset(dir: &std::path::Path) -> i32 {
    const BROKEN: i32 = 1;
    let dataset = match dataset::Dataset::load(dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("check: dataset at {} will not load: {e}", dir.display());
            return BROKEN;
        }
    };
    // The store. This check is what `atlas-dataset-sync` runs against a STAGED generation, so a bad one
    // is refused before it replaces a good one rather than after. `Dataset::load` above already mapped
    // and verified it; this reads that mapping the way SERVING does, which additionally builds the rail's
    // aggregates and proves every column the scorer reads.
    //
    // A dataset that declares no store used to be DEGRADED — it served, and merely ranked More Like This
    // with the pre-pooled scorer. There is no such halfway state left: the store is the only artifact
    // read, so a dataset without one has nothing behind any query route and `Dataset::load` refuses it
    // above. DEGRADED now has no way to fire; it is kept only so the exit codes `atlas-dataset-sync`
    // switches on keep their meanings.
    let loaded = match store::LoadedStore::of(Arc::clone(&dataset.mapped)) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("check: the dataset declares a store that atlas cannot read: {e}");
            return BROKEN;
        }
    };
    // A store whose version does not match the manifest that named it is a mixed generation — the
    // failure mode a moving release makes easy and nothing else here would notice.
    if loaded.store.dataset_version() != dataset.meta.dataset_version {
        eprintln!(
            "check: store says datasetVersion {} but the manifest says {} — mixed generation",
            loaded.store.dataset_version(),
            dataset.meta.dataset_version
        );
        return BROKEN;
    }
    // The two readers SERVING actually uses. `LoadedStore::open` proves the store maps and its rail
    // sections read; it does not touch the facts or facet-row sections, so a store missing `ent_qid` or
    // a `facet_*` axis opened cleanly, passed this check, and then degraded at load — the facts falling
    // through a fallback chain that no longer has a `factsFile` in it, and every browse row answering
    // empty. Both are exactly what this check exists to refuse before a swap.
    let view = loaded.view();
    if let Err(e) = facts::Facts::from_store(&view) {
        eprintln!("check: the store's facts do not read: {e} — refusing");
        return BROKEN;
    }
    if let Err(e) = crate::plotrows::PlotFacets::from_store(&view) {
        eprintln!("check: the store's facet rows do not read: {e} — refusing");
        return BROKEN;
    }
    println!(
        "check: ok (store: {} rows, {} bytes, facts and rows read)",
        loaded.store.rows(),
        loaded.store.bytes()
    );
    0
}

#[tokio::main]
async fn main() {
    let dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_owned());
    if let [_, command, path] = std::env::args().collect::<Vec<_>>().as_slice() {
        if command == "replay" {
            std::process::exit(replay(&dir, path).await);
        }
        if command == "check" {
            std::process::exit(check_dataset(std::path::Path::new(path)));
        }
        if command == "rail-ab" {
            std::process::exit(railab::run(std::path::Path::new(path)));
        }
        if command == "rail-eval" {
            std::process::exit(raileval::run(std::path::Path::new(path)).await);
        }
        if command == "franchise-check" {
            std::process::exit(franchisecheck::run(std::path::Path::new(path)).await);
        }
        if command == "billboard-check" {
            std::process::exit(billboardcheck::run(std::path::Path::new(path), false).await);
        }
        if command == "billboard-eval" {
            std::process::exit(billboardcheck::run(std::path::Path::new(path), true).await);
        }
        if command == "signal-coverage" {
            std::process::exit(awards::run_coverage(std::path::Path::new(path)));
        }
    }
    // Fail-soft: the manifest + catalog resources don't need the dataset, so a missing/old-format
    // dataset.meta.json must not crash-loop the addon. Keep serving; the dataset routes report 503 and
    // the app surfaces a real reason instead of a connection refusal.
    let dataset = match dataset::Dataset::load(std::path::Path::new(&dir)) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("dataset unavailable ({e}) — serving manifest + catalog only until the data is refreshed (scripts/fetch-dataset.sh)");
            None
        }
    };

    let default_country = std::env::var("JW_COUNTRY").unwrap_or_else(|_| "US".to_owned());
    let ttl = Duration::from_secs(
        std::env::var("JW_CACHE_TTL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(21_600),
    );
    let cache_dir = env_opt("CACHE_DIR");
    let mut catalog = catalog::CatalogState::new(Arc::new(justwatch::JustWatchClient::new()), ttl);
    if let Some(dir) = &cache_dir {
        catalog = catalog.kept_in(std::path::Path::new(dir));
    }
    let motn = Arc::new(motn::Motn::new(env_opt("MOTN_KEY"), cache_dir.as_deref().map(std::path::Path::new)));
    catalog = catalog.with_motn(Arc::clone(&motn));
    // A catalog row names TMDB's poster path beside JustWatch's metahub URL for every title the dataset
    // knows, so a client draws first-party art where there is any and keeps metahub for the rest. Read once
    // here rather than through the query indexes, which load after atlas listens (see `CatalogState::posters`).
    if let Some(ds) = dataset.as_ref() {
        // Read out of the mapping `Dataset::load` verified, into an owned map. It used to read the
        // metadata sidecar.
        match plotrows::posters_from_store(&ds.mapped.view()) {
            Ok(posters) => {
                eprintln!("catalog posters: {} titles from the store", posters.len());
                catalog = catalog.with_posters(Arc::new(posters));
            }
            Err(e) => eprintln!("catalog posters unavailable ({e}) — rows carry metahub art alone"),
        }
    }

    // External remains the default during migration. The local engine is compiled and selected separately;
    // merely merging this code neither loads the model nor changes the deployed request path.
    let embed = embed_proxy(dataset.as_ref());

    // Off unless TITLE_SEARCH is set (on for anything but empty or 0): the tvOS app fuses every addon search
    // catalog into its text search as soon as one is declared, so turning this on changes the TV's search.
    let title_search = std::env::var("TITLE_SEARCH")
        .is_ok_and(|v| !v.is_empty() && v != "0")
        .then(|| titles::TitleSearch::new(titles::EXPORT_BASE))
        .and_then(|built| {
            built.map_err(|e| eprintln!("title search disabled (reqwest build failed: {e})")).ok()
        })
        .map(Arc::new);
    let (maturity_daily_max, maturity_pace) = maturity_providers::defaults_from_env();
    let maturity_providers = match (dataset.as_ref(), env_opt("MATURITY_PROVIDERS")) {
        (Some(ds), Some(enabled)) => maturity_providers::Providers::new(
            &ds.mapped,
            cache_dir.as_deref().map(std::path::Path::new),
            &enabled,
            maturity_daily_max,
            maturity_pace,
        )
        .unwrap_or_else(|error| {
            eprintln!("maturity providers disabled ({error})");
            maturity_providers::Providers::disabled()
        }),
        _ => maturity_providers::Providers::disabled(),
    };

    // Off unless INDEX_QUERIES is set, and only over a dataset that loaded. The indexes themselves load on
    // the first query.
    let index = std::env::var("INDEX_QUERIES")
        .is_ok_and(|v| !v.is_empty() && v != "0")
        .then(|| {
            dataset.as_ref().map(|dataset| {
                queries::IndexQueries::new(dataset).with_maturity(Arc::clone(&maturity_providers))
            })
        })
        .flatten();
    // TMDB's vote counts, scores and credits for the corpus (`tmdb.rs`, whose top says what they may be used
    // for): the count every browse row is ordered by, the score /recommend rates a title with, and which
    // titles share a character. Kept in CACHE_DIR, and asked of den-edge's TMDB proxy when TMDB_PROXY names
    // it; without it atlas serves what is kept and asks for nothing. Only alongside the index routes: nothing
    // else reads them.
    let tmdb = index
        .is_some()
        .then(|| {
            dataset.as_ref().map(|ds| {
                tmdb::Tmdb::new(
                    Arc::clone(&ds.mapped),
                    cache_dir.as_deref().map(std::path::PathBuf::from),
                    env_opt("TMDB_PROXY"),
                    env_opt("TMDB_DAILY_MAX").and_then(|v| v.parse().ok()).unwrap_or(tmdb::DEFAULT_DAILY_MAX),
                )
            })
        })
        .flatten()
        .and_then(|built| built.map_err(|e| eprintln!("tmdb disabled (reqwest build failed: {e})")).ok())
        .map(Arc::new);
    let index = index.map(|queries| {
        Arc::new(
            queries
                .with_ratings(tmdb.as_ref().map(|t| t.ratings()))
                .with_characters(tmdb.as_ref().map(|t| t.characters())),
        )
    });

    // What /health says at boot, so the first change after it is logged against the real starting
    // state (a missing dataset is already reported above).
    let health = handler::health_state(dataset.is_some(), true, false, false, false, false, false, false)
        .map_or("ok", |(reason, _)| reason);
    let state = Arc::new(AppState {
        admission: admission::Admission::from_env(),
        dataset,
        catalog,
        default_country,
        embed,
        embed_memo: cache::EmbedMemo::new(cache::EMBED_MEMO_TTL, cache::EMBED_MEMO_ENTRIES),
        library_embed_memo: cache_dir.as_deref().map(std::path::Path::new).map_or_else(
            || cache::EmbedMemo::new(Duration::MAX, cache::LIBRARY_EMBED_MEMO_ENTRIES),
            cache::EmbedMemo::persisted_library,
        ),
        titles: title_search,
        index,
        maturity_providers: Arc::clone(&maturity_providers),
        motn,
        tvmaze: Arc::new(if std::env::var("TVMAZE").is_ok_and(|v| v == "0") {
            tvmaze::Tvmaze::off()
        } else {
            tvmaze::Tvmaze::new(cache_dir.as_deref().map(std::path::Path::new))
        }),
        metrics_token: std::env::var("METRICS_TOKEN").ok().filter(|t| !t.is_empty()),
        log_requests: std::env::var("LOG_REQUESTS").is_ok_and(|v| !v.is_empty() && v != "0"),
        log_identity: log_identity_on(std::env::var("LOG_IDENTITY").ok().as_deref()),
        playground: std::env::var("PLAYGROUND").is_ok_and(|v| !v.is_empty() && v != "0"),
        health: std::sync::Mutex::new(health),
    });
    #[cfg(feature = "embedded-model")]
    if state.embed.as_ref().is_some_and(|embed| {
        matches!(&embed.transport, EmbedTransport::Local(_))
            && matches!(*crate::util::lock(&embed.readiness), EmbedReadiness::Checking)
    }) {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let embed = state.embed.as_ref().expect("checked above").clone();
            let readiness = match embed.canary().await {
                Ok(()) => EmbedReadiness::Ready,
                Err(error) => {
                    eprintln!("embedded model readiness failed ({error})");
                    EmbedReadiness::Failed
                }
            };
            *crate::util::lock(&embed.readiness) = readiness;
            handler::note_embed_health(&state);
        });
    }
    if let Some(search) = &state.titles {
        tokio::spawn(titles::refresh_forever(Arc::clone(search)));
    }
    if let Some(index) = &state.index {
        tokio::spawn(queries::load_at_start(Arc::clone(index)));
    }
    if maturity_providers.enabled() {
        tokio::spawn(maturity_providers::Providers::refresh_forever(Arc::clone(&maturity_providers)));
    }
    if let Some(tmdb) = &tmdb {
        // The vote counts are awaited, not spawned: a local read. The store carries no vote count of its own, so
        // until this lands every browse row would answer in tmdb-id order — and `atlas-dataset-sync` restarts
        // this process on every publish, so that window is not rare. The index load spawned above waits for
        // them (`Ratings::settled`): it copies them into the search and facet indexes it keeps.
        eprintln!("{}", tmdb.load_votes().await);
        // The character links are most of the boot's TMDB work and nothing but the index routes reads them, so
        // they are built while atlas already answers everything else. The first index load waits for them
        // (`Characters::settled`).
        tokio::spawn({
            let tmdb = Arc::clone(tmdb);
            async move { eprintln!("tmdb: {}", tmdb.load_characters().await) }
        });
        tokio::spawn(tmdb::Tmdb::refresh_forever(Arc::clone(tmdb)));
    }
    if state.motn.enabled() {
        tokio::spawn(motn::Motn::refresh_forever(Arc::clone(&state.motn)));
    }
    if state.tvmaze.enabled() {
        tokio::spawn(tvmaze::Tvmaze::refresh_forever(Arc::clone(&state.tvmaze)));
    }
    // JustWatch rows anyone asked for in the last day are refreshed before they expire, so no request waits on
    // JustWatch for one: rows and billboards read what is kept.
    tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            loop {
                state.catalog.refresh_due().await;
                handler::note_catalog_health(&state);
                tokio::time::sleep(catalog::REFRESH_EVERY).await;
            }
        }
    });

    let app = axum::Router::new().fallback(handler::handle).with_state(Arc::clone(&state));

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap_or_else(|e| {
        eprintln!("bind :{port} failed: {e}");
        std::process::exit(1);
    });
    // Before the readiness line below, so nothing can be told we are up while a stop signal would
    // still be a hard kill.
    let shutdown = shutdown_signal();
    // The port it actually BOUND, not the one it was asked for: with PORT=0 the kernel picks one,
    // and reporting the request rather than the result made the line useless in exactly that case.
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    // The version and the effective configuration, so a log says what is running without the unit
    // file beside it. Secret-free by construction: the token and the internal embed address appear
    // only as on/off.
    let on = |b: bool| if b { "on" } else { "off" };
    let providers: Vec<&str> = catalog::selected_providers().iter().map(|p| p.code).collect();
    let dataset = match &state.dataset {
        Some(ds) => {
            format!("titles={} index={}/{}", ds.store_rows, ds.meta.embedding_model, ds.meta.taxonomy_version)
        }
        None => "dataset=unavailable".to_owned(),
    };
    eprintln!(
        "den-atlas {} listening on :{port} — metrics={} log_requests={} log_identity={} {dataset} country={} \
         providers={} catalog_ttl={}s catalog_cache={} embed={} title_search={} index_queries={} tmdb={} \
         motn={} tvmaze={} maturity_providers={} playground={} admission={}",
        release_version(),
        on(state.metrics_token.is_some()),
        on(state.log_requests),
        on(state.log_identity),
        state.default_country,
        providers.join(","),
        ttl.as_secs(),
        cache_dir.as_deref().unwrap_or("memory"),
        on(state.embed.as_ref().is_some_and(EmbedProxy::available)),
        on(state.titles.is_some()),
        on(state.index.is_some()),
        match &tmdb {
            Some(t) if t.asks() => "proxy",
            Some(_) => "kept-only",
            None => "off",
        },
        on(state.motn.enabled()),
        on(state.tvmaze.enabled()),
        on(state.maturity_providers.enabled()),
        on(state.playground && state.index.is_some()),
        state.admission.description(),
    );
    let outcome = serve_until(listener, app, shutdown, DRAIN_GRACE).await;
    eprintln!("{}", outcome.describe());
    let code = outcome.exit_code();
    if code != 0 {
        std::process::exit(code);
    }
}

/// `den-atlas replay <fixture>` — a request kept under `RECOMMEND_FIXTURES`, ranked again against the dataset in
/// `DATA_DIR` with the scoring this binary carries, and every slide printed on its own line. The exit code.
async fn replay(dir: &str, path: &str) -> i32 {
    let fail = |e: String| {
        eprintln!("replay: {e}");
        1
    };
    let dataset = match dataset::Dataset::load(std::path::Path::new(dir)) {
        Ok(dataset) => dataset,
        Err(e) => return fail(format!("dataset: {e}")),
    };
    let fixture = std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| e.to_string()));
    let (request, lists, now) = match fixture.and_then(|f| recommend::replayed(&f)) {
        Ok(replayed) => replayed,
        Err(e) => return fail(format!("{path}: {e}")),
    };
    let indexes = match queries::IndexQueries::new(&dataset).get(|| ()).await {
        Ok((indexes, _)) => indexes,
        Err(e) => return fail(format!("indexes: {e}")),
    };
    // Without TMDB's export or its kept vote counts, which serving reads: `billboard-check` reads both.
    let answer = recommend::answer(&indexes, None, &request, &lists, now, None);
    // An operator ran this by hand to inspect one fixture — not the standing server log `LOG_IDENTITY`
    // gates — so it always names the titles.
    println!("{}", recommend::summary(&indexes, &request, &answer, true));
    for (at, slide) in answer["slides"].as_array().map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        println!("{:>3}. {}", at + 1, recommend::describe(&indexes, slide, true));
    }
    0
}

/// An env var's value, with unset and empty both meaning "not configured" — the rule every den addon
/// uses. `EMBED_URL=` read as set produced a proxy to "".
fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[cfg(feature = "embedded-model")]
fn local_identity_matches(meta: &dataset::Meta, identity: &den_embed::EngineIdentity) -> bool {
    meta.embedding_model == identity.model
        && meta.dims as usize == identity.dims
        && meta.embedder_vector_epoch == Some(identity.vector_epoch)
        && meta.embedder_max_tokens == u32::try_from(identity.max_tokens).ok()
}

fn embed_proxy(_dataset: Option<&dataset::Dataset>) -> Option<EmbedProxy> {
    let inflight = || std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_EMBED_INFLIGHT));
    if let Some(base) = env_opt("EMBED_URL") {
        return match reqwest::Client::builder().timeout(EMBED_CALL_TIMEOUT).build() {
            Ok(client) => Some(EmbedProxy {
                transport: EmbedTransport::Remote { client, base: base.trim_end_matches('/').to_owned() },
                inflight: inflight(),
                readiness: Arc::new(std::sync::Mutex::new(EmbedReadiness::Ready)),
            }),
            Err(e) => {
                eprintln!("embed proxy disabled (reqwest build failed: {e})");
                None
            }
        };
    }

    let local = std::env::var("EMBED_LOCAL").is_ok_and(|value| !matches!(value.as_str(), "" | "0" | "false"));
    if !local {
        return None;
    }
    #[cfg(feature = "embedded-model")]
    {
        match den_embed::EmbeddedService::from_env_prefix("EMBED_") {
            Ok(service) => {
                let identity = service.identity();
                let compatible =
                    _dataset.is_some_and(|dataset| local_identity_matches(&dataset.meta, &identity));
                if !compatible {
                    eprintln!(
                        "embedded model unavailable (engine identity does not exactly match dataset model/dims/vector_epoch/max_tokens)"
                    );
                }
                Some(EmbedProxy {
                    transport: EmbedTransport::Local(service),
                    inflight: inflight(),
                    readiness: Arc::new(std::sync::Mutex::new(if compatible {
                        EmbedReadiness::Checking
                    } else {
                        EmbedReadiness::Failed
                    })),
                })
            }
            Err(error) => {
                eprintln!("embedded model disabled ({error:#})");
                None
            }
        }
    }
    #[cfg(not(feature = "embedded-model"))]
    {
        eprintln!("embedded model disabled (this den-atlas build lacks the embedded-model feature)");
        None
    }
}

/// `LOG_IDENTITY`: on by default — unset, empty, or anything but one of the usual off-tokens. The
/// opposite default from `LOG_REQUESTS`/`PLAYGROUND`, because this gates fields WITHIN a line that
/// is written either way, not the line itself: an operator who sets nothing keeps the titles a
/// decision line names, and has to opt out rather than in.
pub(crate) fn log_identity_on(v: Option<&str>) -> bool {
    !matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("0" | "false" | "off" | "no"))
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
            eprintln!("second signal — exiting without finishing the drain");
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
            eprintln!("{name} — draining in-flight requests");
        }
        Err(e) => {
            eprintln!("{name} handler unavailable ({e}); it will be a hard kill");
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

    /// A staged dataset directory: the manifest, and whatever `extra` it adds.
    ///
    /// The directory name carries the process id. A fixed name under the shared temp dir, cleared with
    /// `remove_dir_all` on entry, means two concurrent runs of this binary — two git worktrees, which is
    /// how this project is usually laid out — delete each other's staging mid-test.
    fn staged(name: &str, extra: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("atlas-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            r#"{{"datasetVersion":"fixture","taxonomyVersion":"t02","embeddingModel":"bge-m3",
                 "dims":1024,"quantization":"int8-symmetric-x127"{extra}}}"#
        );
        std::fs::write(dir.join("dataset.meta.json"), meta).unwrap();
        dir
    }

    /// The gate that decides whether a generation replaces the running one. A manifest with no
    /// `storeFile` names no artifact at all now that nothing else is read, so it is BROKEN — where it
    /// used to be DEGRADED (2), on the reasoning that it still served and merely ranked More Like This
    /// with the pre-pooled scorer. There is nothing left behind such a manifest to serve.
    #[test]
    fn check_calls_a_dataset_with_no_store_broken() {
        let dir = staged("no-store", "");
        assert_eq!(check_dataset(&dir), 1, "a manifest naming no store has no artifact to serve");
    }

    /// The distinction the exit codes exist for. A store that will not read is BROKEN and must never
    /// replace a working generation, however badly the box needs a dataset.
    #[test]
    fn check_calls_an_unreadable_store_broken() {
        let dir = staged("bad-store", r#","storeFile":"den.store""#);
        std::fs::write(dir.join("den.store"), vec![0u8; 256]).unwrap();
        assert_eq!(check_dataset(&dir), 1, "a file that is not a store is BROKEN, not degraded");
    }

    /// And accepts one that declares a store it can read, whose `datasetVersion` matches the manifest.
    #[test]
    fn check_accepts_a_dataset_whose_store_reads() {
        let Some(fixture) = store::spec_fixture() else { return };
        let dir = staged("with-store", r#","storeFile":"den.store""#);
        std::fs::copy(&fixture, dir.join("den.store")).unwrap();
        assert_eq!(check_dataset(&dir), 0, "a readable store at the manifest's version is fit to swap in");
    }

    /// A store from a different generation than the manifest naming it: the mixed-generation case a
    /// moving release makes easy, and the one thing a present-and-readable store can still be wrong about.
    #[test]
    fn check_refuses_a_store_from_another_generation() {
        let Some(fixture) = store::spec_fixture() else { return };
        let dir = staged("mixed", r#","storeFile":"den.store""#);
        std::fs::copy(&fixture, dir.join("den.store")).unwrap();
        let meta = std::fs::read_to_string(dir.join("dataset.meta.json")).unwrap();
        std::fs::write(dir.join("dataset.meta.json"), meta.replace(r#""fixture""#, r#""v9""#)).unwrap();
        assert_eq!(check_dataset(&dir), 1, "the store says fixture and the manifest says v9");
    }

    /// On by default — the opposite default from `LOG_REQUESTS`/`PLAYGROUND` — and off only for one
    /// of the usual false-y tokens, case-insensitively.
    #[test]
    fn log_identity_is_on_by_default_and_off_only_for_a_falsey_token() {
        assert!(log_identity_on(None));
        assert!(log_identity_on(Some("")));
        assert!(log_identity_on(Some("1")));
        assert!(log_identity_on(Some("anything")));
        for v in ["0", "false", "off", "no", "FALSE", " Off "] {
            assert!(!log_identity_on(Some(v)), "{v:?} should turn identity logging off");
        }
    }

    #[cfg(feature = "embedded-model")]
    #[tokio::test]
    async fn local_transport_drives_the_real_embedded_router() {
        let prefix = format!("ATLAS_LOCAL_TRANSPORT_{}_,", std::process::id()).replace(',', "");
        // Lazy load lets the service be constructed without shipping a 555 MB model in the unit-test fixture. A
        // blank input is the engine's real model-free zero vector, so this still proves the feature-gated Local
        // transport traverses den-embed's router and decodes its response rather than testing a mock.
        unsafe {
            std::env::set_var(format!("{prefix}IDLE_UNLOAD_SECS"), "60");
        }
        let service = den_embed::EmbeddedService::from_env_prefix(&prefix).unwrap();
        let proxy = EmbedProxy {
            transport: EmbedTransport::Local(service),
            inflight: Arc::new(tokio::sync::Semaphore::new(1)),
            readiness: Arc::new(std::sync::Mutex::new(EmbedReadiness::Ready)),
        };
        let response =
            proxy.call("/embed", bytes::Bytes::from_static(br#"{"text":""}"#), None).await.unwrap();
        assert_eq!(response.status, axum::http::StatusCode::OK);
        let body = serde_json::from_slice::<serde_json::Value>(&response.body).unwrap();
        assert_eq!(body["model"], "bge-m3");
        assert_eq!(body["vector"].as_array().unwrap().len(), 1024);
        unsafe {
            std::env::remove_var(format!("{prefix}IDLE_UNLOAD_SECS"));
        }
    }

    #[cfg(feature = "embedded-model")]
    #[tokio::test]
    async fn atlas_health_and_readiness_surface_a_failed_local_canary() {
        use tower::ServiceExt;

        let prefix = format!("ATLAS_LOCAL_HEALTH_{}_,", std::process::id()).replace(',', "");
        unsafe { std::env::set_var(format!("{prefix}IDLE_UNLOAD_SECS"), "60") };
        let service = den_embed::EmbeddedService::from_env_prefix(&prefix).unwrap();
        let dir = staged(
            "embed-health",
            r#", "storeFile":"den.store", "embedderVectorEpoch":1, "embedderMaxTokens":1024"#,
        );
        let title = store::fixture::Title {
            media: 0,
            tmdb_id: 1,
            plot: vec![0; 1024],
            premise: vec![0; 1024],
            ..store::fixture::Title::default()
        };
        store::fixture::write(&dir.join("den.store"), "fixture", 1024, &[title], &[]);
        let dataset = dataset::Dataset::load(&dir).unwrap();
        let mut state = AppState::for_test(Some(dataset));
        state.embed = Some(EmbedProxy {
            transport: EmbedTransport::Local(service),
            inflight: Arc::new(tokio::sync::Semaphore::new(1)),
            readiness: Arc::new(std::sync::Mutex::new(EmbedReadiness::Failed)),
        });
        let app = axum::Router::new().fallback(handler::handle).with_state(Arc::new(state));
        for (path, status) in
            [("/health", axum::http::StatusCode::OK), ("/ready", axum::http::StatusCode::SERVICE_UNAVAILABLE)]
        {
            let response = app
                .clone()
                .oneshot(axum::http::Request::get(path).body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["reason"],
                "embed_unavailable"
            );
        }
        unsafe { std::env::remove_var(format!("{prefix}IDLE_UNLOAD_SECS")) };
    }

    #[cfg(feature = "embedded-model")]
    #[test]
    fn local_engine_identity_matches_all_four_corpus_fields_fail_closed() {
        let identity =
            den_embed::EngineIdentity { model: "bge-m3", dims: 1024, vector_epoch: 1, max_tokens: 1024 };
        let good = serde_json::from_str::<dataset::Meta>(
            r#"{"datasetVersion":"v","taxonomyVersion":"t","embeddingModel":"bge-m3","dims":1024,
                "quantization":"int8","storeFile":"s","embedderVectorEpoch":1,"embedderMaxTokens":1024}"#,
        )
        .unwrap();
        assert!(local_identity_matches(&good, &identity));
        let mut mismatches = Vec::new();
        let mut bad = good.clone();
        bad.embedding_model = "other".to_owned();
        mismatches.push(bad);
        let mut bad = good.clone();
        bad.dims = 384;
        mismatches.push(bad);
        let mut bad = good.clone();
        bad.embedder_vector_epoch = Some(2);
        mismatches.push(bad);
        let mut bad = good.clone();
        bad.embedder_max_tokens = Some(512);
        mismatches.push(bad);
        for bad in mismatches {
            assert!(!local_identity_matches(&bad, &identity), "accepted identity mismatch");
        }
        let missing: dataset::Meta = serde_json::from_str(
            r#"{"datasetVersion":"v","taxonomyVersion":"t","embeddingModel":"bge-m3","dims":1024,
                "quantization":"int8","storeFile":"s"}"#,
        )
        .unwrap();
        assert!(!local_identity_matches(&missing, &identity), "missing epoch/cap must fail closed");
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
