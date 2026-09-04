//! Isolated JustWatch source (docs/CATALOG-justwatch.md). Fetches "most popular" (TRENDING) titles for
//! a streaming provider via the unofficial GraphQL API and maps them to IMDb-keyed items. Nothing here
//! can affect the dataset resource: every failure returns `Err(())`, which the handler turns into empty
//! rows. User input never reaches this module — provider codes come from a fixed table.

use crate::util::since_start;
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Stremio content type ↔ JustWatch objectType.
#[derive(Clone, Copy)]
pub enum ObjectType {
    Movie,
    Show,
}

impl ObjectType {
    fn as_jw(self) -> &'static str {
        match self {
            ObjectType::Movie => "MOVIE",
            ObjectType::Show => "SHOW",
        }
    }
    pub fn from_stremio(t: &str) -> Option<ObjectType> {
        match t {
            "movie" => Some(ObjectType::Movie),
            "series" => Some(ObjectType::Show),
            _ => None,
        }
    }
}

/// One trending title, keyed by a validated IMDb id, plus the TMDB id when JustWatch supplies one.
/// `rank` is the 0-based position in its source list. The Den app maps catalog rows through TMDB, so
/// `moviedb` is what lets a row actually render there; a plain Stremio client uses the IMDb id.
#[derive(Debug, Clone, PartialEq)]
pub struct TrendingItem {
    pub imdb: String,
    pub moviedb: Option<i64>,
    pub title: String,
    pub rank: usize,
    /// IMDb score from JustWatch's own `scoring` (free — same response), surfaced as the card rating.
    pub rating: Option<f64>,
    /// Original release year from JustWatch (free — same response), surfaced as the card year.
    pub year: Option<i64>,
}

/// The source seam — the real JustWatch client in prod, a fake in tests. `country` is per-request (an
/// ISO-3166 code) so one client serves every configured/forwarded region.
#[async_trait]
pub trait TrendingSource: Send + Sync {
    /// `sort` is JustWatch's `PopularTitlesSorting` (e.g. "POPULAR" for the "Popular on <service>" rows,
    /// "TRENDING" for the aggregated "Trending Everywhere" chart) — the two rank very differently.
    async fn popular(
        &self,
        provider: &str,
        obj: ObjectType,
        country: &str,
        sort: &str,
    ) -> Result<Vec<TrendingItem>, ()>;

    /// Titles **newly added to a service**, most recently added first. This is the one signal TMDB cannot
    /// provide at all (it carries no added-date and no such sort), so it only exists on this path.
    async fn new_titles(
        &self,
        provider: &str,
        obj: ObjectType,
        country: &str,
    ) -> Result<Vec<TrendingItem>, ()>;

    /// The services a country actually has, as `packageId -> shortName`.
    ///
    /// Needed because **both identifiers are per-country**: Amazon Prime Video is `prv`/119 in UY and FI but
    /// `amp`/9 in the US (verified 2026-07-31). A table hardcoding either is wrong somewhere, which is how
    /// "Popular on Prime Video" shipped empty. Providers are declared by their stable ids and the local code is
    /// looked up here.
    async fn packages(&self, country: &str) -> Result<Vec<(i64, String)>, ()>;

    /// How many charts have come back looking like a schema break rather than an answer.
    ///
    /// A partial break is invisible in every other signal: the row is short but non-empty, so it
    /// caches as complete for the full TTL, serves with a long max-age, and the refresh counts as a
    /// success. Without this the only trace was a line on stderr, which nothing monitors.
    /// Default 0, so a source that cannot detect it simply never reports one.
    fn suspected_schema_breaks(&self) -> usize {
        0
    }

    /// How long ago the last suspected break was seen, or `None` for never. `/health` needs current
    /// state, and a lifetime count is not that — one transient would leave it reporting "rows may be
    /// short" for the life of the process.
    fn last_schema_break_age(&self) -> Option<Duration> {
        None
    }
}

const ENDPOINT: &str = "https://apis.justwatch.com/graphql";
const MAX_BODY: usize = 4 << 20; // cap the response body (a hostile/huge reply must not OOM us)

// Mirrors rleroi/Stremio-Streaming-Catalogs-Addon's GetPopularTitles (only the fields we use).
const QUERY: &str = r#"query GetPopularTitles($country: Country!, $first: Int!, $popularTitlesSortBy: PopularTitlesSorting!, $packages: [String!], $objectTypes: [ObjectType!]) {
  popularTitles(country: $country, first: $first, sortBy: $popularTitlesSortBy, filter: { objectTypes: $objectTypes, packages: $packages }) {
    edges { node { content(country: $country, language: "en") { title originalReleaseYear externalIds { imdbId tmdbId } scoring { imdbScore } } } }
  }
}"#;

// Arrivals — titles newly added to a service, most recently added first.
//
// Uses `newTitles`, NOT `newTitleBuckets`: verified 2026-07-30 that `newTitleBuckets` **silently ignores the
// `packages` filter** (nfx, mxx and hbm all returned byte-identical results), so a per-service row built on it
// would have shown "new in Finland" under a service's name — plausible-looking and wrong. `newTitles` honours
// `packages` (a bogus code correctly returns nothing).
//
// An entry is a Movie, a Show, or a **Season**, and seasons are a large share of arrivals. A Season's own
// `content` is useless to us ("Season 1", `tmdbId` "326119:1", no IMDb id), so we take `show { content }` and
// surface the SHOW. Dropping seasons would gut the row.
const NEW_QUERY: &str = r#"query GetNewTitles($country: Country!, $filter: TitleFilter, $first: Int!) {
  newTitles(country: $country, filter: $filter, first: $first) {
    edges { node { __typename
      ... on MovieOrShowOrSeason { content(country: $country, language: "en") { title originalReleaseYear externalIds { imdbId tmdbId } scoring { imdbScore } } }
      ... on Season { show { content(country: $country, language: "en") { title originalReleaseYear externalIds { imdbId tmdbId } scoring { imdbScore } } } }
    } }
  }
}"#;

const PACKAGES_QUERY: &str = r#"query GetPackages($country: Country!, $platform: Platform!) {
  packages(country: $country, platform: $platform) { packageId shortName monetizationTypes }
}"#;

pub struct JustWatchClient {
    // None if the client couldn't be built (TLS backend init) — catalog then degrades to empty rows
    // instead of `reqwest::Client::new()` panicking at startup.
    http: Option<reqwest::Client>,
    /// Always ENDPOINT in production; a test points it at a local socket so the 200-with-errors
    /// path can be exercised against a real response rather than asserted on a helper in isolation.
    endpoint: String,
    /// How many charts have come back looking like a schema break. Read by the tests: the warning is
    /// a log line, and a log line cannot be asserted on — so testing the predicate alone left the
    /// CALL SITE free to drop it, which is the failure this file already documents for
    /// `graphql_error`.
    suspected_schema_breaks: AtomicUsize,
    /// When the last one was seen, as milliseconds since process start PLUS ONE, so 0 can mean
    /// "never" without colliding with a break at t=0. Monotonic (`util::since_start`): a wall clock
    /// steps under NTP, which pinned the age at zero going back and expired a live break going
    /// forward, while a pre-epoch clock stored the sentinel itself. Monotonic (`util::since_start`): a wall clock
    /// steps under NTP, which pinned the age at zero going back and expired a live break going
    /// forward, and a pre-epoch clock stored the sentinel itself. `/health` reads recency, not the count: a total
    /// is a lifetime figure, and reporting "rows may be short" in the present tense forever after one
    /// transient makes the signal something to ignore.
    last_schema_break: AtomicU64,
}

impl JustWatchClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("den-atlas/0.1 (+https://github.com/oxyc/den)")
            .build()
            .map_err(|e| eprintln!("den-atlas: reqwest client build failed ({e}); catalog disabled"))
            .ok();
        Self {
            http,
            endpoint: ENDPOINT.to_string(),
            suspected_schema_breaks: AtomicUsize::new(0),
            last_schema_break: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn with_endpoint(endpoint: String) -> Self {
        let mut c = Self::new();
        c.endpoint = endpoint;
        c
    }
}

impl Default for JustWatchClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct GqlResp {
    data: Option<GqlData>,
}
#[derive(Deserialize)]
struct GqlData {
    #[serde(rename = "popularTitles")]
    popular: Option<PopularTitles>,
}
#[derive(Deserialize)]
struct PopularTitles {
    edges: Vec<Edge>,
}
#[derive(Deserialize)]
struct Edge {
    node: Node,
}
#[derive(Deserialize)]
struct Node {
    // Optional, like every level of parse_new_titles and for the same reason: an upstream reply is
    // untrusted. These were required, so ONE edge with a null content or a missing title failed the
    // whole document — and an unparseable chart became `Ok(vec![])`, which the caller treats as a
    // complete answer, caches, marks fresh, and reports healthy. A title with no `en` localisation
    // for that country is enough.
    content: Option<Content>,
}
#[derive(Deserialize)]
struct Content {
    title: Option<String>,
    #[serde(rename = "originalReleaseYear")]
    original_release_year: Option<i64>,
    #[serde(rename = "externalIds")]
    external_ids: Option<ExternalIds>,
    scoring: Option<Scoring>,
}
#[derive(Deserialize)]
struct ExternalIds {
    #[serde(rename = "imdbId")]
    imdb_id: Option<String>,
    #[serde(rename = "tmdbId")]
    tmdb_id: Option<String>,
}
#[derive(Deserialize)]
struct Scoring {
    #[serde(rename = "imdbScore")]
    imdb_score: Option<f64>,
}

/// A parsed chart: the usable items, plus how many edges the document actually carried.
///
/// The edge count is what makes a PARTIAL schema change visible. Dropping a bad edge rather than
/// failing the whole document is right, but a half-length row is then cached as complete for the
/// full TTL, served with a long max-age, and /health stays green — so the shortfall has no other
/// way to surface. Counting only the specific shapes we know about does not work: a rename under
/// `externalIds` drops every edge through the ordinary no-IMDb-id path, which is indistinguishable
/// from the routine case. The ratio catches any of them, including ones not thought of here.
pub struct Chart {
    pub items: Vec<TrendingItem>,
    /// Whether the response carried a `popularTitles` node at all. An empty chart is a legitimate
    /// answer — a provider may genuinely carry no movies in a small market — but a document with no
    /// such node is the shape a rename or a `{"data":null}` produces, and those look identical once
    /// both have collapsed to zero items.
    pub present: bool,
    /// Edges present before any were dropped.
    pub edges: usize,
}

/// Parse a GraphQL response body into ranked items, dropping anything without a valid IMDb id (it
/// couldn't be resolved by Cinemeta/other addons). Never panics; a malformed body → empty list.
pub fn parse_popular(body: &str) -> Chart {
    let resp: GqlResp = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(_) => return Chart { items: Vec::new(), present: false, edges: 0 },
    };
    let Some(popular) = resp.data.and_then(|d| d.popular) else {
        return Chart { items: Vec::new(), present: false, edges: 0 };
    };
    let edges = popular.edges;
    let total = edges.len();
    let mut out = Vec::with_capacity(total);
    for e in edges {
        let Some(content) = e.node.content else { continue };
        // A present-but-empty title is as unusable as a missing one — it renders as a nameless card.
        // Not a schema signal on its own: this file documents `""` as what JustWatch sends for a
        // title with no localisation in that country, which is routine in a non-English market.
        let Some(title) = content.title.filter(|t| !t.trim().is_empty()) else { continue };
        let ext = content.external_ids;
        let imdb = match ext.as_ref().and_then(|x| x.imdb_id.clone()) {
            Some(id) if is_imdb(&id) => id,
            _ => continue,
        };
        let moviedb = ext.and_then(|x| x.tmdb_id).and_then(|s| s.parse::<i64>().ok());
        let rating = content.scoring.and_then(|s| s.imdb_score);
        let year = content.original_release_year;
        let rank = out.len();
        out.push(TrendingItem { imdb, moviedb, title, rank, rating, year });
    }
    Chart { items: out, present: true, edges: total }
}

/// The warning to print when a chart looks like a schema break rather than an ordinary chart, or
/// `None` when it looks normal.
///
/// A non-empty body yielding zero usable items is the classic signal (it would otherwise serve empty
/// rows forever). Most of a chart going missing is the same signal arriving one edge at a time: a
/// rename under `externalIds` leaves the document parseable and every row short, and no count of
/// specific known-bad shapes sees it, because those edges drop through the ordinary no-IMDb-id path.
/// A ratio catches any shortfall, including shapes not thought of here.
///
/// Half is well clear of the routine drop rate — titles with no resolvable IMDb id are a small
/// minority of any real chart — so this stays quiet in normal operation, which is the whole point of
/// it. Dropping a few titles that have no localisation in a given country is expected and says
/// nothing; treating that as a schema alarm put a line on the hot path per provider per request, for
/// a condition this file documents as normal.
fn schema_break_warning(label: &str, body: &str, chart: &Chart) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    if !chart.present {
        return Some(format!(
            "den-atlas: justwatch {label} returned no popularTitles node — possible schema change"
        ));
    }
    // An empty chart is an answer, so say nothing: `0 of 0` is not a shortfall, and the previous
    // form of this check reported it as one on every cold fetch for any row a provider genuinely
    // has nothing in. The shortfall test only means anything once there are edges to fall short of.
    if chart.edges == 0 || chart.items.len() * 2 >= chart.edges {
        return None;
    }
    Some(format!(
        "den-atlas: justwatch {label} yielded {} usable titles from {} edges — possible schema change",
        chart.items.len(),
        chart.edges
    ))
}

/// Arrivals response. Defensive throughout (an addon reply is untrusted): every level optional, a malformed
/// body yields an empty list rather than a panic.
#[derive(Deserialize)]
struct NewResp {
    data: Option<NewData>,
}
#[derive(Deserialize)]
struct NewData {
    #[serde(rename = "newTitles")]
    new_titles: Option<NewTitles>,
}
#[derive(Deserialize)]
struct NewTitles {
    edges: Vec<NewEdge>,
}
#[derive(Deserialize)]
struct NewEdge {
    node: NewNode,
}
#[derive(Deserialize)]
struct NewNode {
    #[serde(rename = "__typename")]
    typename: Option<String>,
    content: Option<NewContent>,
    /// Present on a `Season` — its own content is useless ("Season 1", no IMDb id), so we surface the show.
    show: Option<NewShow>,
}
#[derive(Deserialize)]
struct NewShow {
    content: Option<NewContent>,
}
#[derive(Deserialize)]
struct NewContent {
    title: Option<String>,
    #[serde(rename = "originalReleaseYear")]
    original_release_year: Option<i64>,
    #[serde(rename = "externalIds")]
    external_ids: Option<ExternalIds>,
    scoring: Option<Scoring>,
}

/// Parse an arrivals body into items, **most recently added first** (the service's own order, preserved — that
/// ordering IS the feature).
///
/// A show whose new season lands is surfaced as the SHOW, deduped so a weekly series appears once. Items
/// without a usable IMDb id are dropped, matching `parse_popular` and the addon's `idPrefixes: ["tt"]` contract.
///
/// **Filters on `__typename` rather than trusting the request.** `newTitles` honours `objectTypes: [MOVIE]` but
/// NOT `[SHOW]` — verified 2026-07-30, a SHOW query returned 3 Movies among 12 results. Serving those in a
/// series catalog made the app resolve a movie id as a TV id, so the card opened to "couldn't be found on
/// TMDB". (`popularTitles` filters correctly; this is specific to `newTitles`.)
pub fn parse_new_titles(body: &str, want: ObjectType) -> Vec<TrendingItem> {
    let resp: NewResp = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let edges = resp.data.and_then(|d| d.new_titles).map(|n| n.edges).unwrap_or_default();
    let mut out: Vec<TrendingItem> = Vec::with_capacity(edges.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in edges {
        // Keep only entries of the requested kind. A Season counts as a show — it resolves to one below.
        let kind = e.node.typename.as_deref().unwrap_or_default();
        let matches = match want {
            ObjectType::Movie => kind == "Movie",
            ObjectType::Show => kind == "Show" || kind == "Season",
        };
        if !matches {
            continue;
        }
        // A Season's `show.content` wins over its own; a Movie/Show has no `show`.
        let content = match e.node.show.and_then(|s| s.content).or(e.node.content) {
            Some(c) => c,
            None => continue,
        };
        let ext = content.external_ids;
        let imdb = match ext.as_ref().and_then(|x| x.imdb_id.clone()) {
            Some(id) if is_imdb(&id) => id,
            _ => continue,
        };
        // Same rule as parse_popular: a nameless card is not worth a row slot. Checked BEFORE the
        // dedupe below, so a title-less entry doesn't claim the id and suppress a later good one.
        let Some(title) = content.title.filter(|t| !t.trim().is_empty()) else { continue };
        if !seen.insert(imdb.clone()) {
            continue; // a weekly series drops a season repeatedly
        }
        let moviedb = ext.and_then(|x| x.tmdb_id).and_then(|s| s.parse::<i64>().ok());
        let rank = out.len();
        out.push(TrendingItem {
            imdb,
            moviedb,
            title,
            rank,
            rating: content.scoring.and_then(|s| s.imdb_score),
            year: content.original_release_year,
        });
    }
    out
}

#[derive(Deserialize)]
struct PkgResp {
    data: Option<PkgData>,
}
#[derive(Deserialize)]
struct PkgData {
    packages: Option<Vec<PkgEntry>>,
}
#[derive(Deserialize)]
struct PkgEntry {
    #[serde(rename = "packageId")]
    package_id: Option<i64>,
    #[serde(rename = "shortName")]
    short_name: Option<String>,
    #[serde(rename = "monetizationTypes")]
    monetization_types: Option<Vec<String>>,
}

/// `packageId -> shortName` for a country, restricted to services you can watch on a subscription. Rent/buy
/// storefronts are excluded: a "Popular on X" row is about what a subscription covers.
pub fn parse_packages(body: &str) -> Vec<(i64, String)> {
    let resp: PkgResp = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    resp.data
        .and_then(|d| d.packages)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.monetization_types
                .as_ref()
                .is_some_and(|m| m.iter().any(|t| t == "FLATRATE" || t == "FREE" || t == "ADS"))
        })
        .filter_map(|p| Some((p.package_id?, p.short_name?)))
        .collect()
}

fn is_imdb(s: &str) -> bool {
    s.len() >= 3 && s.starts_with("tt") && s[2..].bytes().all(|b| b.is_ascii_digit())
}

impl JustWatchClient {
    /// POST one GraphQL payload and return the body, bounded as it streams — reqwest has no default size
    /// limit, so `.bytes()` would buffer a hostile/huge reply in full before any check. Shared by both
    /// queries; `label` only names the request in log lines.
    async fn post_graphql(&self, payload: &serde_json::Value, label: &str) -> Result<String, ()> {
        let http = match self.http.as_ref() {
            Some(h) => h,
            None => return Err(()),
        };
        let mut resp = match http.post(&self.endpoint).json(payload).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("den-atlas: justwatch request failed ({label}): {e}");
                return Err(());
            }
        };
        if !resp.status().is_success() {
            eprintln!("den-atlas: justwatch http {} ({label})", resp.status());
            return Err(());
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| ())? {
            if buf.len() + chunk.len() > MAX_BODY {
                eprintln!("den-atlas: justwatch body exceeded {MAX_BODY} bytes ({label}) — dropping");
                return Err(());
            }
            buf.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        // GraphQL reports failures with HTTP 200 and {"errors":[…],"data":null}. Checking only the
        // status made a bad country, a rate-limit, a validation error or a schema change look like
        // "this row is empty" — which was then stored as a fresh answer over the last-good rows,
        // pinned for the cache TTL and an hour of CDN max-age, with /health still reporting ok.
        if let Some(why) = graphql_error(&body) {
            eprintln!("den-atlas: justwatch graphql error ({label}): {why}");
            return Err(());
        }
        Ok(body)
    }
}

/// The first GraphQL error message, if the response carries any. `data: null` alongside errors is
/// the unambiguous failure shape; errors beside partial data are still a failure for our purposes,
/// because a partial chart is not a chart.
fn graphql_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let errors = v.get("errors")?.as_array()?;
    let first = errors.first()?;
    Some(first.get("message").and_then(|m| m.as_str()).unwrap_or("unspecified").to_string())
}

#[async_trait]
impl TrendingSource for JustWatchClient {
    async fn popular(
        &self,
        provider: &str,
        obj: ObjectType,
        country: &str,
        sort: &str,
    ) -> Result<Vec<TrendingItem>, ()> {
        let payload = serde_json::json!({
            "query": QUERY,
            "variables": {
                "country": country,
                "first": 100,
                "popularTitlesSortBy": sort,
                "packages": [provider],
                "objectTypes": [obj.as_jw()],
            },
        });
        let label = format!("{provider}/{}", obj.as_jw());
        let body = self.post_graphql(&payload, &label).await?;
        let chart = parse_popular(&body);
        if let Some(w) = schema_break_warning(&label, &body, &chart) {
            self.suspected_schema_breaks.fetch_add(1, Ordering::Relaxed);
            self.last_schema_break.store(since_start().as_millis() as u64 + 1, Ordering::Relaxed);
            eprintln!("{w}");
        }
        Ok(chart.items)
    }

    fn suspected_schema_breaks(&self) -> usize {
        self.suspected_schema_breaks.load(Ordering::Relaxed)
    }

    fn last_schema_break_age(&self) -> Option<Duration> {
        match self.last_schema_break.load(Ordering::Relaxed) {
            0 => None,
            at => Some(since_start().saturating_sub(Duration::from_millis(at - 1))),
        }
    }

    async fn packages(&self, country: &str) -> Result<Vec<(i64, String)>, ()> {
        let payload = serde_json::json!({
            "query": PACKAGES_QUERY,
            "variables": { "country": country, "platform": "WEB" },
        });
        let body = self.post_graphql(&payload, &format!("packages/{country}")).await?;
        let pkgs = parse_packages(&body);
        if pkgs.is_empty() && !body.is_empty() {
            eprintln!("den-atlas: justwatch packages returned a non-empty body but 0 usable entries ({country}) — possible schema change");
        }
        Ok(pkgs)
    }

    async fn new_titles(
        &self,
        provider: &str,
        obj: ObjectType,
        country: &str,
    ) -> Result<Vec<TrendingItem>, ()> {
        let payload = serde_json::json!({
            "query": NEW_QUERY,
            "variables": {
                "country": country,
                // Same page size as the popular rows; `newTitles` accepts 100 (unlike `newTitleBuckets`,
                // which rejects >~20 with TOO_BIG).
                "first": 100,
                "filter": { "packages": [provider], "objectTypes": [obj.as_jw()] },
            },
        });
        let label = format!("new:{provider}/{}", obj.as_jw());
        let body = self.post_graphql(&payload, &label).await?;
        let items = parse_new_titles(&body, obj);
        if items.is_empty() && !body.is_empty() {
            eprintln!("den-atlas: justwatch new-titles returned a non-empty body but 0 usable items ({label}) — possible schema change");
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shapes seen from JustWatch FI: a Movie, a Season (whose own ids are useless), the same show
    /// recurring from a second season drop, and an entry with no IMDb id.
    const NEW_FIXTURE: &str = r#"{"data":{"newTitles":{"edges":[
      {"node":{"__typename":"Movie","content":{"title":"Aquaman","originalReleaseYear":2018,"externalIds":{"imdbId":"tt1477834","tmdbId":"297802"},"scoring":{"imdbScore":6.9}}}},
      {"node":{"__typename":"Season","content":{"title":"Season 1","externalIds":{"imdbId":null,"tmdbId":"326119:1"}},"show":{"content":{"title":"Thunder 3","originalReleaseYear":2026,"externalIds":{"imdbId":"tt43589481","tmdbId":"326119"},"scoring":{"imdbScore":7.1}}}}},
      {"node":{"__typename":"Show","content":{"title":"NoImdb","externalIds":{"imdbId":null,"tmdbId":"320938"}}}},
      {"node":{"__typename":"Season","content":{"title":"Season 9","externalIds":{"imdbId":null,"tmdbId":"326119:9"}},"show":{"content":{"title":"Thunder 3","externalIds":{"imdbId":"tt43589481","tmdbId":"326119"}}}}},
      {"node":{"__typename":"Movie","content":{"title":"Crawl","originalReleaseYear":2019,"externalIds":{"imdbId":"tt8364368","tmdbId":"570670"}}}}
    ]}}}"#;

    #[test]
    fn new_titles_are_ordered_newest_first() {
        let items = parse_new_titles(NEW_FIXTURE, ObjectType::Movie);
        // The service's own order is the feature: the most recently added title leads.
        assert_eq!(items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Aquaman", "Crawl"]);
        assert_eq!(items.iter().map(|i| i.rank).collect::<Vec<_>>(), vec![0, 1]);
    }

    /// The bug this guards: `newTitles` honours `objectTypes: [MOVIE]` but NOT `[SHOW]`, so a movie leaked into
    /// the series catalog, was emitted as `type: series`, and the app resolved a movie id as a TV id —
    /// "The Truthers" opened to "couldn't be found on TMDB".
    #[test]
    fn a_movie_never_leaks_into_the_series_catalog() {
        let shows = parse_new_titles(NEW_FIXTURE, ObjectType::Show);
        assert!(!shows.iter().any(|i| i.title == "Aquaman"), "a Movie must not appear in a SHOW request");
        assert!(!shows.iter().any(|i| i.title == "Crawl"));
        assert_eq!(shows.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Thunder 3"]);

        // ...and the converse: a Show/Season must not appear in a MOVIE request.
        let movies = parse_new_titles(NEW_FIXTURE, ObjectType::Movie);
        assert!(!movies.iter().any(|i| i.title == "Thunder 3"));
    }

    #[test]
    fn a_new_season_surfaces_its_show_not_the_season() {
        let items = parse_new_titles(NEW_FIXTURE, ObjectType::Show);
        let show = items.iter().find(|i| i.title == "Thunder 3").expect("season resolved to its show");
        // The season's own ids are unusable ("326119:1", no IMDb id); the show's are what Den maps through.
        assert_eq!(show.imdb, "tt43589481");
        assert_eq!(show.moviedb, Some(326119));
    }

    #[test]
    fn a_recurring_series_appears_once() {
        let items = parse_new_titles(NEW_FIXTURE, ObjectType::Show);
        // "Thunder 3" appears twice (two season drops) — it must not repeat down the row.
        assert_eq!(items.iter().filter(|i| i.title == "Thunder 3").count(), 1);
    }

    #[test]
    fn new_titles_drops_entries_without_a_usable_imdb_id() {
        let items = parse_new_titles(NEW_FIXTURE, ObjectType::Movie);
        // Matches parse_popular and the addon's `idPrefixes: ["tt"]` contract.
        assert!(!items.iter().any(|i| i.title == "NoImdb"));
    }

    #[test]
    fn malformed_new_titles_body_yields_no_items() {
        for body in ["", "{}", "not json", r#"{"data":null}"#, r#"{"data":{"newTitles":{"edges":[]}}}"#] {
            assert!(parse_new_titles(body, ObjectType::Movie).is_empty(), "body: {body}");
        }
    }

    const FIXTURE: &str = r#"{"data":{"popularTitles":{"edges":[
      {"node":{"content":{"title":"Alpha","originalReleaseYear":1999,"externalIds":{"imdbId":"tt0000001","tmdbId":"1397385"},"scoring":{"imdbScore":7.4}}}},
      {"node":{"content":{"title":"NoId","externalIds":{"imdbId":null}}}},
      {"node":{"content":{"title":"Bad","externalIds":{"imdbId":"nope"}}}},
      {"node":{"content":{"title":"Beta","externalIds":{"imdbId":"tt0000002","tmdbId":null},"scoring":{"imdbScore":null}}}}
    ]}}}"#;

    #[test]
    fn parses_ranks_and_carries_tmdb_rating_and_year() {
        let items = parse_popular(FIXTURE).items;
        assert_eq!(items.len(), 2, "items without a valid tt id are dropped");
        assert_eq!(
            items[0],
            TrendingItem {
                imdb: "tt0000001".into(),
                moviedb: Some(1397385),
                title: "Alpha".into(),
                rank: 0,
                rating: Some(7.4),
                year: Some(1999)
            }
        );
        assert_eq!(
            items[1],
            TrendingItem {
                imdb: "tt0000002".into(),
                moviedb: None,
                title: "Beta".into(),
                rank: 1,
                rating: None,
                year: None
            }
        );
    }

    /// A card with no name is not a card. Skipping a bad edge rather than failing the document is
    /// right, but the guard has to reject `""` as well as `null` — JustWatch sends the empty string
    /// for a title with no localisation in that country, and it rendered as a blank row slot in
    /// rank position 1.
    #[test]
    fn an_edge_with_no_usable_title_is_dropped_not_shipped_blank() {
        let body = r#"{"data":{"popularTitles":{"edges":[
            {"node":{"content":{"title":"","externalIds":{"imdbId":"tt0000001"}}}},
            {"node":{"content":{"externalIds":{"imdbId":"tt0000002"}}}},
            {"node":{"content":null}},
            {"node":{"content":{"title":"   ","externalIds":{"imdbId":"tt0000003"}}}},
            {"node":{"content":{"title":"Real","externalIds":{"imdbId":"tt0000004"}}}}
        ]}}}"#;
        let items = parse_popular(body).items;
        assert_eq!(items.len(), 1, "a nameless edge was shipped: {items:?}");
        assert_eq!(items[0].title, "Real");
        assert_eq!(items[0].rank, 0, "the surviving item did not take the top slot");
    }

    /// Same rule on the arrivals path, and the check has to come BEFORE the dedupe: a title-less
    /// edge that claims its imdb id in `seen` suppresses the good edge for the same title later in
    /// the list, which is exactly the shape a weekly series produces.
    #[test]
    fn a_title_less_arrival_does_not_suppress_its_own_good_edge() {
        let body = r#"{"data":{"newTitles":{"edges":[
            {"node":{"__typename":"Movie","content":{"title":"","externalIds":{"imdbId":"tt0000001"}}}},
            {"node":{"__typename":"Movie","content":{"title":"Real","externalIds":{"imdbId":"tt0000001"}}}}
        ]}}}"#;
        let items = parse_new_titles(body, ObjectType::Movie);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].title, "Real", "the blank edge claimed the id and hid the good one");
    }

    #[test]
    fn malformed_body_is_empty_not_panic() {
        assert!(parse_popular("not json").items.is_empty());
        assert!(parse_popular(r#"{"data":null}"#).items.is_empty());
    }

    #[test]
    fn imdb_validation() {
        assert!(is_imdb("tt0111161"));
        assert!(!is_imdb("tt"));
        assert!(!is_imdb("nm123"));
        assert!(!is_imdb("tt12a"));
    }

    /// GraphQL reports failures with HTTP 200 and `{"errors":[…],"data":null}`. Checking only the
    /// status made a bad country, a rate-limit, a validation error or a schema change look like
    /// "this row is empty" — which was then stored as a fresh answer OVER the last-good rows,
    /// pinned for the 6h cache TTL and an hour of CDN max-age, with /health still reporting ok.
    #[test]
    fn a_graphql_error_is_not_an_empty_row() {
        let body = r#"{"errors":[{"message":"locale by the country code: couldn't get locale with country code \"ZZ\"","extensions":{"code":"BAD_REQUEST"}}],"data":null}"#;
        let why = graphql_error(body).expect("a 200 carrying errors is a failure, not an empty chart");
        assert!(why.contains("locale"), "the log line must name the cause: {why}");

        // Errors beside partial data are still a failure — a partial chart is not a chart.
        assert!(graphql_error(
            r#"{"errors":[{"message":"rate limited"}],"data":{"popularTitles":{"edges":[]}}}"#
        )
        .is_some());

        // ...and a genuinely empty chart is NOT an error.
        assert!(graphql_error(r#"{"data":{"popularTitles":{"edges":[]}}}"#).is_none());
        assert!(graphql_error("not json at all").is_none());
    }

    /// Serve one fixed HTTP 200 body on an ephemeral port, then close.
    async fn serve_once(body: impl Into<String>) -> String {
        use tokio::io::AsyncWriteExt;
        let body: String = body.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// Build a chart body of `n` edges, of which `bad` are broken in the way `how` says.
    fn chart_body(n: usize, bad: usize, how: &str) -> String {
        let mut edges = Vec::new();
        for i in 0..n {
            let id = format!("tt{:07}", i + 1);
            edges.push(if i < bad {
                match how {
                    // No `en` localisation for this country — routine, and documented as such.
                    "no_title" => format!(
                        r#"{{"node":{{"content":{{"title":"","externalIds":{{"imdbId":"{id}"}}}}}}}}"#
                    ),
                    // A rename under externalIds: parseable, but every affected row loses its id.
                    _ => format!(
                        r#"{{"node":{{"content":{{"title":"T{i}","external_ids":{{"imdbId":"{id}"}}}}}}}}"#
                    ),
                }
            } else {
                format!(r#"{{"node":{{"content":{{"title":"T{i}","externalIds":{{"imdbId":"{id}"}}}}}}}}"#)
            });
        }
        format!(r#"{{"data":{{"popularTitles":{{"edges":[{}]}}}}}}"#, edges.join(","))
    }

    /// The alarm has to stay silent on a healthy chart, or it is not an alarm. A few titles with no
    /// localisation in a country is normal — treating it as a schema break put a line on the hot
    /// path for every provider of every request in any non-English market.
    #[test]
    fn a_healthy_chart_raises_no_schema_warning() {
        for how in ["no_title", "renamed"] {
            let body = chart_body(100, 3, how);
            let chart = parse_popular(&body);
            assert_eq!(chart.edges, 100);
            assert_eq!(chart.items.len(), 97, "{how}");
            assert_eq!(
                schema_break_warning("nfx/MOVIE", &body, &chart),
                None,
                "3 dropped titles out of 100 raised a schema alarm ({how})"
            );
        }
    }

    /// ...and it has to fire when most of the chart vanishes, whatever the shape of the break. A
    /// rename under `externalIds` drops edges through the ORDINARY no-IMDb-id path, so counting
    /// known-bad shapes never sees it; the row just comes back short, cached as complete for 6h,
    /// served with an hour of max-age, /health green.
    #[test]
    fn a_chart_that_mostly_vanished_raises_one() {
        let body = chart_body(100, 80, "renamed");
        let chart = parse_popular(&body);
        assert_eq!(chart.items.len(), 20);
        let w = schema_break_warning("nfx/MOVIE", &body, &chart)
            .expect("80 of 100 edges lost their id and nothing was reported");
        assert!(w.contains("20 usable titles from 100 edges"), "{w}");
        assert!(w.contains("nfx/MOVIE"), "the warning does not say which row: {w}");
    }

    /// An empty chart is an ANSWER; a missing node is a break. Both collapse to zero items, and the
    /// old check could not tell them apart — so a provider that genuinely carries nothing of that
    /// type in a small market logged "possible schema change" on every cold fetch, forever, in a
    /// line that read "0 usable titles from 0 edges". That is the condition this alarm exists to
    /// not fire on.
    #[test]
    fn an_empty_chart_is_an_answer_but_a_missing_node_is_a_break() {
        let empty = r#"{"data":{"popularTitles":{"edges":[]}}}"#;
        let chart = parse_popular(empty);
        assert!(chart.present, "an empty edge list is still a chart");
        assert_eq!(
            schema_break_warning("nfx/MOVIE", empty, &chart),
            None,
            "a genuinely empty row was reported as a schema change"
        );

        // The node itself gone — a rename, or a 200 carrying `data: null` — is the break the
        // zero-items check was originally there to catch, and it must survive the change above.
        for missing in
            [r#"{"data":null}"#, r#"{"data":{}}"#, r#"{"data":{"popular":{"edges":[]}}}"#, "not json"]
        {
            let chart = parse_popular(missing);
            assert!(!chart.present, "{missing} looked like a present chart");
            let w = schema_break_warning("nfx/MOVIE", missing, &chart)
                .unwrap_or_else(|| panic!("{missing} raised nothing"));
            assert!(w.contains("no popularTitles node"), "{w}");
        }

        // An empty body is a transport failure, reported elsewhere; not a schema claim.
        assert!(schema_break_warning("nfx/MOVIE", "", &parse_popular(empty)).is_none());
    }

    /// The threshold is "fewer than half", and both sides of that boundary are pinned — otherwise
    /// the constant can be moved (2 → 3) with every other test still green.
    #[test]
    fn the_shortfall_threshold_sits_exactly_at_half() {
        // Exactly half is fine; one below it is not.
        let half = chart_body(10, 5, "renamed");
        assert_eq!(
            schema_break_warning("nfx/MOVIE", &half, &parse_popular(&half)),
            None,
            "exactly half the chart surviving must not alarm"
        );
        let under = chart_body(10, 6, "renamed");
        assert!(
            schema_break_warning("nfx/MOVIE", &under, &parse_popular(&under)).is_some(),
            "4 of 10 surviving is a shortfall and must alarm"
        );
    }

    /// End-to-end through `popular`, so the parse the warning reasons about is the real one: a
    /// partial break still serves the surviving titles rather than failing the row.
    #[tokio::test]
    async fn a_partial_schema_break_still_serves_what_survived() {
        let base = serve_once(chart_body(10, 8, "renamed")).await;
        let c = JustWatchClient::with_endpoint(base);
        let items = c.popular("nfx", ObjectType::Movie, "US", "POPULAR").await.expect("row should not fail");
        assert_eq!(items.len(), 2, "a partial break should degrade, not empty the row");
        assert_eq!(items[0].rank, 0, "ranks must stay contiguous after the drops");
        assert_eq!(items[1].rank, 1);
        // ...and the break was NOTICED. Asserting the predicate alone leaves the call site free to
        // drop it — deleting the whole `if let` at the call site passed every other test here.
        assert_eq!(c.suspected_schema_breaks(), 1, "the row came back gutted and nothing recorded it");
    }

    /// The recency signal `/health` actually reads, through the REAL client. The handler test uses a
    /// fake that overrides `last_schema_break_age` directly, so it never touches this — deleting the
    /// `store` line, or reversing the subtraction, passed the whole suite. That is exactly what the
    /// counter's own doc comment says this kind of field exists to prevent.
    #[tokio::test]
    async fn a_gutted_row_records_when_it_happened() {
        let base = serve_once(chart_body(10, 8, "renamed")).await;
        let c = JustWatchClient::with_endpoint(base);
        assert_eq!(c.last_schema_break_age(), None, "a break was recorded before anything ran");

        let _ = c.popular("nfx", ObjectType::Movie, "US", "POPULAR").await.expect("row should not fail");
        let age = c.last_schema_break_age().expect("the break was not recorded at all");
        // Just happened: small and forward-going. A reversed subtraction saturates to zero, so the
        // upper bound is what catches it — as would a stored value that never moves.
        assert!(age < Duration::from_secs(5), "the recorded age is {age:?}, not 'just now'");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let later = c.last_schema_break_age().expect("the record vanished");
        assert!(later > age, "the age does not advance: {age:?} then {later:?}");
    }

    /// ...and a healthy row records nothing, so the age stays `None` rather than "a break, long ago".
    #[tokio::test]
    async fn a_healthy_row_records_no_break_time() {
        let base = serve_once(chart_body(10, 1, "no_title")).await;
        let c = JustWatchClient::with_endpoint(base);
        let _ = c.popular("nfx", ObjectType::Movie, "US", "POPULAR").await.expect("row should not fail");
        assert_eq!(c.last_schema_break_age(), None, "a normal row was recorded as a schema break");
    }

    /// The counterpart: a healthy row must not register a break. Without this, a call site that
    /// counted unconditionally would satisfy the test above.
    #[tokio::test]
    async fn a_healthy_row_records_no_schema_break() {
        let base = serve_once(chart_body(10, 1, "no_title")).await;
        let c = JustWatchClient::with_endpoint(base);
        let items = c.popular("nfx", ObjectType::Movie, "US", "POPULAR").await.expect("row should not fail");
        assert_eq!(items.len(), 9);
        assert_eq!(c.suspected_schema_breaks(), 0, "a normal row was reported as a schema break");
    }

    /// End-to-end, against a real 200 carrying GraphQL errors: `popular` must report failure, not an
    /// empty chart. Asserting `graphql_error` alone proves nothing — the call site can drop the
    /// check entirely and the helper still passes its own test.
    #[tokio::test]
    async fn a_200_carrying_graphql_errors_fails_the_fetch() {
        let base = serve_once(
            r#"{"errors":[{"message":"locale by the country code: couldn't get locale with country code \"ZZ\""}],"data":null}"#,
        )
        .await;
        let c = JustWatchClient::with_endpoint(base);
        let r = c.popular("nfx", ObjectType::Movie, "ZZ", "TRENDING").await;
        assert!(
            r.is_err(),
            "a GraphQL error was reported as an empty row, which then overwrites the last-good rows"
        );
    }

    /// ...and a genuinely empty chart still succeeds, or every quiet country would serve stale.
    #[tokio::test]
    async fn a_genuinely_empty_chart_is_still_an_answer() {
        let base = serve_once(r#"{"data":{"popularTitles":{"edges":[]}}}"#).await;
        let c = JustWatchClient::with_endpoint(base);
        assert!(c.popular("nfx", ObjectType::Movie, "US", "TRENDING").await.is_ok());
    }
}
