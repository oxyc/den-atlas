//! Catalog resource (docs/CATALOG-justwatch.md): per-provider "Popular on <service>" rows plus the
//! aggregated "Trending Everywhere" row. Public and tokenless — additive to the dataset addon, and
//! isolated: a JustWatch failure degrades to empty rows and never touches the dataset paths.

use crate::cache::{Lookup, TtlCache};
use crate::justwatch::{ObjectType, TrendingItem, TrendingSource};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Debug)]
pub struct Provider {
    pub code: &'static str, // JustWatch package short-code
    pub id: &'static str,   // Stremio catalog id
    pub name: &'static str, // row title the client renders
    /// Every JustWatch package id this service is known by, most common first. Identical to the TMDB
    /// watch-provider ids (verified against both lists). Per-country: Prime is 119 in UY/FI, 9 in the US.
    pub package_ids: &'static [i64],
}

// Short codes can drift per country; if a row comes back empty, verify against JustWatch's provider
// list for that country. Adding a service = one row here.
// Providers are declared by their **stable JustWatch package ids**, not by short code — because BOTH the code
// and the id are per-country. Amazon Prime Video is `prv`/119 in UY and FI but `amp`/9 in the US (verified
// 2026-07-31), which is how a hardcoded code shipped an empty "Popular on Prime Video" row. The local code is
// resolved per request from `packages(country)`; a service the country doesn't carry resolves to nothing and
// its row is simply absent.
//
// `package_ids` lists every id a service is known by, most common first. The first is the primary — it's what
// `denProviderId` publishes for back-compat — and all of them are published as `denProviderIds` so a client
// keying on its own region's id (Den keys on TMDB's, which is the same number) matches whichever applies.
const PROVIDERS: &[Provider] = &[
    Provider { code: "nfx", id: "jw-nfx", name: "Popular on Netflix", package_ids: &[8] },
    Provider { code: "mxx", id: "jw-mxx", name: "Popular on HBO Max", package_ids: &[1899] },
    Provider { code: "prv", id: "jw-prv", name: "Popular on Prime Video", package_ids: &[119, 9] },
    Provider { code: "dnp", id: "jw-dnp", name: "Popular on Disney+", package_ids: &[337] },
    // 531 is Paramount+ in LatAm/EU and is also TMDB's id everywhere; the US splits it into tiers with their
    // own ids and no plain 531, so a client keying on TMDB's 531 still matches while we resolve the local tier.
    Provider { code: "pmp", id: "jw-pmp", name: "Popular on Paramount+", package_ids: &[531, 2303, 2616] },
    Provider { code: "atp", id: "jw-atp", name: "Popular on Apple TV+", package_ids: &[350, 2552] },
    Provider { code: "sst", id: "jw-sst", name: "Popular on SkyShowtime", package_ids: &[1773] },
];

pub const NEW_SUFFIX: &str = "-new";

/// The arrivals catalog id for a provider, e.g. "jw-nfx-new".
pub fn new_catalog_id(provider: &Provider) -> String {
    format!("{}{}", provider.id, NEW_SUFFIX)
}

/// Resolve a catalog id to a provider + whether it's the arrivals variant.
pub fn resolve_catalog<'a>(catalog_id: &str, providers: &[&'a Provider]) -> Option<(&'a Provider, bool)> {
    if let Some(base) = catalog_id.strip_suffix(NEW_SUFFIX) {
        return providers.iter().find(|p| p.id == base).map(|p| (*p, true));
    }
    providers.iter().find(|p| p.id == catalog_id).map(|p| (*p, false))
}

pub const TRENDING_ID: &str = "jw-trending";
pub const TRENDING_NAME: &str = "Trending Everywhere";
const STREMIO_TYPES: [&str; 2] = ["movie", "series"];

/// Look up a provider by its JustWatch short-code (for parsing a per-install config). `None` for an
/// unknown code, so a garbled config segment simply drops that provider.
/// How many services the table carries — the ceiling on any install's selection.
#[cfg(test)]
pub fn provider_count() -> usize {
    PROVIDERS.len()
}

pub fn provider_by_code(code: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.code == code)
}

/// The operator-default provider set — the full table, or a subset selected by `JW_PROVIDERS` (codes).
/// Used when no per-install config is supplied. Resolved once (env is effectively static).
pub fn selected_providers() -> &'static [&'static Provider] {
    static SELECTED: OnceLock<Vec<&'static Provider>> = OnceLock::new();
    SELECTED.get_or_init(|| match std::env::var("JW_PROVIDERS") {
        Ok(v) if !v.trim().is_empty() => {
            let codes: Vec<String> = v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
            PROVIDERS.iter().filter(|p| codes.iter().any(|c| c == p.code)).collect()
        }
        _ => PROVIDERS.iter().collect(),
    })
}

pub struct CatalogEntry {
    pub type_: &'static str,
    pub id: String,
    pub name: String,
    /// Every JustWatch package id this row's service is known by — identical to the TMDB watch-provider ids.
    /// Published on the manifest so a client can associate the row with its own provider directory instead of
    /// parsing the display name. Empty for the aggregate "Trending Everywhere" row, which spans providers.
    pub package_ids: &'static [i64],
}

/// The manifest `catalogs[]` for a given provider set — one per provider × type, plus "Trending
/// Everywhere" × type. Empty when no providers are selected (the feature is off for that install).
pub fn catalog_entries(providers: &[&'static Provider]) -> Vec<CatalogEntry> {
    if providers.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity((providers.len() + 1) * STREMIO_TYPES.len());
    for t in STREMIO_TYPES {
        // "Trending Everywhere" (the aggregated cross-provider chart) leads, then the per-provider rows.
        out.push(CatalogEntry { type_: t, id: TRENDING_ID.to_owned(), name: TRENDING_NAME.to_owned(), package_ids: &[] });
        for p in providers {
            out.push(CatalogEntry { type_: t, id: p.id.to_owned(), name: p.name.to_owned(), package_ids: p.package_ids });
            // "New on <service>" sits next to "Popular on <service>". `name` is derived from the provider's
            // display name so a new service needs only its one PROVIDERS row.
            out.push(CatalogEntry {
                type_: t,
                id: new_catalog_id(p),
                name: format!("New on {}", p.name.trim_start_matches("Popular on ")),
                package_ids: p.package_ids,
            });
        }
    }
    out
}

/// The rendered catalog body plus whether it's freshly-sourced data. `fresh == false` marks a
/// stale-served or empty-degradation body so the handler can shorten its HTTP TTL (don't let a CDN
/// pin an outage-empty row for the full max-age).
pub struct CatalogResponse {
    pub body: String,
    pub fresh: bool,
}

pub struct CatalogState {
    source: Arc<dyn TrendingSource>,
    cache: TtlCache,
    // Per-key refresh gate (single-flight): coalesces concurrent misses so a cold-cache burst makes one
    // upstream fetch per key, not N. Keyspace is bounded (ids × types × the handful of live countries).
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// `country -> (packageId -> shortName)`. Both identifiers are per-country, so the local code for a
    /// provider has to be looked up rather than hardcoded. Cached because it changes on the order of months.
    packages: Mutex<HashMap<String, Arc<Vec<(i64, String)>>>>,
    // Whether the most recent upstream refresh succeeded. Starts optimistic (true); every actual fetch
    // attempt flips it (success ⇒ true, failure ⇒ false), while a plain cache hit (no refresh) leaves it
    // as-is. Read by `/health` to report `stale_catalog` (ADDON-02) — distinct from the per-response
    // `fresh` flag, which shortens one row's HTTP TTL.
    last_refresh_ok: AtomicBool,
    /// A PROCESS-WIDE ceiling on simultaneous upstream calls.
    ///
    /// Bounding the fan-out per request is not enough: the caller picks the cache key (country ×
    /// provider subset ≈ 171k of them), so every request is a distinct miss with its own
    /// single-flight gate and nothing coalesces. ~1,000 concurrent GETs still reached ~4,000
    /// simultaneous calls and earned this host a 403 from JustWatch. What upstream cares about is
    /// the total, so that is what has to be capped — the queue is bounded by the request timeout.
    upstream: Arc<tokio::sync::Semaphore>,
}

/// Enough to keep a cold cache filling briskly (the default selection is 7 providers, so one
/// aggregate refresh fits inside this), far below anything JustWatch would read as abuse.
const MAX_UPSTREAM_INFLIGHT: usize = 8;

impl CatalogState {
    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn new(source: Arc<dyn TrendingSource>, ttl: Duration) -> Self {
        Self {
            source,
            cache: TtlCache::new(ttl),
            inflight: Mutex::new(HashMap::new()),
            packages: Mutex::new(HashMap::new()),
            last_refresh_ok: AtomicBool::new(true),
            upstream: Arc::new(tokio::sync::Semaphore::new(MAX_UPSTREAM_INFLIGHT)),
        }
    }

    /// Whether the last JustWatch refresh succeeded — the `/health` freshness signal (ADDON-02).
    pub fn fresh(&self) -> bool {
        self.last_refresh_ok.load(Ordering::Relaxed)
    }

    /// The `{ "metas": [...] }` body for a catalog id + Stremio type in a given `country`, restricted to
    /// this install's `providers`. `None` (→ 404) for an unknown/unselected id or unknown type.
    /// Otherwise always valid JSON; on a source failure it serves last-good rows, else empty — both
    /// marked `fresh: false` so the handler can cache them briefly.
    pub async fn metas_json(
        &self,
        catalog_id: &str,
        stremio_type: &str,
        country: &str,
        providers: &[&'static Provider],
    ) -> Option<CatalogResponse> {
        let obj = ObjectType::from_stremio(stremio_type)?;
        let is_trending = catalog_id == TRENDING_ID && !providers.is_empty();
        let resolved = resolve_catalog(catalog_id, providers);
        if !is_trending && resolved.is_none() {
            return None; // unknown id, or not one of this install's selected providers
        }

        // The aggregate row is a union over THIS install's selected providers, so the selection is
        // part of what the value is. Without it in the key, whichever install warmed the entry won
        // for the whole TTL: a Netflix-only install's chart was served verbatim to someone who had
        // picked four services, and vice versa — titles they cannot stream. Non-aggregate rows are
        // already one provider, named by catalog_id, so they need nothing extra.
        let key = if is_trending {
            let mut codes: Vec<&str> = providers.iter().map(|p| p.code).collect();
            codes.sort_unstable(); // selection is a set; order must not split the cache
            format!("jw:{}:{}:{}:{}", country, catalog_id, stremio_type, codes.join(","))
        } else {
            format!("jw:{}:{}:{}", country, catalog_id, stremio_type)
        };
        if let Lookup::Fresh(v) = self.cache.get(&key) {
            return Some(CatalogResponse { body: v, fresh: true });
        }

        // Single-flight: one refresh per key runs; the rest wait and then hit the warm cache. The std
        // lock is only held to fetch the per-key gate (never across the await).
        let gate = {
            let mut map = self.inflight.lock().unwrap();
            Arc::clone(map.entry(key.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
        };
        let _hold = gate.lock().await;
        if let Lookup::Fresh(v) = self.cache.get(&key) {
            return Some(CatalogResponse { body: v, fresh: true }); // filled while we waited
        }

        let fetched = if is_trending {
            self.aggregate(obj, country, providers).await
        } else {
            let (provider, is_new) = resolved.unwrap();
            // A code we cannot resolve is a REFRESH failure, not an unknown route. Propagating None
            // with `?` returned 404 from a catalog the manifest advertises, and did it BEFORE the
            // arm below that clears last_refresh_ok — so the row a user asked for failed upstream
            // while /health still said ok. It falls through to the same serve-stale/empty
            // degradation as any other failed fetch now, which is also what the aggregate row does.
            // Err = could not ask (a refresh failure → degrade below); Ok = an answer we can act on.
            match self.packages_for(country).await {
                Err(()) => None,
                Ok(map) => match self.code_in(provider, &map) {
                    Some(code) if is_new => {
                        let _permit = self.upstream.acquire().await;
                        // Arrivals come back most-recently-added first; that order IS the row.
                        self.source.new_titles(&code, obj, country).await.ok()
                    }
                    Some(code) => {
                        let _permit = self.upstream.acquire().await;
                        // "Popular on <service>" = JustWatch's POPULAR sort, not TRENDING.
                        self.source.popular(&code, obj, country, "POPULAR").await.ok()
                    }
                // A country that genuinely doesn't carry the service has no row — absent, not
                // empty-but-present. But an EMPTY map means we could not ask, which is a refresh
                // failure: returning None there 404'd a catalog the manifest advertises, and did it
                // before the arm that clears last_refresh_ok, so /health still said ok while the row
                // the user asked for was failing upstream.
                    // We asked and got an answer: this country simply doesn't carry the service,
                    // so the row is absent rather than empty-but-present.
                    None => return None,
                },
            }
        };

        match fetched {
            Some(items) => {
                let body = render_metas(&items, stremio_type);
                self.cache.put(&key, body.clone());
                self.last_refresh_ok.store(true, Ordering::Relaxed);
                Some(CatalogResponse { body, fresh: true })
            }
            // Refresh failed → serve stale if we have it, else an empty list (graceful degradation).
            // Not fresh → the handler uses a short TTL so recovery isn't masked at the CDN, and `/health`
            // reports `stale_catalog`.
            None => {
                self.last_refresh_ok.store(false, Ordering::Relaxed);
                Some(CatalogResponse {
                    body: match self.cache.get(&key) {
                        Lookup::Fresh(v) | Lookup::Stale(v) => v,
                        Lookup::Miss => render_metas(&[], stremio_type),
                    },
                    fresh: false,
                })
            }
        }
    }

    /// The country's package list, fetched at most once per call. An empty result (JustWatch down)
    /// is NOT cached — otherwise one outage blanks every row until restart — so callers must resolve
    /// it once and reuse it rather than per provider.
    /// `Err(())` = we could not ask. `Ok(list)` = JustWatch answered, and the list may legitimately
    /// be empty for a small market once the monetization filter runs. `unwrap_or_default()` flattened
    /// those together, so a country JustWatch genuinely serves nothing for was treated as an outage:
    /// permanently `degraded: stale_catalog`, and re-fetched on every single request forever.
    async fn packages_for(&self, country: &str) -> Result<Arc<Vec<(i64, String)>>, ()> {
        if let Some(m) = self.packages.lock().unwrap().get(country).cloned() {
            return Ok(m);
        }
        let fetched = {
            let _permit = self.upstream.acquire().await;
            Arc::new(self.source.packages(country).await?)
        };
        // Cache the answer either way, including a legitimately empty one — otherwise every request
        // for such a country re-asks, which is the loop that pushes hardest exactly when the upstream
        // is already refusing us.
        self.packages.lock().unwrap().insert(country.to_owned(), Arc::clone(&fetched));
        Ok(fetched)
    }

    /// The country-local short code for a provider, or `None` when that country doesn't carry the
    /// service, or when we could not resolve the list — in which case its row is simply absent
    /// rather than wrong.
    fn code_in(&self, provider: &Provider, map: &[(i64, String)]) -> Option<String> {
        if map.is_empty() {
            // No row rather than a wrong one. The fallback used to send `provider.code` upstream
            // unvalidated, and JustWatch does NOT reject a code it doesn't recognise — verified
            // live: `packages:["bogus_xyz"]` returns the country's overall chart, byte-identical to
            // sending no filter. So a stale or mistyped code published the whole country's chart
            // under a service's name, and because that response is non-empty the "0 usable items"
            // schema-change guard never fired. (prv/119 is already not the US Prime code.)
            // No per-provider line here: this runs inside the aggregate loop, so one request with a
            // country JustWatch cannot serve emitted a line per provider — measured at ~260 KB of
            // stderr for a single crafted request, which buried the 429/403 lines that mattered.
            // packages_for logs the lookup failure once.
            return None;
        }
        provider
            .package_ids
            .iter()
            .find_map(|id| map.iter().find(|(pid, _)| pid == id).map(|(_, code)| code.clone()))
    }


    /// "Trending Everywhere": union across providers, re-ranked by inverse-rank-sum. Providers are
    /// fetched concurrently so a cold miss costs ~one provider's latency, not the sum. `None` when no
    /// provider produced a list, so the caller can serve stale rather than publish an empty row.
    /// Results are placed by provider index to keep the dedupe representative deterministic
    /// regardless of completion order.
    async fn aggregate(
        &self,
        obj: ObjectType,
        country: &str,
        providers: &[&'static Provider],
    ) -> Option<Vec<TrendingItem>> {
        let mut slots: Vec<Option<Vec<TrendingItem>>> = vec![None; providers.len()];
        let mut set: JoinSet<(usize, Result<Vec<TrendingItem>, ()>)> = JoinSet::new();
        // ONE package fetch for the whole aggregate. This was awaited per provider, and a failing
        // lookup is deliberately not cached, so a JustWatch blip cost a full 8s timeout per provider
        // serially — ~64s for the default seven, inside a single request with no server-side
        // deadline, while every concurrent request for the key queued behind the single-flight gate
        // and the client's retries each started another seven.
        let Ok(map) = self.packages_for(country).await else {
            return None; // could not ask → serve stale rather than publish a union of nothing
        };
        for (i, p) in providers.iter().enumerate() {
            let src = Arc::clone(&self.source);
            let Some(code) = self.code_in(p, &map) else { continue };
            let country = country.to_owned();
            // The aggregate IS "Trending Everywhere" → TRENDING, on purpose (distinct from the Popular rows).
            let permits = Arc::clone(&self.upstream);
            set.spawn(async move {
                let _permit = permits.acquire().await;
                (i, src.popular(&code, obj, &country, "TRENDING").await)
            });
        }
        let mut asked = 0usize;
        let mut answered = 0usize;
        while let Some(joined) = set.join_next().await {
            asked += 1;
            if let Ok((i, Ok(items))) = joined {
                answered += 1;
                slots[i] = Some(items);
            }
        }
        // A PARTIAL union is not the union. Dropping the failures and rendering the survivors turned
        // "Trending Everywhere" into one service's chart — cached fresh for 6h plus an hour of CDN,
        // with /health green — and the 7 requests fire at one host on one 8s timeout, so a single
        // slow provider is enough. It is the same principle graphql_error is built on: a partial
        // chart is not a chart. Serve the last good union instead.
        if answered < asked {
            eprintln!(
                "den-atlas: aggregate for {country} had {answered}/{asked} providers answer — \
                 not publishing a partial union"
            );
            return None;
        }
        let lists: Vec<Vec<TrendingItem>> = slots.into_iter().flatten().collect();
        if lists.is_empty() {
            return None;
        }
        Some(aggregate_inverse_rank(&lists))
    }
}

/// Dedupe by IMDb id and score by Σ 1/(rank+1) across lists, so a title trending on several services
/// outranks one that's #1 on a single service. Deterministic (imdb tiebreak). Top 100.
pub fn aggregate_inverse_rank(lists: &[Vec<TrendingItem>]) -> Vec<TrendingItem> {
    use std::collections::HashMap;
    let mut score: HashMap<&str, f64> = HashMap::new();
    let mut repr: HashMap<&str, &TrendingItem> = HashMap::new();
    for list in lists {
        for it in list {
            *score.entry(it.imdb.as_str()).or_insert(0.0) += 1.0 / (it.rank as f64 + 1.0);
            repr.entry(it.imdb.as_str()).or_insert(it);
        }
    }
    let mut ranked: Vec<(&str, f64)> = score.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(b.0))
    });
    ranked
        .into_iter()
        .take(100)
        .enumerate()
        .map(|(i, (imdb, _))| {
            let r = repr[imdb];
            TrendingItem { imdb: r.imdb.clone(), moviedb: r.moviedb, title: r.title.clone(), rank: i, rating: r.rating, year: r.year }
        })
        .collect()
}

/// Render items as Stremio catalog metas. `id` is the IMDb id (a plain Stremio client + Cinemeta resolve
/// the detail page from it); `imdb_id` + `moviedb_id` are the extra keys the Den app maps rows through
/// (it bridges everything via TMDB — an item without `moviedb_id` won't render there). Poster is
/// metahub-by-IMDb (Cinemeta's own source), so no TMDB fetch is needed to draw the grid.
pub fn render_metas(items: &[TrendingItem], stremio_type: &str) -> String {
    let metas: Vec<serde_json::Value> = items
        .iter()
        .map(|it| {
            let mut m = serde_json::json!({
                "id": it.imdb,
                "imdb_id": it.imdb,
                "type": stremio_type,
                "name": it.title,
                "poster": format!("https://images.metahub.space/poster/medium/{}/img", it.imdb),
            });
            if let Some(tmdb) = it.moviedb {
                m["moviedb_id"] = serde_json::json!(tmdb);
            }
            // JustWatch's IMDb score → the Den card's star (the app maps `imdbRating` → voteAverage; a
            // detail visit later upgrades it to the OMDb/IMDb value). Emitted as a string, Stremio-style.
            if let Some(rating) = it.rating {
                m["imdbRating"] = serde_json::json!(format!("{rating:.1}"));
            }
            // Original release year → the card year (Stremio `releaseInfo`; the app reads its first 4 digits).
            if let Some(year) = it.year {
                m["releaseInfo"] = serde_json::json!(year.to_string());
            }
            m
        })
        .collect();
    serde_json::json!({ "metas": metas }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn item(imdb: &str, title: &str, rank: usize) -> TrendingItem {
        TrendingItem { imdb: imdb.into(), moviedb: Some(42), title: title.into(), rank, rating: None, year: None }
    }

    struct Fake {
        data: Vec<TrendingItem>,
        calls: AtomicUsize,
        fail_after: usize, // Ok for the first `fail_after` calls, then Err
        /// Records which arm the router picked, so a test can prove "-new" reaches `new_titles`.
        new_calls: AtomicUsize,
        /// Fail the package lookup, and count how often it is asked.
        no_packages: bool,
        package_calls: AtomicUsize,
        /// Answer the package lookup successfully with an EMPTY list (a small market).
        empty_packages: bool,
        /// Fail `popular` for every provider after the first — a partial aggregate.
        popular_ok_first_only: bool,
        /// Observe how many upstream calls are in flight at once, and the high-water mark.
        live: AtomicUsize,
        peak: AtomicUsize,
        /// Hold each call briefly so overlap is observable.
        dwell: bool,
    }
    impl Fake {
        /// Bracket a call so `peak` records the high-water mark of simultaneous upstream work.
        async fn tracked<T>(&self, f: impl std::future::Future<Output = T>) -> T {
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if self.dwell {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let out = f.await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            out
        }

        fn answer(&self) -> Result<Vec<TrendingItem>, ()> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n >= self.fail_after {
                Err(())
            } else {
                Ok(self.data.clone())
            }
        }
    }
    #[async_trait]
    impl TrendingSource for Fake {
        async fn popular(&self, _p: &str, _o: ObjectType, _country: &str, _sort: &str) -> Result<Vec<TrendingItem>, ()> {
            if self.popular_ok_first_only && self.calls.load(Ordering::SeqCst) > 0 {
                self.calls.fetch_add(1, Ordering::SeqCst);
                return Err(());
            }
            self.tracked(async { self.answer() }).await
        }
        async fn new_titles(&self, _p: &str, _o: ObjectType, _country: &str) -> Result<Vec<TrendingItem>, ()> {
            self.new_calls.fetch_add(1, Ordering::SeqCst);
            self.answer()
        }
        /// Mirrors a real country list: Prime is 119 here (as in UY/FI), never 9.
        async fn packages(&self, _country: &str) -> Result<Vec<(i64, String)>, ()> {
            self.package_calls.fetch_add(1, Ordering::SeqCst);
            if self.no_packages {
                return Err(());
            }
            if self.empty_packages {
                return Ok(Vec::new()); // asked, answered, this market carries nothing
            }
            Ok(vec![
                (8, "nfx".into()),
                (119, "prv".into()),
                (1899, "mxx".into()),
                (531, "pmp".into()),
            ])
        }
    }

    fn state(data: Vec<TrendingItem>, ttl: Duration, fail_after: usize) -> CatalogState {
        CatalogState::new(Arc::new(fake(data, fail_after, false)), ttl)
    }

    fn fake(data: Vec<TrendingItem>, fail_after: usize, no_packages: bool) -> Fake {
        Fake {
            data,
            calls: AtomicUsize::new(0),
            fail_after,
            new_calls: AtomicUsize::new(0),
            no_packages,
            package_calls: AtomicUsize::new(0),
            empty_packages: false,
            popular_ok_first_only: false,
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            dwell: false,
        }
    }

    #[tokio::test]
    async fn renders_provider_row() {
        let s = state(vec![item("tt1", "A", 0)], Duration::from_secs(3600), usize::MAX);
        let r = s.metas_json("jw-nfx", "movie", "US", selected_providers()).await.unwrap();
        assert!(r.fresh);
        assert!(r.body.contains(r#""id":"tt1""#));
        assert!(r.body.contains(r#""imdb_id":"tt1""#));
        assert!(r.body.contains(r#""moviedb_id":42"#), "emits moviedb_id so the Den app can map the row");
        assert!(r.body.contains(r#""type":"movie""#));
        assert!(r.body.contains("images.metahub.space/poster/medium/tt1/img"));
    }

    #[tokio::test]
    async fn unknown_id_or_type_is_none() {
        let s = state(vec![], Duration::from_secs(3600), usize::MAX);
        assert!(s.metas_json("bogus", "movie", "US", selected_providers()).await.is_none());
        assert!(s.metas_json("jw-nfx", "audiobook", "US", selected_providers()).await.is_none());
    }

    #[tokio::test]
    async fn source_failure_degrades_to_empty_and_not_fresh() {
        let s = state(vec![item("tt1", "A", 0)], Duration::from_secs(3600), 0); // fail immediately
        let r = s.metas_json("jw-nfx", "movie", "US", selected_providers()).await.unwrap();
        assert_eq!(r.body, r#"{"metas":[]}"#);
        assert!(!r.fresh, "empty degradation must be marked not-fresh for a short HTTP TTL");
    }

    #[tokio::test]
    async fn serves_stale_on_refresh_error() {
        // ttl=0 → the first put is immediately stale, so the 2nd call refreshes and (source now failing)
        // must return the stale last-good rows rather than going empty.
        let s = state(vec![item("tt1", "A", 0)], Duration::ZERO, 1); // Ok once, then Err
        let first = s.metas_json("jw-nfx", "movie", "US", selected_providers()).await.unwrap();
        assert!(first.body.contains(r#""id":"tt1""#));
        let second = s.metas_json("jw-nfx", "movie", "US", selected_providers()).await.unwrap();
        assert_eq!(second.body, first.body, "stale value served when refresh fails");
        assert!(!second.fresh, "stale-served body must be marked not-fresh");
    }

    #[test]
    fn aggregation_floats_multi_service_titles() {
        // tt_multi is #2 on two services; tt_top is #1 on one. Sum of 1/(rank+1):
        // tt_multi = 1/2 + 1/2 = 1.0 ; tt_top = 1/1 = 1.0 → tie broken by id, but add a 3rd list to break.
        let l1 = vec![item("tt_top", "Top", 0), item("tt_multi", "Multi", 1)];
        let l2 = vec![item("tt_multi", "Multi", 1), item("tt_x", "X", 2)];
        let l3 = vec![item("tt_multi", "Multi", 3), item("tt_y", "Y", 0)];
        let agg = aggregate_inverse_rank(&[l1, l2, l3]);
        assert_eq!(agg[0].imdb, "tt_multi", "a title trending on 3 services floats to the top");
        // ranks are renumbered 0..n
        assert_eq!(agg[0].rank, 0);
    }

    #[test]
    fn catalog_entries_cover_providers_and_trending() {
        let entries = catalog_entries(selected_providers());
        assert!(entries.iter().any(|e| e.id == "jw-nfx" && e.type_ == "movie" && e.name == "Popular on Netflix"));
        assert!(entries.iter().any(|e| e.id == TRENDING_ID && e.type_ == "series"));
        // per type: N providers × (popular + new) + 1 trending
        let per_type = selected_providers().len() * 2 + 1;
        assert_eq!(entries.len(), per_type * 2);
    }

    #[test]
    fn arrivals_catalog_per_provider_is_named_from_the_provider() {
        let entries = catalog_entries(selected_providers());
        // "New on Netflix" is derived from "Popular on Netflix" — adding a service needs only its
        // PROVIDERS row, no second name to keep in sync.
        assert!(entries.iter().any(|e| e.id == "jw-nfx-new" && e.type_ == "movie" && e.name == "New on Netflix"));
        assert!(entries.iter().any(|e| e.id == "jw-pmp-new" && e.type_ == "series" && e.name == "New on Paramount+"));
        // Every provider gets both rows, for both types.
        for p in selected_providers() {
            for t in STREMIO_TYPES {
                assert!(entries.iter().any(|e| e.id == p.id && e.type_ == t), "missing popular {} {t}", p.id);
                assert!(entries.iter().any(|e| e.id == new_catalog_id(p) && e.type_ == t), "missing new {} {t}", p.id);
            }
        }
    }

    #[tokio::test]
    async fn a_provider_the_country_lacks_yields_no_row() {
        let fake = Arc::new(Fake {
            data: vec![item("tt1", "A", 0)],
            calls: AtomicUsize::new(0),
            fail_after: 9,
            new_calls: AtomicUsize::new(0),
            no_packages: false,
            package_calls: AtomicUsize::new(0),
            empty_packages: false,
            popular_ok_first_only: false,
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            dwell: false,
        });
        let s = CatalogState::new(fake.clone(), Duration::from_secs(60));
        // SkyShowtime (1773) isn't in the stub's country list — the row is absent, not empty-but-present,
        // and no upstream call is made for it.
        assert!(s.metas_json("jw-sst", "movie", "UY", selected_providers()).await.is_none());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        // Paramount+ (531) is, so it resolves and fetches.
        assert!(s.metas_json("jw-pmp", "movie", "UY", selected_providers()).await.is_some());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn resolve_catalog_distinguishes_popular_from_arrivals() {
        let ps = selected_providers();
        assert_eq!(resolve_catalog("jw-nfx", ps).map(|(p, n)| (p.code, n)), Some(("nfx", false)));
        assert_eq!(resolve_catalog("jw-nfx-new", ps).map(|(p, n)| (p.code, n)), Some(("nfx", true)));
        assert!(resolve_catalog("jw-bogus", ps).is_none());
        assert!(resolve_catalog("jw-bogus-new", ps).is_none());
    }

    #[tokio::test]
    async fn arrivals_id_routes_to_new_titles_not_popular() {
        // Keep the Fake so the test can read which arm ran — no test-only hook in production code.
        let fake = Arc::new(Fake {
            data: vec![item("tt1", "A", 0)],
            calls: AtomicUsize::new(0),
            fail_after: 9,
            new_calls: AtomicUsize::new(0),
            no_packages: false,
            package_calls: AtomicUsize::new(0),
            empty_packages: false,
            popular_ok_first_only: false,
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            dwell: false,
        });
        let s = CatalogState::new(fake.clone(), Duration::from_secs(60));
        // The arrivals row must not silently serve the Popular chart — that would look plausible and be wrong.
        let r = s.metas_json("jw-nfx-new", "movie", "US", selected_providers()).await;
        assert!(r.is_some());
        assert_eq!(fake.new_calls.load(Ordering::SeqCst), 1);

        let _ = s.metas_json("jw-nfx", "movie", "US", selected_providers()).await;
        assert_eq!(fake.new_calls.load(Ordering::SeqCst), 1, "the Popular row must not hit new_titles");
    }

    #[test]
    fn empty_provider_set_yields_no_catalog_entries() {
        assert!(catalog_entries(&[]).is_empty());
    }

    /// The aggregate row is a union over the install's OWN provider selection, so the selection is
    /// part of what the value is. Without it in the key, whichever install warmed the entry won for
    /// the whole 6h TTL: a Netflix-only chart was served verbatim to someone who picked four
    /// services, and a four-service chart to someone who picked one — titles they cannot stream.
    #[tokio::test]
    async fn the_aggregate_row_is_keyed_by_the_install_s_providers() {
        let s = state(vec![item("tt1", "A", 0)], Duration::from_secs(3600), usize::MAX);
        let one: Vec<&'static Provider> = vec![provider_by_code("nfx").unwrap()];
        let two: Vec<&'static Provider> =
            vec![provider_by_code("nfx").unwrap(), provider_by_code("mxx").unwrap()];

        let a = s.metas_json(TRENDING_ID, "movie", "US", &one).await.unwrap();
        let b = s.metas_json(TRENDING_ID, "movie", "US", &two).await.unwrap();
        assert!(a.fresh && b.fresh);
        assert!(s.cache_len() >= 2, "two different selections shared one entry");

        // Same SIZE, different membership — the case that matters and the one a count-based key
        // gets wrong. Keying on `codes.len()` passes everything above while serving a Netflix
        // install's chart to an HBO Max install, which is the whole bug.
        let nfx_only: Vec<&'static Provider> = vec![provider_by_code("nfx").unwrap()];
        let mxx_only: Vec<&'static Provider> = vec![provider_by_code("mxx").unwrap()];
        let before = s.cache_len();
        let _ = s.metas_json(TRENDING_ID, "series", "US", &nfx_only).await.unwrap();
        let mid = s.cache_len();
        let _ = s.metas_json(TRENDING_ID, "series", "US", &mxx_only).await.unwrap();
        assert!(
            s.cache_len() > mid && mid > before,
            "two same-size selections with different members shared a cache entry"
        );

        // ...and the selection is a SET: order must not split the cache into duplicates.
        let reordered: Vec<&'static Provider> =
            vec![provider_by_code("mxx").unwrap(), provider_by_code("nfx").unwrap()];
        let before = s.cache_len();
        let c = s.metas_json(TRENDING_ID, "movie", "US", &reordered).await.unwrap();
        assert!(c.fresh);
        assert_eq!(s.cache_len(), before, "reordering the same selection split the cache");
    }

    /// A package lookup that fails must not send an unvalidated code upstream. JustWatch does not
    /// reject a code it doesn't recognise — it returns the country's whole chart — so guessing
    /// published everything under one service's name, and the non-empty response meant the
    /// "0 usable items" schema-change guard never fired either.
    #[tokio::test]
    async fn an_unresolvable_provider_is_never_guessed() {
        let fake = Arc::new(fake(vec![item("tt1", "A", 0)], usize::MAX, true));
        let s = CatalogState::new(fake.clone(), Duration::from_secs(60));
        let r = s.metas_json("jw-nfx", "movie", "US", selected_providers()).await;

        // Never a guessed code: JustWatch does not reject one it doesn't know, it returns the
        // country's whole chart, so a guess publishes everything under one service's name.
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "an unresolved code was sent upstream anyway"
        );
        // ...and it degrades rather than 404ing a catalog the manifest advertises — the manifest
        // promises the row, and a lookup we could not make is a refresh failure, not a missing route.
        let r = r.expect("an advertised row must not 404 because the upstream was unreachable");
        assert!(!r.fresh, "a failed lookup was reported as a fresh answer");
        assert!(r.body.contains(r#""metas":[]"#), "{}", r.body);
    }

    /// ...while a country that genuinely does not carry the service still has NO row — absent, not
    /// empty-but-present. The two look identical at the call site and must not be conflated.
    #[tokio::test]
    async fn a_service_the_country_lacks_still_has_no_row() {
        let s = state(vec![item("tt1", "A", 0)], Duration::from_secs(60), usize::MAX);
        // SkyShowtime (1773) isn't in the stub's package list, which IS resolvable.
        assert!(s.metas_json("jw-sst", "movie", "UY", selected_providers()).await.is_none());
    }

    /// The package list is fetched once per aggregate, not once per provider. A failing lookup is
    /// deliberately not cached, so awaiting it per provider cost a full upstream timeout each,
    /// serially — ~64s for the default seven, inside one request with no server-side deadline.
    #[tokio::test]
    async fn the_aggregate_resolves_packages_once() {
        let fake = Arc::new(fake(vec![item("tt1", "A", 0)], usize::MAX, true));
        let s = CatalogState::new(fake.clone(), Duration::from_secs(60));
        let _ = s.metas_json(TRENDING_ID, "movie", "US", selected_providers()).await;
        assert_eq!(
            fake.package_calls.load(Ordering::SeqCst),
            1,
            "one failing package lookup per provider, serially, is a per-request timeout multiplier"
        );
    }

    /// "Trending Everywhere" is a union. Rendering the survivors of a partial fetch turned it into
    /// one service's chart — cached fresh for 6h plus an hour of CDN, with /health green — and the
    /// providers fire simultaneously at one host on one timeout, so a single slow one is enough.
    #[tokio::test]
    async fn a_partial_aggregate_is_not_published_as_the_union() {
        let mut f = fake(vec![item("tt1", "A", 0)], usize::MAX, false);
        f.popular_ok_first_only = true;
        let fake = Arc::new(f);
        let s = CatalogState::new(fake.clone(), Duration::from_secs(3600));

        let r = s
            .metas_json(TRENDING_ID, "movie", "US", selected_providers())
            .await
            .expect("a failed refresh degrades, it does not 404");
        assert!(!r.fresh, "a partial union was published as a complete, fresh answer");
        assert!(!s.fresh(), "/health stayed green while most providers failed");
    }

    /// The health flag is what /health reports (ADDON-02), and it is a different field from the
    /// per-response `fresh`. Nothing asserted it: the line clearing it could be deleted outright and
    /// the whole suite still passed.
    #[tokio::test]
    async fn a_failed_refresh_clears_the_health_flag() {
        let fake = Arc::new(fake(vec![item("tt1", "A", 0)], 0, false)); // fails from the first call
        let s = CatalogState::new(fake.clone(), Duration::from_secs(3600));
        assert!(s.fresh(), "starts optimistic");

        let _ = s.metas_json(TRENDING_ID, "movie", "US", selected_providers()).await;
        assert!(!s.fresh(), "an upstream failure left /health reporting ok");
    }

    /// A small market whose package list is legitimately empty is an ANSWER, not an outage. Flattening
    /// `Ok(vec![])` and `Err(())` together made such a country permanently `degraded: stale_catalog`
    /// and re-fetched on every single request, which pushes hardest exactly when upstream is refusing.
    #[tokio::test]
    async fn a_country_that_carries_nothing_is_an_answer_not_an_outage() {
        let mut f = fake(vec![item("tt1", "A", 0)], usize::MAX, false);
        f.empty_packages = true;
        let fake = Arc::new(f);
        let s = CatalogState::new(fake.clone(), Duration::from_secs(3600));

        // No row for a service the market doesn't carry...
        assert!(s.metas_json("jw-nfx", "movie", "ZZ", selected_providers()).await.is_none());
        assert!(s.fresh(), "an empty market was reported as an upstream outage");

        // ...and the answer is cached, rather than re-asked on every request.
        let after = fake.package_calls.load(Ordering::SeqCst);
        let _ = s.metas_json("jw-mxx", "movie", "ZZ", selected_providers()).await;
        assert_eq!(
            fake.package_calls.load(Ordering::SeqCst),
            after,
            "an empty package list was re-fetched on every request"
        );
    }

    /// Bounding the fan-out PER REQUEST is not a bound on the process. The caller picks the cache
    /// key — country × provider subset is ~171k distinct keys — so every request is its own miss
    /// with its own single-flight gate and nothing coalesces. ~1,000 concurrent GETs still reached
    /// ~4,000 simultaneous upstream calls, which is what got this host 403'd. Upstream cares about
    /// the total, so the total is what has to be capped.
    #[tokio::test]
    async fn upstream_concurrency_is_capped_across_requests() {
        let mut f = fake(vec![item("tt1", "A", 0)], usize::MAX, false);
        f.dwell = true;
        let fake = Arc::new(f);
        let s = Arc::new(CatalogState::new(fake.clone(), Duration::from_secs(3600)));

        // Distinct countries ⇒ distinct keys ⇒ no coalescing, exactly the attacker's shape.
        let mut set = JoinSet::new();
        for i in 0..40 {
            let s = Arc::clone(&s);
            let country = format!("{}{}", (b'A' + (i / 26) as u8) as char, (b'A' + (i % 26) as u8) as char);
            set.spawn(async move {
                let _ = s.metas_json(TRENDING_ID, "movie", &country, selected_providers()).await;
            });
        }
        while set.join_next().await.is_some() {}

        let peak = fake.peak.load(Ordering::SeqCst);
        assert!(peak > 1, "the probe never overlapped, so it proves nothing (peak {peak})");
        assert!(
            peak <= MAX_UPSTREAM_INFLIGHT,
            "{peak} simultaneous upstream calls against a cap of {MAX_UPSTREAM_INFLIGHT} — \
             per-request bounds do not bound the process"
        );
    }
}
