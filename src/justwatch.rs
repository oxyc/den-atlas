//! Isolated JustWatch source (docs/CATALOG-justwatch.md). Fetches "most popular" (TRENDING) titles for
//! a streaming provider via the unofficial GraphQL API and maps them to IMDb-keyed items. Nothing here
//! can affect the dataset resource: every failure returns `Err(())`, which the handler turns into empty
//! rows. User input never reaches this module — provider codes come from a fixed table.

use async_trait::async_trait;
use serde::Deserialize;
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
}

/// The source seam — the real JustWatch client in prod, a fake in tests. `country` is per-request (an
/// ISO-3166 code) so one client serves every configured/forwarded region.
#[async_trait]
pub trait TrendingSource: Send + Sync {
    async fn popular(&self, provider: &str, obj: ObjectType, country: &str) -> Result<Vec<TrendingItem>, ()>;
}

const ENDPOINT: &str = "https://apis.justwatch.com/graphql";
const MAX_BODY: usize = 4 << 20; // cap the response body (a hostile/huge reply must not OOM us)

// Mirrors rleroi/Stremio-Streaming-Catalogs-Addon's GetPopularTitles (only the fields we use).
const QUERY: &str = r#"query GetPopularTitles($country: Country!, $first: Int!, $popularTitlesSortBy: PopularTitlesSorting!, $packages: [String!], $objectTypes: [ObjectType!]) {
  popularTitles(country: $country, first: $first, sortBy: $popularTitlesSortBy, filter: { objectTypes: $objectTypes, packages: $packages }) {
    edges { node { content(country: $country, language: "en") { title externalIds { imdbId tmdbId } scoring { imdbScore } } } }
  }
}"#;

pub struct JustWatchClient {
    // None if the client couldn't be built (TLS backend init) — catalog then degrades to empty rows
    // instead of `reqwest::Client::new()` panicking at startup.
    http: Option<reqwest::Client>,
}

impl JustWatchClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("den-atlas/0.1 (+https://github.com/oxyc/den)")
            .build()
            .map_err(|e| eprintln!("den-atlas: reqwest client build failed ({e}); catalog disabled"))
            .ok();
        Self { http }
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
    content: Content,
}
#[derive(Deserialize)]
struct Content {
    title: String,
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

/// Parse a GraphQL response body into ranked items, dropping anything without a valid IMDb id (it
/// couldn't be resolved by Cinemeta/other addons). Never panics; a malformed body → empty list.
pub fn parse_popular(body: &str) -> Vec<TrendingItem> {
    let resp: GqlResp = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let edges = resp.data.and_then(|d| d.popular).map(|p| p.edges).unwrap_or_default();
    let mut out = Vec::with_capacity(edges.len());
    for e in edges {
        let ext = e.node.content.external_ids;
        let imdb = match ext.as_ref().and_then(|x| x.imdb_id.clone()) {
            Some(id) if is_imdb(&id) => id,
            _ => continue,
        };
        let moviedb = ext.and_then(|x| x.tmdb_id).and_then(|s| s.parse::<i64>().ok());
        let rating = e.node.content.scoring.and_then(|s| s.imdb_score);
        let rank = out.len();
        out.push(TrendingItem { imdb, moviedb, title: e.node.content.title, rank, rating });
    }
    out
}

fn is_imdb(s: &str) -> bool {
    s.len() >= 3 && s.starts_with("tt") && s[2..].bytes().all(|b| b.is_ascii_digit())
}

#[async_trait]
impl TrendingSource for JustWatchClient {
    async fn popular(&self, provider: &str, obj: ObjectType, country: &str) -> Result<Vec<TrendingItem>, ()> {
        let payload = serde_json::json!({
            "query": QUERY,
            "variables": {
                "country": country,
                "first": 100,
                "popularTitlesSortBy": "TRENDING",
                "packages": [provider],
                "objectTypes": [obj.as_jw()],
            },
        });
        let http = match self.http.as_ref() {
            Some(h) => h,
            None => return Err(()),
        };
        let mut resp = match http.post(ENDPOINT).json(&payload).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("den-atlas: justwatch request failed ({provider}/{}): {e}", obj.as_jw());
                return Err(());
            }
        };
        if !resp.status().is_success() {
            eprintln!("den-atlas: justwatch http {} ({provider}/{})", resp.status(), obj.as_jw());
            return Err(());
        }
        // Bound the body as it streams — reqwest has no default size limit, so `.bytes()` would buffer a
        // hostile/huge reply in full before any check. Bail once we exceed the cap.
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| ())? {
            if buf.len() + chunk.len() > MAX_BODY {
                eprintln!("den-atlas: justwatch body exceeded {MAX_BODY} bytes ({provider}) — dropping");
                return Err(());
            }
            buf.extend_from_slice(&chunk);
        }
        let items = parse_popular(&String::from_utf8_lossy(&buf));
        // A non-empty body that yields zero usable items is the signal of a breaking GraphQL schema change
        // (it would otherwise silently serve empty rows forever).
        if items.is_empty() && !buf.is_empty() {
            eprintln!("den-atlas: justwatch returned a non-empty body but 0 usable items ({provider}) — possible schema change");
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"data":{"popularTitles":{"edges":[
      {"node":{"content":{"title":"Alpha","externalIds":{"imdbId":"tt0000001","tmdbId":"1397385"},"scoring":{"imdbScore":7.4}}}},
      {"node":{"content":{"title":"NoId","externalIds":{"imdbId":null}}}},
      {"node":{"content":{"title":"Bad","externalIds":{"imdbId":"nope"}}}},
      {"node":{"content":{"title":"Beta","externalIds":{"imdbId":"tt0000002","tmdbId":null},"scoring":{"imdbScore":null}}}}
    ]}}}"#;

    #[test]
    fn parses_ranks_and_carries_tmdb_and_rating() {
        let items = parse_popular(FIXTURE);
        assert_eq!(items.len(), 2, "items without a valid tt id are dropped");
        assert_eq!(items[0], TrendingItem { imdb: "tt0000001".into(), moviedb: Some(1397385), title: "Alpha".into(), rank: 0, rating: Some(7.4) });
        assert_eq!(items[1], TrendingItem { imdb: "tt0000002".into(), moviedb: None, title: "Beta".into(), rank: 1, rating: None });
    }

    #[test]
    fn malformed_body_is_empty_not_panic() {
        assert!(parse_popular("not json").is_empty());
        assert!(parse_popular(r#"{"data":null}"#).is_empty());
    }

    #[test]
    fn imdb_validation() {
        assert!(is_imdb("tt0111161"));
        assert!(!is_imdb("tt"));
        assert!(!is_imdb("nm123"));
        assert!(!is_imdb("tt12a"));
    }
}
