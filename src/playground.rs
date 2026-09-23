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

/// The `name` value of a query string, percent-decoded, and the query without it — so the rest goes
/// through `parse`, which refuses any key it does not know.
pub fn take_param(query: &str, name: &str) -> (Option<String>, String) {
    let mut found = None;
    let mut rest: Vec<&str> = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            (key, value) if key == name => found = Some(crate::handler::percent_decode(value)),
            _ => rest.push(pair),
        }
    }
    (found, rest.join("&"))
}

/// Comma-separated `seed_key`s, a title named twice counted once in its first place, at most `max`. The
/// error names `what`.
fn parse_keys(value: &str, what: &str, max: usize) -> Result<Vec<(MediaType, u32)>, String> {
    let mut keys: Vec<(MediaType, u32)> = Vec::new();
    for key in value.split(',').filter(|key| !key.is_empty()) {
        let parsed = key
            .split_once(':')
            .and_then(|(kind, id)| {
                let media = match kind {
                    "movie" => MediaType::Movie,
                    "series" => MediaType::Tv,
                    _ => return None,
                };
                Some((media, id.parse().ok()?))
            })
            .ok_or_else(|| format!("{what}: {key:?} is not movie:<tmdbId> or series:<tmdbId>"))?;
        if !keys.contains(&parsed) {
            keys.push(parsed);
        }
    }
    if keys.len() > max {
        return Err(format!("{what}: at most {max}, got {}", keys.len()));
    }
    Ok(keys)
}

/// A seed list: absent is `DEFAULT_SEEDS`, empty is none, otherwise comma-separated `seed_key`s. A seed
/// named twice is ranked once, in its first place; more than `MAX_SEEDS` is refused.
pub fn parse_seeds(value: Option<&str>) -> Result<Vec<(MediaType, u32)>, String> {
    value.map_or_else(|| Ok(DEFAULT_SEEDS.to_vec()), |value| parse_keys(value, "seeds", MAX_SEEDS))
}

/// You Might Also Like's seeds: absent is `DEFAULT_SUGGEST_SEEDS`, empty is none.
pub fn parse_suggest_seeds(value: Option<&str>) -> Result<Vec<(MediaType, u32)>, String> {
    value.map_or_else(
        || Ok(DEFAULT_SUGGEST_SEEDS.to_vec()),
        |value| parse_keys(value, "suggest", MAX_SUGGEST_SEEDS),
    )
}

/// You Might Also Like's seeds when the page names none: a household that watched these three.
pub const DEFAULT_SUGGEST_SEEDS: &[(MediaType, u32)] = &[
    (MediaType::Tv, 1438),    // The Wire
    (MediaType::Movie, 5723), // Once
    (MediaType::Movie, 278),  // The Shawshank Redemption
];

/// You Might Also Like pools one tuned row per seed; it rides in the same request as the rows, so its seeds
/// are bounded apart from theirs.
pub const MAX_SUGGEST_SEEDS: usize = 6;

/// Titles You Might Also Like shows: `/index/suggest`'s default.
const SUGGEST_SHOWN: usize = 20;

/// What the playground ranks with beyond the store: TMDB's kept numbers and its export popularity.
pub struct Sources<'a> {
    pub indexes: &'a Indexes,
    /// TMDB's daily export, for popularity; `None` when title search is off.
    pub export: Option<&'a den_titlesearch::TitleIndex>,
}

/// `den_index::Audience`: TMDB's kept rating and count (`Indexes::rating`, empty while nothing is kept) and
/// its export popularity. Read by the filters and the popularity term only — plain filters and a sort term,
/// as `tmdb.rs` allows; no answer here carries the numbers themselves.
struct Viewers<'a> {
    sources: &'a Sources<'a>,
}

impl den_index::Audience for Viewers<'_> {
    fn rating(&self, (media, tmdb_id): (MediaType, u32)) -> Option<(f64, f64)> {
        let (votes, rating) = self.sources.indexes.rating(media, tmdb_id)?;
        Some((f64::from(rating), f64::from(votes)))
    }

    fn popularity(&self, (media, tmdb_id): (MediaType, u32)) -> Option<f64> {
        let kind = match media {
            MediaType::Movie => den_titlesearch::MediaType::Movie,
            MediaType::Tv => den_titlesearch::MediaType::Tv,
        };
        self.sources.export?.popularity_of(kind, tmdb_id)
    }
}

/// One seed's row with everything `tuning` asks for: its knobs, TMDB's numbers and popularity, its filters, and
/// — when it names watched titles — without them and in den-core's tilted order. Whether it was tilted.
///
/// At production's `Tuning` this is `Indexes::more_like_this_scored` exactly: no filter is consulted, no
/// title is dropped and nothing is reordered.
fn tuned_row(sources: &Sources<'_>, media: MediaType, tmdb_id: u32, tuning: &Tuning) -> (Vec<Scored>, bool) {
    type Key = (MediaType, u32);
    let indexes = sources.indexes;
    let viewers = Viewers { sources };
    // A facet filter over both types: a mixed row keeps the other type's titles that carry it too.
    let allowed: Option<std::collections::HashSet<Key>> = (!tuning.filters.is_empty()).then(|| {
        [MediaType::Movie, MediaType::Tv]
            .into_iter()
            .flat_map(|kind| crate::plotrows::carrying(indexes, kind, &tuning.filters))
            .map(|(key, _)| key)
            .collect()
    });
    let watched: std::collections::HashSet<Key> = tuning.watched.iter().copied().collect();
    let keep = |key: Key| !watched.contains(&key) && allowed.as_ref().is_none_or(|a| a.contains(&key));
    let filtering = allowed.is_some() || !watched.is_empty();
    let extras = den_index::Extras {
        audience: Some(&viewers),
        keep: filtering.then_some(&keep as &dyn Fn(Key) -> bool),
    };
    let row = indexes.more_like_this_with(tmdb_id, media, &tuning.params, extras);
    let Some(tilt) = watched_tilt(&tuning.watched) else { return (row, false) };
    let Some(cards) = indexes.cards.as_ref() else { return (row, false) };
    let order: Vec<Key> = row.iter().map(Scored::key).collect();
    let tilted = tilt.applied(indexes, &order, cards);
    let mut by_key: std::collections::HashMap<Key, Scored> = row.into_iter().map(|s| (s.key(), s)).collect();
    (tilted.iter().filter_map(|key| by_key.remove(key)).collect(), true)
}

/// den-core's household tilt with the watched titles as what the household liked — the `tilt.liked` a
/// browse row reads — at den-core's default weights and with no era curve. `None` for an empty list.
fn watched_tilt(watched: &[(MediaType, u32)]) -> Option<crate::plotrows::Tilt> {
    if watched.is_empty() {
        return None;
    }
    let liked: Vec<String> = watched
        .iter()
        .map(|&(media, id)| format!("{}{id}", if media == MediaType::Tv { 't' } else { 'm' }))
        .collect();
    crate::plotrows::Tilt::parse(&format!("{}liked={}", crate::plotrows::TILT_PREFIX, liked.join(",")))
}

/// `GET /playground/rows.json?seeds=…&suggest=…&<tuning>` — every seed's tuned row in one answer, each the
/// shape `/playground/similar` gives one seed, with its `key`, and You Might Also Like for the `suggest`
/// seeds. One request per change of the page's knobs, however many seeds it shows.
pub fn rows(
    sources: &Sources<'_>,
    seeds: &[(MediaType, u32)],
    suggest_seeds: &[(MediaType, u32)],
    tuning: &Tuning,
) -> Value {
    let rows: Vec<Value> = seeds
        .iter()
        .map(|&(media, id)| {
            let mut row = answer(sources, media, id, tuning);
            row["key"] = json!(seed_key(media, id));
            row
        })
        .collect();
    json!({ "changed": changed(&tuning.params), "rows": rows, "suggest": suggest(sources, suggest_seeds, tuning) })
}

/// You Might Also Like, tuned: `/index/suggest`'s pooling — each seed's row minus the seeds and the watched
/// titles, pooled in seed order — over tuned rows, with each pooled title's place in production's pool.
pub fn suggest(sources: &Sources<'_>, seeds: &[(MediaType, u32)], tuning: &Tuning) -> Value {
    let indexes = sources.indexes;
    let seeds: Vec<(u32, MediaType)> = seeds.iter().map(|&(m, id)| (id, m)).collect();
    let mut excluded: std::collections::HashSet<(u32, MediaType)> = seeds.iter().copied().collect();
    let production_pool =
        crate::handler::suggest_pool_mixed(&seeds, &excluded, SUGGEST_SHOWN, |id, media| {
            indexes.more_like_this_mixed(id, media).iter().map(|&(m, id)| (id, m)).collect()
        })
        .1;
    excluded.extend(tuning.watched.iter().map(|&(m, id)| (id, m)));
    let (per_seed, pooled) =
        crate::handler::suggest_pool_mixed(&seeds, &excluded, SUGGEST_SHOWN, |id, media| {
            tuned_row(sources, media, id, tuning).0.iter().map(|s| (s.tmdb_id, s.media_type)).collect()
        });
    let card = |media: MediaType, id: u32| {
        let card = indexes.cards.as_ref().and_then(|cards| cards.get(&(media, id)));
        (card.map(|c| c.title.clone()), card.and_then(|c| c.year))
    };
    let pooled: Vec<Value> = pooled
        .iter()
        .enumerate()
        .map(|(at, &(id, media))| {
            let (title, year) = card(media, id);
            json!({
                "rank": at + 1,
                "key": seed_key(media, id),
                "title": title,
                "year": year,
                "production": production_pool.iter().position(|&p| p == (id, media)).map(|p| p + 1),
            })
        })
        .collect();
    let seeds: Vec<Value> = per_seed
        .iter()
        .map(|&((id, media), ref ids)| {
            let (title, year) = card(media, id);
            json!({ "key": seed_key(media, id), "title": title, "year": year, "total": ids.len() })
        })
        .collect();
    json!({ "seeds": seeds, "titles": pooled })
}

/// Titles `/playground/titles.json` suggests.
const TITLE_HITS: usize = 8;

/// `GET /playground/titles.json?q=` — the page's title autocomplete: the den-titles fuzzy search (typo-tolerant,
/// popularity-weighted) over films and series at once, kept to the titles the store has, since only those can
/// have a row, with the store's title and year. One request per search, where the catalog route is one per type.
pub fn titles(indexes: &Indexes, export: &den_titlesearch::TitleIndex, query: &str) -> Value {
    let Some(cards) = indexes.cards.as_ref() else { return json!({ "titles": [] }) };
    // Deeper than what is shown, because the export holds titles the store does not.
    let titles: Vec<Value> = export
        .search(query, None, TITLE_HITS * 4)
        .iter()
        .filter_map(|hit| {
            let media = match hit.media_type {
                den_titlesearch::MediaType::Movie => MediaType::Movie,
                den_titlesearch::MediaType::Tv => MediaType::Tv,
            };
            let card = cards.get(&(media, hit.tmdb_id))?;
            Some(json!({ "key": seed_key(media, hit.tmdb_id), "title": card.title, "year": card.year }))
        })
        .take(TITLE_HITS)
        .collect();
    json!({ "titles": titles })
}

/// `GET /playground/params.json` — every knob, its range, and production's value for it.
pub fn params_json() -> String {
    let production = SimilarParams::default();
    let knobs: Vec<Value> = SimilarParams::KNOBS
        .iter()
        .map(|k| {
            json!({
                "name": k.name,
                "group": k.group,
                "min": k.min,
                "max": k.max,
                "integer": k.integer,
                "about": k.about,
                "default": production.get(k.name),
            })
        })
        .collect();
    let keys = |list: &[(MediaType, u32)]| list.iter().map(|&(m, id)| seed_key(m, id)).collect::<Vec<_>>();
    json!({
        "knobs": knobs,
        "knobGroups": den_index::KNOB_GROUPS,
        "defaultLimit": DEFAULT_LIMIT,
        "maxLimit": MAX_LIMIT,
        "defaultSeeds": keys(DEFAULT_SEEDS),
        "maxSeeds": MAX_SEEDS,
        "maxRowsLimit": MAX_ROWS_LIMIT,
        "defaultSuggestSeeds": keys(DEFAULT_SUGGEST_SEEDS),
        "maxSuggestSeeds": MAX_SUGGEST_SEEDS,
        "maxWatched": MAX_WATCHED,
        "fileFormat": FILE_FORMAT,
        "fileVersion": FILE_VERSION,
        "knobSchema": KNOB_SCHEMA,
    })
    .to_string()
}

/// The most titles a `watched` list may name: the same cap a browse row's `tilt.liked` has, for the same
/// reason — a centroid over more is not a better centroid, and the list travels in a URL.
pub const MAX_WATCHED: usize = 500;

/// The prefix a facet filter carries: `filter.tone=bleak`, `filter.mood=Feel-good`.
const FILTER_PREFIX: &str = "filter.";

/// Everything a request tunes, beyond the corpus.
#[derive(Clone, Debug, PartialEq)]
pub struct Tuning {
    pub params: SimilarParams,
    pub limit: usize,
    /// Titles already watched: dropped from every row, and each row ordered for someone who watched them —
    /// den-core's tilt, exactly as a browse row's `tilt.liked` orders it.
    pub watched: Vec<(MediaType, u32)>,
    /// Constraints every candidate must carry, as `/index/plot` reads them: a plot facet (`tone=bleak`) or a
    /// label (`mood=Feel-good`, `subgenre=Heist`). Several combine.
    pub filters: Vec<(String, String)>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            params: SimilarParams::default(),
            limit: DEFAULT_LIMIT,
            watched: Vec::new(),
            filters: Vec::new(),
        }
    }
}

/// The query string as a `Tuning`. Every key must be `limit`, `watched`, a `filter.<axis>` or a knob: a
/// misspelt knob is a 400 naming it, never a silent production answer that looks tuned.
pub fn parse(query: &str) -> Result<Tuning, String> {
    let mut tuning = Tuning::default();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "limit" {
            tuning.limit = value.parse().ok().filter(|n| (1..=MAX_LIMIT).contains(n)).ok_or_else(|| {
                format!("limit must be a whole number from 1 to {MAX_LIMIT}, got {value:?}")
            })?;
            continue;
        }
        if key == "watched" {
            tuning.watched = parse_keys(&crate::handler::percent_decode(value), "watched", MAX_WATCHED)?;
            continue;
        }
        if let Some(axis) = key.strip_prefix(FILTER_PREFIX) {
            // Form-encoded, as a browser's URLSearchParams writes it: a space is a `+`.
            let value = crate::handler::percent_decode(&value.replace('+', " "));
            if axis.is_empty() || value.is_empty() {
                return Err(format!("{key}: a filter is filter.<axis>=<value>"));
            }
            tuning.filters.push((crate::handler::percent_decode(axis), value));
            continue;
        }
        if SimilarParams::KNOBS.iter().all(|k| k.name != key) {
            return Err(format!("unknown parameter {key}"));
        }
        let value: f64 = value.parse().map_err(|_| format!("{key} must be a number, got {value:?}"))?;
        tuning.params.set(key, value)?;
    }
    Ok(tuning)
}

/// One seed's tuned row, with production's rank beside each title and the production titles the tuning
/// pushed out of the shown count.
pub fn answer(sources: &Sources<'_>, media_type: MediaType, tmdb_id: u32, tuning: &Tuning) -> Value {
    let indexes = sources.indexes;
    let (params, limit) = (&tuning.params, tuning.limit);
    let production = indexes.more_like_this_mixed(tmdb_id, media_type);
    let (row, tilted) = tuned_row(sources, media_type, tmdb_id, tuning);
    let card = |key: (MediaType, u32)| {
        let card = indexes.cards.as_ref().and_then(|cards| cards.get(&key));
        (card.map(|c| c.title.clone()), card.and_then(|c| c.year))
    };
    let shown: Vec<&Scored> = row.iter().take(limit).collect();
    let titles: Vec<Value> = shown
        .iter()
        .enumerate()
        .map(|(at, s)| {
            let (title, year) = card(s.key());
            json!({
                "rank": at + 1,
                "id": s.tmdb_id,
                "key": seed_key(s.media_type, s.tmdb_id),
                "title": title,
                "year": year,
                "production": production.iter().position(|&key| key == s.key()).map(|p| p + 1),
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
        .filter(|(_, key)| !shown.iter().any(|s| s.key() == **key))
        .map(|(at, &(media, id))| {
            let (title, year) = card((media, id));
            json!({ "production": at + 1, "id": id, "key": seed_key(media, id), "title": title, "year": year })
        })
        .collect();
    let (title, year) = card((media_type, tmdb_id));
    json!({
        "seed": { "id": tmdb_id, "title": title, "year": year },
        "changed": changed(params),
        "tilted": tilted,
        "total": row.len(),
        "productionTotal": production.len(),
        "spread": row.first().map(|s| s.spread),
        "titles": titles,
        "left": left,
    })
}

/// What `/playground/export.json` writes and `/playground/import.json` reads: a stable, versioned file that
/// Den Web's saved levers will read too. The shape, version 1:
///
/// ```text
/// {
///   "format": "den-atlas-playground",   // always this
///   "version": 1,                       // FILE_VERSION: the file's shape
///   "knobSchema": 1,                    // KNOB_SCHEMA: the knob set's meaning
///   "state": {                          // what import restores; nothing else is read
///     "knobs": { "<knob>": <number> },  // only knobs moved off production
///     "limit": 20,                      // titles per seed
///     "seeds": ["series:1438", …],      // More Like This seeds, in order
///     "suggestSeeds": ["movie:278", …], // You Might Also Like seeds
///     "watched": ["movie:5723", …],     // excluded, and the rows tilted towards them
///     "filters": [{ "axis": "tone", "value": "bleak" }]
///   },
///   "snapshot": {                       // read-only, for whoever debugs the file; import ignores it
///     "atlas": "<version>", "datasetVersion": "…", "storeSha256": "…", "at": "<RFC 3339 UTC>",
///     "changed": ["<knob>", …],
///     "rows": [ /* each seed's /playground/similar answer: its titles' ranks, production ranks, scores
///                  and every signal's value and points; the first 20, or 50 exported in full */ ],
///     "suggest": { /* You Might Also Like, as rows.json gives it */ },
///     "judged": { /* /playground/judged.json, when it was asked for */ }
///   }
/// }
/// ```
pub const FILE_FORMAT: &str = "den-atlas-playground";
pub const FILE_VERSION: u64 = 1;
/// Bumped when a knob is removed or changes meaning. An older file's knob that no longer exists is reported
/// and skipped; a same-version file naming an unknown knob is refused, since it can only be a mistake.
pub const KNOB_SCHEMA: u64 = 1;
/// Titles per seed in a snapshot unless exported in full. In full it is `MAX_ROWS_LIMIT`, what the page
/// can show: at 200 a twelve-seed export cost ~0.8 s of CPU and 2 MB.
const SNAPSHOT_TITLES: usize = 20;

/// Everything the page holds: the tuning and both seed lists.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub tuning: Tuning,
    pub seeds: Vec<(MediaType, u32)>,
    pub suggest: Vec<(MediaType, u32)>,
}

impl State {
    /// The state a query string names: `seeds`, `suggest`, and everything `parse` reads.
    pub fn parse(query: &str) -> Result<State, String> {
        let (seeds, rest) = take_param(query, "seeds");
        let (suggest, rest) = take_param(&rest, "suggest");
        Ok(State {
            seeds: parse_seeds(seeds.as_deref())?,
            suggest: parse_suggest_seeds(suggest.as_deref())?,
            tuning: parse(&rest)?,
        })
    }

    /// The canonical query string for this state — what the page's address and Copy link carry. `State::parse`
    /// reads it back to an equal state.
    pub fn query(&self) -> String {
        let keys = |list: &[(MediaType, u32)]| {
            list.iter().map(|&(m, id)| seed_key(m, id)).collect::<Vec<_>>().join(",")
        };
        let mut parts: Vec<String> = changed(&self.tuning.params)
            .into_iter()
            .map(|name| format!("{name}={}", self.tuning.params.get(name).unwrap_or_default()))
            .collect();
        parts.push(format!("limit={}", self.tuning.limit));
        parts.push(format!("seeds={}", keys(&self.seeds)));
        parts.push(format!("suggest={}", keys(&self.suggest)));
        if !self.tuning.watched.is_empty() {
            parts.push(format!("watched={}", keys(&self.tuning.watched)));
        }
        for (axis, value) in &self.tuning.filters {
            parts.push(format!("{FILTER_PREFIX}{}={}", encode(axis), encode(value)));
        }
        parts.join("&")
    }

    /// The file's `state` object.
    pub fn to_json(&self) -> Value {
        let keys =
            |list: &[(MediaType, u32)]| list.iter().map(|&(m, id)| seed_key(m, id)).collect::<Vec<_>>();
        let knobs: serde_json::Map<String, Value> = changed(&self.tuning.params)
            .into_iter()
            .map(|name| (name.to_owned(), json!(self.tuning.params.get(name))))
            .collect();
        let filters: Vec<Value> =
            self.tuning.filters.iter().map(|(axis, value)| json!({ "axis": axis, "value": value })).collect();
        json!({
            "knobs": knobs,
            "limit": self.tuning.limit,
            "seeds": keys(&self.seeds),
            "suggestSeeds": keys(&self.suggest),
            "watched": keys(&self.tuning.watched),
            "filters": filters,
        })
    }
}

/// Percent-encode everything but unreserved characters, so a filter value survives the address.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The dataset facts a snapshot names.
pub struct Provenance<'a> {
    pub dataset_version: &'a str,
    pub store_sha256: Option<&'a str>,
    /// Unix seconds.
    pub now: u64,
}

/// `GET /playground/export.json?<state>&full=&judged=` — the file described at `FILE_FORMAT`.
pub fn export(
    sources: &Sources<'_>,
    state: &State,
    provenance: &Provenance<'_>,
    full: bool,
    judged_too: bool,
) -> Value {
    let shown = Tuning { limit: if full { MAX_ROWS_LIMIT } else { SNAPSHOT_TITLES }, ..state.tuning.clone() };
    let rows: Vec<Value> = state
        .seeds
        .iter()
        .map(|&(media, id)| {
            let mut row = answer(sources, media, id, &shown);
            row["key"] = json!(seed_key(media, id));
            row
        })
        .collect();
    let mut snapshot = json!({
        "atlas": env!("CARGO_PKG_VERSION"),
        "datasetVersion": provenance.dataset_version,
        "storeSha256": provenance.store_sha256,
        "at": crate::util::rfc3339(provenance.now),
        "changed": changed(&state.tuning.params),
        "rows": rows,
        "suggest": suggest(sources, &state.suggest, &state.tuning),
    });
    if judged_too {
        snapshot["judged"] = judged(sources, &state.tuning);
    }
    json!({
        "format": FILE_FORMAT,
        "version": FILE_VERSION,
        "knobSchema": KNOB_SCHEMA,
        "state": state.to_json(),
        "snapshot": snapshot,
    })
}

/// `POST /playground/import.json` — a file's state, and the knobs an older file named that no longer exist
/// (skipped). Refuses, naming the problem, a file of another format or a newer version, an unknown knob in a
/// file of this knob schema, and any value out of its range. `snapshot` is ignored.
pub fn import(file: &Value) -> Result<(State, Vec<String>), String> {
    if file["format"] != FILE_FORMAT {
        return Err(format!("not a {FILE_FORMAT} file (format {})", file["format"]));
    }
    let version = file["version"].as_u64().ok_or("version: missing")?;
    if version > FILE_VERSION {
        return Err(format!("version {version}: this atlas reads files up to version {FILE_VERSION}"));
    }
    let schema = file["knobSchema"].as_u64().ok_or("knobSchema: missing")?;
    let state = &file["state"];
    let mut tuning = Tuning::default();
    let mut removed = Vec::new();
    for (name, value) in state["knobs"].as_object().into_iter().flatten() {
        if SimilarParams::KNOBS.iter().all(|k| k.name != name) {
            if schema < KNOB_SCHEMA {
                removed.push(name.clone());
                continue;
            }
            return Err(format!("unknown knob {name}"));
        }
        let value = value.as_f64().ok_or_else(|| format!("{name} must be a number, got {value}"))?;
        tuning.params.set(name, value)?;
    }
    if let Some(limit) = state.get("limit").filter(|l| !l.is_null()) {
        tuning.limit = limit
            .as_u64()
            .and_then(|l| usize::try_from(l).ok())
            .filter(|l| (1..=MAX_LIMIT).contains(l))
            .ok_or_else(|| format!("limit must be a whole number from 1 to {MAX_LIMIT}, got {limit}"))?;
    }
    let list = |field: &str, what: &str, max: usize| -> Result<Vec<(MediaType, u32)>, String> {
        let Some(items) = state.get(field).filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
        let items = items.as_array().ok_or_else(|| format!("{field}: a list of movie:/series: keys"))?;
        let keys: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
        parse_keys(
            &keys.ok_or_else(|| format!("{field}: a list of movie:/series: keys"))?.join(","),
            what,
            max,
        )
    };
    tuning.watched = list("watched", "watched", MAX_WATCHED)?;
    for filter in state["filters"].as_array().into_iter().flatten() {
        let (Some(axis), Some(value)) = (filter["axis"].as_str(), filter["value"].as_str()) else {
            return Err(format!("filters: {{\"axis\", \"value\"}} strings, got {filter}"));
        };
        tuning.filters.push((axis.to_owned(), value.to_owned()));
    }
    let state = State {
        seeds: list("seeds", "seeds", MAX_SEEDS)?,
        suggest: list("suggestSeeds", "suggest", MAX_SUGGEST_SEEDS)?,
        tuning,
    };
    Ok((state, removed))
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
pub fn judged(sources: &Sources<'_>, tuning: &Tuning) -> Value {
    use den_index::eval::{mean, score, Scores};
    let (indexes, params) = (sources.indexes, &tuning.params);
    let tuned = *tuning != Tuning { limit: tuning.limit, ..Tuning::default() };
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
        let production = score(&indexes.more_like_this_mixed(c.id, c.media), &c.grades, crate::raileval::K);
        let mine = if tuned {
            let row: Vec<(MediaType, u32)> =
                tuned_row(sources, c.media, c.id, tuning).0.iter().map(Scored::key).collect();
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
        "character": term(s.character, p.w_character),
        "series": term(s.series, p.w_series),
        "region": term(s.region, p.w_region),
        "home": term(s.home, p.w_home),
        "facet": term(s.facet, p.w_facet),
        "noul": term(s.noul, p.w_noul),
        "critique": term(s.critique, p.w_critique),
        "coverage": term(s.coverage, p.w_coverage),
        "tone": term(s.tone, p.w_tone),
        "world": term(s.world, -p.w_world),
        "year": term(s.year, p.w_year),
        "popularity": term(s.popularity, p.w_popularity),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_is_production() {
        assert_eq!(parse("").unwrap(), Tuning::default());
        assert_eq!(Tuning::default().params, SimilarParams::default());
        assert_eq!(Tuning::default().limit, DEFAULT_LIMIT);
    }

    /// No `seeds` in the URL is the default set; an empty one is no seeds at all — the page's "clear".
    #[test]
    fn an_absent_seed_list_is_the_default_set_and_an_empty_one_is_none() {
        assert_eq!(parse_seeds(None).unwrap(), DEFAULT_SEEDS);
        assert_eq!(DEFAULT_SEEDS.len(), 6);
        assert!(DEFAULT_SEEDS.contains(&(MediaType::Tv, 38148)), "Beck, the 1997 series");
        assert_eq!(parse_seeds(Some("")).unwrap(), []);
        let (seeds, rest) = take_param("w_maker=1&limit=5", "seeds");
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
        let (read, rest) = take_param(&query, "seeds");
        assert_eq!(parse_seeds(read.as_deref()).unwrap(), seeds);
        assert_eq!(rest, "w_maker=0.5&limit=10", "the rest still goes through `parse`");
        let (encoded, _) = take_param("seeds=movie%3A5723%2Cseries%3A1438%2Cmovie%3A278", "seeds");
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
        let tuning = parse("w_maker=0.5&pool_k=100&limit=50").unwrap();
        assert_eq!((tuning.params.w_maker, tuning.params.pool_k, tuning.limit), (0.5, 100, 50));
        for (query, names) in [
            ("w_maker=abc", "w_maker"),
            ("w_maker=99", "w_maker"),
            ("w_makr=1", "w_makr"),
            ("limit=0", "limit"),
            ("limit=201", "limit"),
            ("pool_k=1001", "pool_k"),
            ("max_row=401", "max_row"),
            ("watched=1438", "watched"),
            ("filter.tone=", "filter.tone"),
            ("min_rating=11", "min_rating"),
            ("spread_high_pct=50", "spread_high_pct"),
        ] {
            let err = parse(query).unwrap_err();
            assert!(err.contains(names), "{query}: {err}");
        }
    }

    /// Watched titles and filters are read from the address, form-encoded as a browser writes them.
    #[test]
    fn watched_titles_and_filters_are_read() {
        let tuning =
            parse("watched=movie%3A5723,series:1438&filter.tone=bleak&filter.mood=Feel-good").unwrap();
        assert_eq!(tuning.watched, [(MediaType::Movie, 5723), (MediaType::Tv, 1438)]);
        assert_eq!(tuning.filters, [("tone".into(), "bleak".into()), ("mood".into(), "Feel-good".into())]);
        let spaced = parse("filter.subgenre=Police+Procedural").unwrap();
        assert_eq!(spaced.filters, [("subgenre".into(), "Police Procedural".into())]);
    }

    /// A state with every part set, to round-trip.
    fn everything() -> State {
        let mut tuning =
            parse("w_maker=0.5&min_rating=6.5&pool_floor_pct=20&critique_floor=0.15&limit=30").unwrap();
        tuning.watched = vec![(MediaType::Movie, 5723), (MediaType::Tv, 1438)];
        tuning.filters =
            vec![("tone".into(), "bleak".into()), ("subgenre".into(), "Police Procedural".into())];
        State {
            tuning,
            seeds: vec![(MediaType::Tv, 1399), (MediaType::Movie, 278)],
            suggest: vec![(MediaType::Tv, 1438)],
        }
    }

    /// The address (and so Copy link) carries the whole state: `query` read back by `State::parse` is equal.
    /// An empty seed list stays empty rather than becoming the defaults.
    #[test]
    fn the_state_round_trips_through_the_address() {
        let state = everything();
        assert_eq!(State::parse(&state.query()).unwrap(), state);
        let cleared = State { seeds: Vec::new(), suggest: Vec::new(), ..everything() };
        assert_eq!(State::parse(&cleared.query()).unwrap(), cleared);
        let production = State::parse("").unwrap();
        assert_eq!(production.seeds, DEFAULT_SEEDS);
        assert_eq!(production.suggest, DEFAULT_SUGGEST_SEEDS);
        assert_eq!(State::parse(&production.query()).unwrap(), production);
    }

    /// A file's state, imported, is the state exported — whether or not the file carries a snapshot.
    #[test]
    fn the_state_round_trips_through_a_file() {
        let state = everything();
        let file = json!({
            "format": FILE_FORMAT,
            "version": FILE_VERSION,
            "knobSchema": KNOB_SCHEMA,
            "state": state.to_json(),
            "snapshot": { "rows": [{ "anything": "at all" }] },
        });
        let (read, removed) = import(&file).unwrap();
        assert_eq!(read, state);
        assert!(removed.is_empty());
        let mut bare = file.clone();
        bare.as_object_mut().unwrap().remove("snapshot");
        assert_eq!(import(&bare).unwrap().0, state, "the snapshot is optional");
        // Only the knobs moved off production are written.
        assert_eq!(file["state"]["knobs"].as_object().unwrap().len(), 4);
    }

    /// Refused by name: another format, a newer version, an unknown knob in a current file, an out-of-range
    /// value, a malformed key. An older file's missing knob is reported and skipped.
    #[test]
    fn a_bad_file_is_refused_by_name_and_an_old_one_says_what_it_lost() {
        let file = |state: Value, version: u64, schema: u64| json!({ "format": FILE_FORMAT, "version": version, "knobSchema": schema, "state": state });
        let refused = |f: Value| import(&f).unwrap_err();
        assert!(refused(json!({ "format": "other" })).contains("den-atlas-playground"));
        assert!(refused(file(json!({}), FILE_VERSION + 1, KNOB_SCHEMA)).contains("version"));
        assert!(refused(file(json!({ "knobs": { "w_nope": 1 } }), 1, KNOB_SCHEMA)).contains("w_nope"));
        assert!(refused(file(json!({ "knobs": { "w_maker": 99 } }), 1, KNOB_SCHEMA)).contains("w_maker"));
        assert!(refused(file(json!({ "knobs": { "w_maker": "x" } }), 1, KNOB_SCHEMA)).contains("w_maker"));
        assert!(refused(file(json!({ "limit": 0 }), 1, KNOB_SCHEMA)).contains("limit"));
        assert!(refused(file(json!({ "seeds": ["tv:1"] }), 1, KNOB_SCHEMA)).contains("seeds"));
        assert!(refused(file(json!({ "filters": [{ "axis": "tone" }] }), 1, KNOB_SCHEMA)).contains("filters"));
        let (old, removed) =
            import(&file(json!({ "knobs": { "w_retired": 1, "w_maker": 0.5 } }), 1, KNOB_SCHEMA - 1))
                .unwrap();
        assert_eq!(removed, ["w_retired"]);
        assert_eq!(old.tuning.params.w_maker, 0.5, "the rest of an old file still imports");
    }

    /// The byte-identity proof: More Like This ranked with `SimilarParams::default()` — through the
    /// serving entry point and through the playground's — against the rows the scorer served for five
    /// anchors BEFORE it took parameters (`similar-golden.json`: the whole row, `limit=200`, first captured
    /// from den-atlas 0.53.0 over HTTP). `scripts/similar-golden.py` recaptures it; it was recaptured when
    /// the scorer's `ln` moved to `libm`, which moved scores by ULPs and changed no id.
    ///
    /// The golden rows are of one type, and `/index/similar`'s `ids` and `total` — what every client reads —
    /// are the golden exactly, as on main. The mixed row (`mixed`, `mix_types` on) holds the seed type's
    /// titles in the golden's order, its first ones; the other type is merged in between and never reorders
    /// them.
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
        let one_type = SimilarParams { mix_types: false, ..SimilarParams::default() };
        for anchor in anchors {
            let media = if anchor["type"] == "movie" { MediaType::Movie } else { MediaType::Tv };
            let id = anchor["id"].as_u64().unwrap() as u32;
            let want: Vec<u32> =
                anchor["ids"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
            assert!(!want.is_empty(), "{}", anchor["name"]);
            // The fields every client reads, as `/index/similar` serves them: byte for byte main's row.
            assert_eq!(&*indexes.more_like_this(id, media), want.as_slice(), "{} (serving)", anchor["name"]);
            let answer = crate::handler::similar_json(&indexes, media, id, "limit=200");
            assert_eq!(answer["ids"], serde_json::json!(want), "{} (/index/similar ids)", anchor["name"]);
            assert_eq!(answer["total"], want.len(), "{} (/index/similar total)", anchor["name"]);
            let own = |row: &[(MediaType, u32)]| -> Vec<u32> {
                row.iter().filter(|&&(kind, _)| kind == media).map(|&(_, id)| id).collect()
            };
            // The mixed row, beside them: its titles of the seed's type are the golden's first ones.
            let served = own(&indexes.more_like_this_mixed(id, media));
            assert!(served.len() <= want.len(), "{}", anchor["name"]);
            assert_eq!(served, want[..served.len()], "{} (mixed, its own type)", anchor["name"]);
            let tuned: Vec<(MediaType, u32)> = indexes
                .more_like_this_scored(id, media, &SimilarParams::default())
                .iter()
                .map(Scored::key)
                .collect();
            assert_eq!(own(&tuned), served, "{} (playground, default parameters)", anchor["name"]);
            let single: Vec<u32> =
                indexes.more_like_this_scored(id, media, &one_type).iter().map(|s| s.tmdb_id).collect();
            assert_eq!(single, want, "{} (mix_types = 0)", anchor["name"]);
        }

        // The character links, built from the credits `CACHE_DIR` keeps, change nothing while unweighed:
        // not the pool, not a score. Without `CACHE_DIR` the indexes above have no links at all.
        let Ok(cache) = std::env::var("CACHE_DIR") else {
            eprintln!("SKIP: set CACHE_DIR to kept TMDB credits to check the links at w_character = 0");
            return;
        };
        let mapped = crate::store::MappedStore::open(std::path::Path::new(&store)).expect("store");
        let credits = crate::tmdb::read_credits(&std::path::Path::new(&cache).join("tmdb-credits.tsv"), 0);
        let list = crate::characters::build(&mapped.view(), &credits).expect("the links build");
        assert!(list.links() > 0, "CACHE_DIR holds no credits that link anything");
        let linked = crate::queries::IndexQueries::new(&ds)
            .with_characters(Some(std::sync::Arc::new(crate::characters::Characters::with_index(list))));
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (indexes, _) = runtime.block_on(linked.get(|| ())).expect("the indexes load");
        let unweighed = SimilarParams { w_character: 0.0, ..one_type };
        for anchor in anchors {
            let media = if anchor["type"] == "movie" { MediaType::Movie } else { MediaType::Tv };
            let id = anchor["id"].as_u64().unwrap() as u32;
            let want: Vec<u32> =
                anchor["ids"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
            let row: Vec<u32> =
                indexes.more_like_this_scored(id, media, &unweighed).iter().map(|s| s.tmdb_id).collect();
            assert_eq!(row, want, "{} (character links loaded, w_character = 0)", anchor["name"]);
        }
    }

    /// On the real corpus: the playground's own path at production's `Tuning` — the audience consulted,
    /// every filter at rest — ranks the golden anchors exactly as serving does; and the two knobs that need
    /// aggregates built for them, `critique_floor` and `holds`, each move The Wire's row.
    ///
    /// Opt-in, as above.
    #[test]
    fn the_playground_path_is_production_at_rest_and_the_aggregate_knobs_move_a_row() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to exercise this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let indexes = crate::queries::load_for_tools(&crate::dataset::Dataset::load(dir).expect("dataset"))
            .expect("the indexes load");
        let sources = Sources { indexes: &indexes, export: None };
        let golden: Value = serde_json::from_str(include_str!("similar-golden.json")).unwrap();
        for anchor in golden["anchors"].as_array().unwrap() {
            let media = if anchor["type"] == "movie" { MediaType::Movie } else { MediaType::Tv };
            let id = anchor["id"].as_u64().unwrap() as u32;
            let (row, tilted) = tuned_row(&sources, media, id, &Tuning::default());
            let row: Vec<(MediaType, u32)> = row.iter().map(Scored::key).collect();
            assert_eq!(row.as_slice(), &*indexes.more_like_this_mixed(id, media), "{}", anchor["name"]);
            assert!(!tilted);
        }
        let wire = |query: &str| -> Vec<u32> {
            let tuning = parse(query).unwrap();
            tuned_row(&sources, MediaType::Tv, 1438, &tuning).0.iter().take(20).map(|s| s.tmdb_id).collect()
        };
        let production = wire("");
        assert_ne!(wire("critique_floor=0.5"), production, "critique_floor");
        assert_ne!(wire("holds=0.3"), production, "holds");
        assert_eq!(wire("critique_floor=0.1&holds=0.7"), production, "production's values, spelled out");
    }
}
