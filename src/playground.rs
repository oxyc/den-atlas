//! The tuning playground (oxyc/den#116): More Like This ranked by the SERVING scorer with any of its knobs
//! overridden, each title shown with the signals that placed it.
//!
//! Off unless the operator sets `PLAYGROUND`, and then only beside `INDEX_QUERIES`. Off, every
//! `/playground…` path is a 404 like any unknown route. The overrides exist ONLY on these routes:
//! `/index/similar` never reads them, flag or no flag, so turning the playground on cannot change what a
//! client is served, and a tuned row is never memoised or cached where a production row would be.
//!
//! Every answer here is computed per request, so an enabled playground is a cost anyone who can reach
//! atlas can spend. Enable it where that is acceptable.

use crate::queries::Indexes;
use den_index::{MediaType, Scored, SimilarParams};
use serde_json::{json, Value};

/// The page, embedded like /configure. Static and dependency-free: it builds its controls from
/// `/playground/params.json`, so a knob added to `SimilarParams` appears without touching it.
pub const PAGE: &str = include_str!("playground.html");

/// Titles shown when the request names no `limit`: the rail's first screenful.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 1000;

/// `GET /playground/params.json` — every knob, its range, and production's value for it.
pub fn params_json() -> String {
    let production = SimilarParams::default();
    let knobs: Vec<Value> = SimilarParams::KNOBS
        .iter()
        .map(|k| {
            json!({
                "name": k.name,
                "min": k.min,
                "max": k.max,
                "integer": k.integer,
                "about": k.about,
                "default": production.get(k.name),
            })
        })
        .collect();
    json!({ "knobs": knobs, "defaultLimit": DEFAULT_LIMIT, "maxLimit": MAX_LIMIT }).to_string()
}

/// The query string as parameters and a count. Every key must be `limit` or a knob: a misspelt knob is a
/// 400 naming it, never a silent production answer that looks tuned.
pub fn parse(query: &str) -> Result<(SimilarParams, usize), String> {
    let mut params = SimilarParams::default();
    let mut limit = DEFAULT_LIMIT;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "limit" {
            limit = value.parse().ok().filter(|n| (1..=MAX_LIMIT).contains(n)).ok_or_else(|| {
                format!("limit must be a whole number from 1 to {MAX_LIMIT}, got {value:?}")
            })?;
            continue;
        }
        if key == "watched" {
            return Err("watched: More Like This takes no viewing history yet".to_owned());
        }
        if SimilarParams::KNOBS.iter().all(|k| k.name != key) {
            return Err(format!("unknown parameter {key}"));
        }
        let value: f64 = value.parse().map_err(|_| format!("{key} must be a number, got {value:?}"))?;
        params.set(key, value)?;
    }
    Ok((params, limit))
}

/// One seed's tuned row, with production's rank beside each title and the production titles the tuning
/// pushed out of the shown count.
pub fn answer(
    indexes: &Indexes,
    media_type: MediaType,
    tmdb_id: u32,
    params: &SimilarParams,
    limit: usize,
) -> Value {
    let production = indexes.more_like_this(tmdb_id, media_type);
    let row = indexes.more_like_this_scored(tmdb_id, media_type, params);
    let card = |id: u32| {
        let card = indexes.cards.as_ref().and_then(|cards| cards.get(&(media_type, id)));
        (card.map(|c| c.title.clone()), card.and_then(|c| c.year))
    };
    let shown: Vec<&Scored> = row.iter().take(limit).collect();
    let titles: Vec<Value> = shown
        .iter()
        .enumerate()
        .map(|(at, s)| {
            let (title, year) = card(s.tmdb_id);
            json!({
                "rank": at + 1,
                "id": s.tmdb_id,
                "title": title,
                "year": year,
                "production": production.iter().position(|&id| id == s.tmdb_id).map(|p| p + 1),
                "score": s.score,
                "base": s.base,
                // Not `premise`/`plot`: the serving guard (`tos.rs`) refuses those keys as prose fields.
                "premiseCosine": s.premise,
                "plotCosine": s.plot,
                "subgenre": s.subgenre,
                "held": s.held,
                "signals": signals(s, params),
            })
        })
        .collect();
    let left: Vec<Value> = production
        .iter()
        .take(limit)
        .enumerate()
        .filter(|(_, id)| !shown.iter().any(|s| s.tmdb_id == **id))
        .map(|(at, &id)| {
            let (title, year) = card(id);
            json!({ "production": at + 1, "id": id, "title": title, "year": year })
        })
        .collect();
    let defaults = SimilarParams::default();
    let changed: Vec<&str> = SimilarParams::KNOBS
        .iter()
        .map(|k| k.name)
        .filter(|name| params.get(name) != defaults.get(name))
        .collect();
    let (title, year) = card(tmdb_id);
    json!({
        "seed": { "id": tmdb_id, "title": title, "year": year },
        "changed": changed,
        "total": row.len(),
        "productionTotal": production.len(),
        "spread": row.first().map(|s| s.spread),
        "titles": titles,
        "left": left,
    })
}

/// Each signal's raw value and the points it added to the score: `spread × weight × value`, negated for
/// `world`, which is a penalty.
fn signals(s: &Scored, p: &SimilarParams) -> Value {
    let term = |value: f64, weight: f64| json!({ "value": value, "points": s.spread * weight * value });
    json!({
        "maker": term(s.maker, p.w_maker),
        "home": term(s.home, p.w_home),
        "facet": term(s.facet, p.w_facet),
        "noul": term(s.noul, p.w_noul),
        "critique": term(s.critique, p.w_critique),
        "coverage": term(s.coverage, p.w_coverage),
        "tone": term(s.tone, p.w_tone),
        "world": term(s.world, -p.w_world),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_is_production() {
        let (params, limit) = parse("").unwrap();
        assert_eq!(params, SimilarParams::default());
        assert_eq!(limit, DEFAULT_LIMIT);
    }

    #[test]
    fn overrides_and_count_are_read_and_bad_ones_are_named() {
        let (params, limit) = parse("w_maker=0.5&pool_k=100&limit=50").unwrap();
        assert_eq!((params.w_maker, params.pool_k, limit), (0.5, 100, 50));
        for (query, names) in [
            ("w_maker=abc", "w_maker"),
            ("w_maker=99", "w_maker"),
            ("w_makr=1", "w_makr"),
            ("limit=0", "limit"),
            ("limit=5000", "limit"),
            ("watched=1438", "watched"),
        ] {
            let err = parse(query).unwrap_err();
            assert!(err.contains(names), "{query}: {err}");
        }
    }

    /// The byte-identity proof: More Like This ranked with `SimilarParams::default()` — through the
    /// serving entry point and through the playground's — against the rows the scorer served for five
    /// anchors BEFORE it took parameters (`similar-golden.json`, captured from den-atlas 0.53.0 over HTTP
    /// with `limit=200`, i.e. the whole row).
    ///
    /// Opt-in, like every test that needs the real corpus: `DEN_STORE` names a store whose directory holds
    /// its `dataset.meta.json`. It skips unless that store is the generation the golden was captured on,
    /// because a different corpus ranks differently by design.
    #[test]
    fn default_parameters_rank_exactly_as_the_scorer_did_before_it_took_any() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to exercise this");
            return;
        };
        let golden: Value = serde_json::from_str(include_str!("similar-golden.json")).unwrap();
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        if golden["datasetVersion"] != ds.meta.dataset_version.as_str() {
            eprintln!(
                "SKIP: the golden is for datasetVersion {}, this store is {}",
                golden["datasetVersion"], ds.meta.dataset_version
            );
            return;
        }
        let indexes = crate::queries::load_for_tools(&ds).expect("the indexes load");
        let anchors = golden["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 5);
        for anchor in anchors {
            let media = if anchor["type"] == "movie" { MediaType::Movie } else { MediaType::Tv };
            let id = anchor["id"].as_u64().unwrap() as u32;
            let want: Vec<u32> =
                anchor["ids"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
            assert!(!want.is_empty(), "{}", anchor["name"]);
            assert_eq!(&*indexes.more_like_this(id, media), want.as_slice(), "{} (serving)", anchor["name"]);
            let tuned: Vec<u32> = indexes
                .more_like_this_scored(id, media, &SimilarParams::default())
                .iter()
                .map(|s| s.tmdb_id)
                .collect();
            assert_eq!(tuned, want, "{} (playground, default parameters)", anchor["name"]);
        }
    }
}
