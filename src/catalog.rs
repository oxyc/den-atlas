//! Catalog resource (docs/CATALOG-justwatch.md): per-provider "Popular on <service>" rows plus the
//! aggregated "Trending Everywhere" row. Public and tokenless — additive to the dataset addon, and
//! isolated: a JustWatch failure degrades to empty rows and never touches the dataset paths.

use crate::cache::{Lookup, TtlCache};
use crate::justwatch::{ObjectType, TrendingItem, TrendingSource};
use std::time::Duration;

pub struct Provider {
    pub code: &'static str, // JustWatch package short-code
    pub id: &'static str,   // Stremio catalog id
    pub name: &'static str, // row title the client renders
}

// Short codes can drift per country; if a row comes back empty, verify against JustWatch's provider
// list for that country. Adding a service = one row here.
const PROVIDERS: &[Provider] = &[
    Provider { code: "nfx", id: "jw-nfx", name: "Popular on Netflix" },
    Provider { code: "max", id: "jw-max", name: "Popular on Max" },
    Provider { code: "amp", id: "jw-amp", name: "Popular on Prime Video" },
    Provider { code: "dnp", id: "jw-dnp", name: "Popular on Disney+" },
    Provider { code: "atp", id: "jw-atp", name: "Popular on Apple TV+" },
];

pub const TRENDING_ID: &str = "jw-trending";
pub const TRENDING_NAME: &str = "Trending Everywhere";
const STREMIO_TYPES: [&str; 2] = ["movie", "series"];

/// The providers actually served — the full table, or a subset selected by `JW_PROVIDERS` (codes).
pub fn selected_providers() -> Vec<&'static Provider> {
    match std::env::var("JW_PROVIDERS") {
        Ok(v) if !v.trim().is_empty() => {
            let codes: Vec<String> = v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
            PROVIDERS.iter().filter(|p| codes.iter().any(|c| c == p.code)).collect()
        }
        _ => PROVIDERS.iter().collect(),
    }
}

pub struct CatalogEntry {
    pub type_: &'static str,
    pub id: String,
    pub name: String,
}

/// The manifest `catalogs[]` — one per provider × type, plus "Trending Everywhere" × type.
pub fn catalog_entries() -> Vec<CatalogEntry> {
    let providers = selected_providers();
    let mut out = Vec::with_capacity((providers.len() + 1) * STREMIO_TYPES.len());
    for t in STREMIO_TYPES {
        for p in &providers {
            out.push(CatalogEntry { type_: t, id: p.id.to_owned(), name: p.name.to_owned() });
        }
        out.push(CatalogEntry { type_: t, id: TRENDING_ID.to_owned(), name: TRENDING_NAME.to_owned() });
    }
    out
}

pub struct CatalogState {
    source: Box<dyn TrendingSource>,
    cache: TtlCache,
    country: String,
}

impl CatalogState {
    pub fn new(source: Box<dyn TrendingSource>, ttl: Duration, country: String) -> Self {
        Self { source, cache: TtlCache::new(ttl), country }
    }

    /// `{ "metas": [...] }` for a catalog id + Stremio type. `None` (→ 404) for an unknown id/type.
    /// Always valid JSON otherwise; on a source failure it serves the last-good rows, or an empty list.
    pub async fn metas_json(&self, catalog_id: &str, stremio_type: &str) -> Option<String> {
        let obj = ObjectType::from_stremio(stremio_type)?;
        let is_trending = catalog_id == TRENDING_ID;
        let code = selected_providers().into_iter().find(|p| p.id == catalog_id).map(|p| p.code);
        if !is_trending && code.is_none() {
            return None; // unknown catalog id
        }

        let key = format!("jw:{}:{}:{}", self.country, catalog_id, stremio_type);
        if let Lookup::Fresh(v) = self.cache.get(&key) {
            return Some(v);
        }

        let fetched = if is_trending {
            self.aggregate(obj).await
        } else {
            self.source.popular(code.unwrap(), obj).await.ok()
        };

        match fetched {
            Some(items) => {
                let body = render_metas(&items, stremio_type);
                self.cache.put(&key, body.clone());
                Some(body)
            }
            // Refresh failed → serve stale if we have it, else an empty list (graceful degradation).
            None => Some(match self.cache.get(&key) {
                Lookup::Fresh(v) | Lookup::Stale(v) => v,
                Lookup::Miss => render_metas(&[], stremio_type),
            }),
        }
    }

    /// "Trending Everywhere": union across providers, re-ranked by inverse-rank-sum. `None` only when
    /// every provider fetch failed (so the caller can serve stale/empty).
    async fn aggregate(&self, obj: ObjectType) -> Option<Vec<TrendingItem>> {
        let mut lists = Vec::new();
        for p in selected_providers() {
            if let Ok(items) = self.source.popular(p.code, obj).await {
                lists.push(items);
            }
        }
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
            TrendingItem { imdb: r.imdb.clone(), title: r.title.clone(), rank: i }
        })
        .collect()
}

/// Render items as Stremio catalog metas. Poster is metahub-by-IMDb (Cinemeta's own source), so the
/// grid is consistent and no TMDB is needed; Cinemeta fills the detail page from the tt id.
pub fn render_metas(items: &[TrendingItem], stremio_type: &str) -> String {
    let metas: Vec<serde_json::Value> = items
        .iter()
        .map(|it| {
            serde_json::json!({
                "id": it.imdb,
                "type": stremio_type,
                "name": it.title,
                "poster": format!("https://images.metahub.space/poster/medium/{}/img", it.imdb),
            })
        })
        .collect();
    serde_json::json!({ "metas": metas }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn item(imdb: &str, title: &str, rank: usize) -> TrendingItem {
        TrendingItem { imdb: imdb.into(), title: title.into(), rank }
    }

    struct Fake {
        data: Vec<TrendingItem>,
        calls: AtomicUsize,
        fail_after: usize, // Ok for the first `fail_after` calls, then Err
    }
    #[async_trait]
    impl TrendingSource for Fake {
        async fn popular(&self, _p: &str, _o: ObjectType) -> Result<Vec<TrendingItem>, ()> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n >= self.fail_after {
                Err(())
            } else {
                Ok(self.data.clone())
            }
        }
    }

    fn state(data: Vec<TrendingItem>, ttl: Duration, fail_after: usize) -> CatalogState {
        CatalogState::new(
            Box::new(Fake { data, calls: AtomicUsize::new(0), fail_after }),
            ttl,
            "US".to_owned(),
        )
    }

    #[tokio::test]
    async fn renders_provider_row() {
        let s = state(vec![item("tt1", "A", 0)], Duration::from_secs(3600), usize::MAX);
        let body = s.metas_json("jw-nfx", "movie").await.unwrap();
        assert!(body.contains(r#""id":"tt1""#));
        assert!(body.contains(r#""type":"movie""#));
        assert!(body.contains("images.metahub.space/poster/medium/tt1/img"));
    }

    #[tokio::test]
    async fn unknown_id_or_type_is_none() {
        let s = state(vec![], Duration::from_secs(3600), usize::MAX);
        assert!(s.metas_json("bogus", "movie").await.is_none());
        assert!(s.metas_json("jw-nfx", "audiobook").await.is_none());
    }

    #[tokio::test]
    async fn source_failure_degrades_to_empty() {
        let s = state(vec![item("tt1", "A", 0)], Duration::from_secs(3600), 0); // fail immediately
        let body = s.metas_json("jw-nfx", "movie").await.unwrap();
        assert_eq!(body, r#"{"metas":[]}"#);
    }

    #[tokio::test]
    async fn serves_stale_on_refresh_error() {
        // ttl=0 → the first put is immediately stale, so the 2nd call refreshes and (source now failing)
        // must return the stale last-good rows rather than going empty.
        let s = state(vec![item("tt1", "A", 0)], Duration::ZERO, 1); // Ok once, then Err
        let first = s.metas_json("jw-nfx", "movie").await.unwrap();
        assert!(first.contains(r#""id":"tt1""#));
        let second = s.metas_json("jw-nfx", "movie").await.unwrap();
        assert_eq!(second, first, "stale value served when refresh fails");
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
        let entries = catalog_entries();
        assert!(entries.iter().any(|e| e.id == "jw-nfx" && e.type_ == "movie" && e.name == "Popular on Netflix"));
        assert!(entries.iter().any(|e| e.id == TRENDING_ID && e.type_ == "series"));
        // per type: N providers + 1 trending
        let per_type = selected_providers().len() + 1;
        assert_eq!(entries.len(), per_type * 2);
    }
}
