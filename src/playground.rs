//! The tuning playground (oxyc/den#116): More Like This ranked by the SERVING scorer with any of its knobs
//! overridden, each title shown with the signals that placed it.
//!
//! Off unless the operator sets `PLAYGROUND`, and then only beside `INDEX_QUERIES`. Off, every
//! `/playground…` path is a 404 like any unknown route. The overrides exist ONLY on these routes:
//! `/index/similar` never reads them, flag or no flag, so turning the playground on cannot change what a
//! client is served, and a tuned row is never memoised or cached where a production row would be.
//!
//! Every answer here is computed per request, so an enabled playground is a cost anyone who can reach
//! atlas can spend. The knob ranges (`SimilarParams::KNOBS`) and `MAX_LIMIT` bound what one request can
//! cost; how many requests an address may make is the relay's to limit.

use crate::queries::Indexes;
use den_index::{MediaType, Scored, SimilarParams};
use serde_json::{json, Value};

/// The page, embedded like /configure. Static and dependency-free: it builds its controls from
/// `/playground/params.json`, so a knob added to `SimilarParams` appears without touching it.
pub const PAGE: &str = include_str!("playground.html");

/// Titles shown when the request names no `limit`: the rail's first screenful.
const DEFAULT_LIMIT: usize = 20;
/// Production's whole row (`den_index::MAX_ROW`): every title shown carries its signals, so the count
/// shown is what a response's size scales with.
const MAX_LIMIT: usize = 200;

/// The seeds the page opens with when its URL names none: a spread of kinds and sizes of catalogue, each
/// a row someone has looked at closely.
pub const DEFAULT_SEEDS: &[(MediaType, u32)] = &[
    (MediaType::Tv, 1399),      // Game of Thrones
    (MediaType::Tv, 1438),      // The Wire
    (MediaType::Movie, 5723),   // Once
    (MediaType::Movie, 614945), // Voicemails for Isabelle
    (MediaType::Tv, 38148),     // Beck (1997)
    (MediaType::Movie, 278),    // The Shawshank Redemption
];

/// Seeds one `/playground/rows.json` request may rank. Each is a whole tuned row, so this times the
/// clamped cost of one row bounds the request.
pub const MAX_SEEDS: usize = 12;

/// Titles shown per seed in one `/playground/rows.json` answer. Lower than `MAX_LIMIT` because the answer
/// is up to twelve rows, and every title shown carries its signals: at 200 per seed the worst request was
/// 1.76 MB.
pub const MAX_ROWS_LIMIT: usize = 50;

/// `limit` checked against `MAX_ROWS_LIMIT`, after `parse` has held it to `MAX_LIMIT`.
pub fn rows_limit(limit: usize) -> Result<usize, String> {
    if limit > MAX_ROWS_LIMIT {
        return Err(format!("limit must be at most {MAX_ROWS_LIMIT} per seed here, got {limit}"));
    }
    Ok(limit)
}

/// `movie:5723` / `series:1438`: a seed as the page's URL and `rows.json` name it, in the type names
/// atlas's routes use.
pub fn seed_key(media: MediaType, id: u32) -> String {
    match media {
        MediaType::Movie => format!("movie:{id}"),
        MediaType::Tv => format!("series:{id}"),
    }
}

/// The `seeds` value of a query string, percent-decoded, and the query without it — so the rest goes
/// through `parse`, which refuses any key it does not know.
pub fn take_seeds(query: &str) -> (Option<String>, String) {
    let mut seeds = None;
    let mut rest: Vec<&str> = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("seeds", value) => seeds = Some(crate::handler::percent_decode(value)),
            _ => rest.push(pair),
        }
    }
    (seeds, rest.join("&"))
}

/// A seed list: absent is `DEFAULT_SEEDS`, empty is none, otherwise comma-separated `seed_key`s. A seed
/// named twice is ranked once, in its first place; more than `MAX_SEEDS` is refused.
pub fn parse_seeds(value: Option<&str>) -> Result<Vec<(MediaType, u32)>, String> {
    let Some(value) = value else { return Ok(DEFAULT_SEEDS.to_vec()) };
    let mut seeds: Vec<(MediaType, u32)> = Vec::new();
    for key in value.split(',').filter(|key| !key.is_empty()) {
        let seed = key
            .split_once(':')
            .and_then(|(kind, id)| {
                let media = match kind {
                    "movie" => MediaType::Movie,
                    "series" => MediaType::Tv,
                    _ => return None,
                };
                Some((media, id.parse().ok()?))
            })
            .ok_or_else(|| format!("seeds: {key:?} is not movie:<tmdbId> or series:<tmdbId>"))?;
        if !seeds.contains(&seed) {
            seeds.push(seed);
        }
    }
    if seeds.len() > MAX_SEEDS {
        return Err(format!("seeds: at most {MAX_SEEDS}, got {}", seeds.len()));
    }
    Ok(seeds)
}

/// `GET /playground/rows.json?seeds=…&<knob>=…&limit=` — every seed's tuned row in one answer, each the
/// shape `/playground/similar` gives one seed, with its `key`. One request per change of the page's
/// knobs, however many seeds it shows.
pub fn rows(indexes: &Indexes, seeds: &[(MediaType, u32)], params: &SimilarParams, limit: usize) -> Value {
    let rows: Vec<Value> = seeds
        .iter()
        .map(|&(media, id)| {
            let mut row = answer(indexes, media, id, params, limit);
            row["key"] = json!(seed_key(media, id));
            row
        })
        .collect();
    json!({ "changed": changed(params), "rows": rows })
}

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
    let default_seeds: Vec<String> = DEFAULT_SEEDS.iter().map(|&(media, id)| seed_key(media, id)).collect();
    json!({
        "knobs": knobs,
        "defaultLimit": DEFAULT_LIMIT,
        "maxLimit": MAX_LIMIT,
        "defaultSeeds": default_seeds,
        "maxSeeds": MAX_SEEDS,
        "maxRowsLimit": MAX_ROWS_LIMIT,
    })
    .to_string()
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
    let (title, year) = card(tmdb_id);
    json!({
        "seed": { "id": tmdb_id, "title": title, "year": year },
        "changed": changed(params),
        "total": row.len(),
        "productionTotal": production.len(),
        "spread": row.first().map(|s| s.spread),
        "titles": titles,
        "left": left,
    })
}

/// The knobs moved off production's value, by name.
fn changed(params: &SimilarParams) -> Vec<&'static str> {
    let defaults = SimilarParams::default();
    SimilarParams::KNOBS
        .iter()
        .map(|k| k.name)
        .filter(|name| params.get(name) != defaults.get(name))
        .collect()
}

/// `GET /playground/judged.json?<knob>=…` — the knobs scored against the hand-judged set
/// (`judged/rail.json`), each half's mean beside production's, and every case.
///
/// The halves are kept apart because they have different jobs (`raileval.rs`): tune on dev; read test
/// once, to confirm. A number tuned live against all the cases would be fitted to the half meant to check
/// it. The page leads with dev and keeps test folded away.
///
/// Costs one tuned row per case — 46 rows — when a knob is moved; production's rows are memoised.
pub fn judged(indexes: &Indexes, params: &SimilarParams) -> Value {
    use den_index::eval::{mean, score, Scores};
    let tuned = *params != SimilarParams::default();
    let as_json = |s: &Scores| {
        json!({
            "ndcg": s.ndcg,
            "condensed": s.condensed,
            "precision": s.precision,
            "bad": s.bad,
            "judged": s.judged,
        })
    };
    let mut halves: Vec<(&str, Vec<Scores>, Vec<Scores>)> =
        vec![("dev", vec![], vec![]), ("test", vec![], vec![])];
    let mut cases = Vec::new();
    for c in crate::raileval::embedded() {
        let production = score(&indexes.more_like_this(c.id, c.media), &c.grades, crate::raileval::K);
        let mine = if tuned {
            let row: Vec<u32> =
                indexes.more_like_this_scored(c.id, c.media, params).iter().map(|s| s.tmdb_id).collect();
            score(&row, &c.grades, crate::raileval::K)
        } else {
            production
        };
        if let Some(half) = halves.iter_mut().find(|h| h.0 == c.case.split) {
            half.1.push(mine);
            half.2.push(production);
        }
        cases.push(json!({
            "seed": c.case.seed,
            "title": c.case.title,
            "split": c.case.split,
            "tuned": as_json(&mine),
            "production": as_json(&production),
        }));
    }
    let mut out = json!({ "k": crate::raileval::K, "changed": changed(params), "cases": cases });
    for (half, mine, production) in &halves {
        out[*half] = json!({
            "n": mine.len(),
            "tuned": as_json(&mean(mine)),
            "production": as_json(&mean(production)),
        });
    }
    out
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

    /// No `seeds` in the URL is the default set; an empty one is no seeds at all — the page's "clear".
    #[test]
    fn an_absent_seed_list_is_the_default_set_and_an_empty_one_is_none() {
        assert_eq!(parse_seeds(None).unwrap(), DEFAULT_SEEDS);
        assert_eq!(DEFAULT_SEEDS.len(), 6);
        assert!(DEFAULT_SEEDS.contains(&(MediaType::Tv, 38148)), "Beck, the 1997 series");
        assert_eq!(parse_seeds(Some("")).unwrap(), []);
        let (seeds, rest) = take_seeds("w_maker=1&limit=5");
        assert_eq!((seeds, rest.as_str()), (None, "w_maker=1&limit=5"));

        let p: Value = serde_json::from_str(&params_json()).unwrap();
        let keys: Vec<String> = DEFAULT_SEEDS.iter().map(|&(m, id)| seed_key(m, id)).collect();
        assert_eq!(p["defaultSeeds"], json!(keys), "the page is told the same defaults");
        assert_eq!(p["maxSeeds"], MAX_SEEDS);
    }

    /// The page writes its seeds into its URL as `seed_key`s, and reads them back as the same list — also
    /// when the browser has percent-encoded the separators.
    #[test]
    fn a_seed_list_round_trips_through_the_url() {
        let seeds = vec![(MediaType::Movie, 5723), (MediaType::Tv, 1438), (MediaType::Movie, 278)];
        let value = seeds.iter().map(|&(m, id)| seed_key(m, id)).collect::<Vec<_>>().join(",");
        assert_eq!(value, "movie:5723,series:1438,movie:278");
        let query = format!("w_maker=0.5&seeds={value}&limit=10");
        let (read, rest) = take_seeds(&query);
        assert_eq!(parse_seeds(read.as_deref()).unwrap(), seeds);
        assert_eq!(rest, "w_maker=0.5&limit=10", "the rest still goes through `parse`");
        let (encoded, _) = take_seeds("seeds=movie%3A5723%2Cseries%3A1438%2Cmovie%3A278");
        assert_eq!(parse_seeds(encoded.as_deref()).unwrap(), seeds);
    }

    #[test]
    fn a_bad_or_oversized_seed_list_is_refused_and_a_repeat_ranked_once() {
        for bad in ["tv:1399", "movie:x", "movie", "movie:1,,series:"] {
            assert!(parse_seeds(Some(bad)).unwrap_err().contains("seeds"), "{bad}");
        }
        assert_eq!(parse_seeds(Some("movie:1,movie:1,series:1")).unwrap().len(), 2);
        let many = (1..=MAX_SEEDS as u32 + 1).map(|id| format!("movie:{id}")).collect::<Vec<_>>().join(",");
        assert!(parse_seeds(Some(&many)).unwrap_err().contains("at most"));
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
            ("limit=201", "limit"),
            ("pool_k=1001", "pool_k"),
            ("max_row=401", "max_row"),
            ("watched=1438", "watched"),
        ] {
            let err = parse(query).unwrap_err();
            assert!(err.contains(names), "{query}: {err}");
        }
    }

    /// The byte-identity proof: More Like This ranked with `SimilarParams::default()` — through the
    /// serving entry point and through the playground's — against the rows the scorer served for five
    /// anchors BEFORE it took parameters (`similar-golden.json`: the whole row, `limit=200`, first captured
    /// from den-atlas 0.53.0 over HTTP). `scripts/similar-golden.py` recaptures it; it was recaptured when
    /// the scorer's `ln` moved to `libm`, which moved scores by ULPs and changed no id.
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
