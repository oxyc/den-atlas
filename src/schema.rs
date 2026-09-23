//! The self-describing query schema. Every population count carries both the number of titles for which the
//! field is known and the full corpus denominator; partial classification must never read as a census.

use crate::facts::{Record, SourceKinds};
use crate::queries::Indexes;
use den_index::{MediaType, DISPLAY_CONFIDENCE_FLOOR};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

type Key = (MediaType, u32);

fn type_name(media_type: MediaType) -> &'static str {
    if media_type == MediaType::Tv {
        "series"
    } else {
        "movie"
    }
}

/// The titles a scoped count is out of: the whole corpus, or the titles of one type.
pub fn population(indexes: &Indexes, scope: Option<MediaType>) -> usize {
    let Some(media_type) = scope else { return indexes.population };
    indexes
        .facets
        .as_ref()
        .map(|facets| facets.coverage_for("mediaType", Some(media_type)))
        .filter(|&n| n > 0)
        .or_else(|| {
            indexes.plot.media_type_counts().into_iter().find(|(kind, _)| *kind == media_type).map(|x| x.1)
        })
        .unwrap_or(0)
}

/// Titles whose facts record passes `on_record`, among the titles of `scope`.
fn on_record(indexes: &Indexes, scope: Option<MediaType>, on_record: impl Fn(&Record) -> bool) -> usize {
    indexes.facts.as_ref().map_or(0, |facts| {
        facts
            .keys()
            .filter(|(kind, _)| scope.is_none_or(|want| *kind == want))
            .filter(|&(kind, id)| facts.get(id, kind).is_some_and(&on_record))
            .count()
    })
}

/// Every title anything gives a genre for, as `plotrows::genres` reads them: the labels and the facts.
fn genre_keys(indexes: &Indexes, scope: Option<MediaType>) -> HashSet<Key> {
    let facts = indexes.facts.as_ref().into_iter().flat_map(crate::facts::Facts::keys);
    indexes
        .plot
        .titles()
        .chain(facts)
        .filter(|(kind, _)| scope.is_none_or(|want| *kind == want))
        .filter(|&key| !crate::plotrows::genres(indexes, key).is_empty())
        .collect()
}

/// How many titles of `scope` have `field` on record — the one definition of "known" behind every coverage figure
/// atlas reports, in the schema, on a browse row and on a search. A title without it is unknown, never a
/// mismatch, so this is the most any count over the field can reach.
pub fn known(indexes: &Indexes, field: &str, scope: Option<MediaType>) -> usize {
    let floor = DISPLAY_CONFIDENCE_FLOOR;
    match field {
        "mediaType" => population(indexes, scope),
        "country" | "language" | "year" | "decade" => {
            indexes.facets.as_ref().map_or(0, |facets| facets.coverage_for(field, scope))
        }
        "subgenre" => indexes.plot.subgenre_coverage_for(scope, floor),
        "mood" => indexes.plot.mood_coverage_for(scope, floor),
        "runtimeMinutes" => on_record(indexes, scope, |r| r.runtime_minutes.is_some()),
        "basedOnKind" => on_record(indexes, scope, |r| !r.source_kinds.is_empty()),
        "broadcaster" => on_record(indexes, scope, |r| !r.broadcasters.is_empty()),
        "people" => on_record(indexes, scope, |r| !r.makers.is_empty() || !r.cast.is_empty()),
        "genre" => genre_keys(indexes, scope).len(),
        axis => indexes.plot_facets.as_ref().map_or(0, |facets| facets.coverage_for(axis, scope)),
    }
}

fn coverage(known: usize, population: usize) -> Value {
    json!({
        "count": known,
        "denominator": population,
        "ratio": if population == 0 { 0.0 } else { known as f64 / population as f64 },
    })
}

fn counted_values(values: Vec<(String, usize)>, known: usize, population: usize) -> Value {
    values
        .into_iter()
        .map(|(value, matched)| {
            json!({
                "value": value,
                "count": {
                    "matched": matched,
                    "known": known,
                    "population": population,
                }
            })
        })
        .collect()
}

fn field(
    kind: &str,
    filterable: bool,
    known: usize,
    population: usize,
    values: Vec<(String, usize)>,
) -> Value {
    let mut answer = json!({
        "type": kind,
        "filterable": filterable,
        "coverage": coverage(known, population),
    });
    if !values.is_empty() {
        answer["values"] = counted_values(values, known, population);
    }
    answer
}

fn named(values: Vec<(&str, usize)>) -> Vec<(String, usize)> {
    values.into_iter().map(|(value, count)| (value.to_owned(), count)).collect()
}

/// `/index/schema.json`: what can be queried, its value vocabulary, and how much of the corpus can honestly
/// answer each field. It deliberately says that a search's totals are retrieval counts, not corpus aggregates;
/// counting and group-by are the filter routes' (`/index/filter/{type}/counts.json`), which AND bitsets over
/// the corpus, and must not be built by relabelling search output.
pub fn document(indexes: &Indexes) -> Value {
    let population = indexes.population;
    let floor = DISPLAY_CONFIDENCE_FLOOR;
    let mut fields = Map::new();

    let type_values: Vec<(String, usize)> = indexes.facets.as_ref().map_or_else(
        || {
            indexes
                .plot
                .media_type_counts()
                .into_iter()
                .map(|(kind, count)| {
                    (if kind == MediaType::Tv { "series" } else { "movie" }.to_owned(), count)
                })
                .collect()
        },
        |facets| facets.value_counts("mediaType"),
    );
    let type_known = type_values.iter().map(|(_, count)| count).sum();
    fields.insert("mediaType".to_owned(), field("enum", true, type_known, population, type_values));

    let genres = named(indexes.plot.primary_genre_counts());
    let genre_known = genres.iter().map(|(_, count)| count).sum();
    fields.insert("primaryGenre".to_owned(), field("enum", true, genre_known, population, genres));

    let subgenres = named(indexes.plot.subgenre_counts(floor));
    let subgenre_known = indexes.plot.subgenre_coverage(floor);
    fields.insert("subgenre".to_owned(), field("enum", true, subgenre_known, population, subgenres));

    let moods = named(indexes.plot.mood_counts(floor));
    let mood_known = indexes.plot.mood_coverage(floor);
    fields.insert("mood".to_owned(), field("enum", true, mood_known, population, moods));

    for (name, kind, enumerable) in [
        ("country", "countryCode", true),
        ("language", "languageCode", true),
        ("year", "integer", false),
        ("decade", "integer", true),
    ] {
        let known = indexes.facets.as_ref().map_or(0, |facets| facets.coverage(name));
        let values = if enumerable {
            indexes.facets.as_ref().map_or_else(Vec::new, |facets| facets.value_counts(name))
        } else {
            Vec::new()
        };
        fields.insert(name.to_owned(), field(kind, true, known, population, values));
    }

    if let Some(plot_facets) = &indexes.plot_facets {
        for facet in plot_facets.schema() {
            // Apart from `values`, whose counts partition the axis: a merged row repeats its members' titles.
            let merged: Vec<Value> = crate::plotrows::MERGED_ROWS
                .iter()
                .filter(|m| m.axis == facet.axis)
                .map(|m| {
                    let matched: usize = facet
                        .values
                        .iter()
                        .filter(|(value, _)| m.members.contains(&value.as_str()))
                        .map(|(_, count)| count)
                        .sum();
                    json!({
                        "value": m.value,
                        "title": m.title,
                        "of": m.members,
                        "count": { "matched": matched, "known": facet.known, "population": population },
                    })
                })
                .collect();
            let mut entry = field("enum", true, facet.known, population, facet.values);
            if !merged.is_empty() {
                entry["merged"] = json!(merged);
            }
            fields.insert(facet.axis, entry);
        }
    }

    fields.insert(
        "title".to_owned(),
        field(
            "string",
            true,
            indexes.cards.as_ref().map_or(0, std::collections::HashMap::len),
            population,
            vec![],
        ),
    );
    fields.insert("plotSemantic".to_owned(), field("vector", true, indexes.plot.len(), population, vec![]));
    fields.insert(
        "premiseSemantic".to_owned(),
        field("vector", true, indexes.premise.as_ref().map_or(0, den_index::Index::len), population, vec![]),
    );
    fields.insert(
        "people".to_owned(),
        field("entity", true, known(indexes, "people", None), population, vec![]),
    );

    // The facts `/index/query` reads that the fields above did not name.
    fields.insert(
        "runtimeMinutes".to_owned(),
        field("integer", true, known(indexes, "runtimeMinutes", None), population, vec![]),
    );
    let kinds = named(
        SourceKinds::names(u16::MAX)
            .into_iter()
            .map(|name| {
                let bit = SourceKinds::parse(name).unwrap_or(0);
                (name, on_record(indexes, None, |r| r.source_kinds.contains(bit)))
            })
            .filter(|&(_, count)| count > 0)
            .collect(),
    );
    fields.insert(
        "basedOnKind".to_owned(),
        field("enum", true, known(indexes, "basedOnKind", None), population, sorted(kinds)),
    );
    let series = self::population(indexes, Some(MediaType::Tv));
    fields.insert(
        "broadcaster".to_owned(),
        field("entity", true, known(indexes, "broadcaster", Some(MediaType::Tv)), series, vec![]),
    );
    let genre_titles = genre_keys(indexes, None);
    let mut genres: BTreeMap<u16, usize> = BTreeMap::new();
    for &key in &genre_titles {
        for genre in crate::plotrows::genres(indexes, key) {
            *genres.entry(genre).or_default() += 1;
        }
    }
    let genres = genres.into_iter().map(|(id, count)| (id.to_string(), count)).collect();
    fields.insert("genre".to_owned(), field("enum", true, genre_titles.len(), population, sorted(genres)));

    for (name, about) in [
        ("mediaType", json!({ "parameter": "type" })),
        ("primaryGenre", json!({ "format": "label name" })),
        ("subgenre", json!({ "format": "label name", "minConfidence": DISPLAY_CONFIDENCE_FLOOR })),
        ("mood", json!({ "format": "label name", "minConfidence": DISPLAY_CONFIDENCE_FLOOR })),
        ("country", json!({ "format": "ISO 3166-1 alpha-2", "of": "country of origin" })),
        ("language", json!({ "format": "ISO 639-1", "of": "original language", "parameter": "language" })),
        ("year", json!({ "unit": "calendar year", "of": "release", "parameter": ["year_min", "year_max"] })),
        (
            "decade",
            json!({ "unit": "calendar year", "format": "the decade's first year: 1980 is 1980-1989" }),
        ),
        (
            "runtimeMinutes",
            json!({ "unit": "minutes", "parameter": "runtime_max",
                    "note": "a series' runtime is per episode, so runtime_max drops films only" }),
        ),
        (
            "broadcaster",
            json!({ "format": "Wikidata Q-id", "of": "network or service a series first aired on",
                    "appliesTo": "series", "parameter": "broadcaster" }),
        ),
        ("genre", json!({ "format": "TMDB genre id" })),
        ("people", json!({ "format": "Wikidata Q-id", "of": "directors, creators, writers and cast" })),
        ("plotSemantic", json!({ "parameter": "q" })),
        ("premiseSemantic", json!({ "parameter": "q" })),
        ("title", json!({ "parameter": "q" })),
    ] {
        if let (Some(Value::Object(target)), Value::Object(about)) = (fields.get_mut(name), about) {
            target.extend(about);
        }
    }

    json!({
        "schemaVersion": 1,
        "datasetVersion": indexes.dataset_version,
        "taxonomyVersion": indexes.plot.taxonomy_version(),
        "population": { "count": population },
        "fields": fields,
        "semantics": {
            "missing": "unknown",
            "resultTotal": "retrievedCandidatesNotCorpusCount",
            // One axis at a time, under any AND-ed selection: counts.json counts every other kind's values
            // among the titles carrying the selection.
            "groupBy": true,
            "groupByRoute": "/index/filter/{type}/counts.json",
            "counts": {
                "coverage": "count: titles with the field on record; denominator: the titles it is out of (the \
                             corpus, or the media type the field or route is scoped to); ratio: count / \
                             denominator",
                "valueCount": "matched: titles with this value; known: titles with the field on record; \
                               population: the titles the field is out of",
                "mergedCount": "matched: titles with any value the merged row is of, so it overlaps \
                                values and is not added to them",
                "rowTotal": "titles carrying every constraint among those with each constraint's field on \
                             record, which is at most coverage.fields.<field>.count",
                "resultTotal": "candidates the search retrieved and scored above zero: a pool the ranking \
                                drew from, not a corpus count",
                "filterTotal": "total on the filter routes: titles on record as carrying every selected value, \
                                out of denominator, the titles of the route's type. A title with a selected kind \
                                unknown is not counted, so it is a floor: coverage says how much of the type \
                                each selected kind is known for",
                "filterValueCount": "kinds.<kind>.values.<id> on counts.json, and values[].count on \
                                     values/{kind}.json: titles carrying the selection and that value, out of \
                                     that answer's denominator (the selection's titles, or for a single-mode kind \
                                     with a value selected, the selection without it, so it may exceed total). \
                                     A title with the kind unknown is in the denominator and in no value, so the \
                                     values need not sum to it",
            },
            "applied": {
                "filter": "drops a title on record as not matching; keeps one with no record, ranked lower",
                "discount": "read from the query's words, so it ranks a title on record as not matching far \
                             lower but never drops it",
                "boost": "raises a title that carries it; never lowers or drops one that does not",
                "require": "drops every title not on record as matching, unknown included",
            },
        },
        "routes": routes(),
        "filter": crate::filter::schema(),
        "tmdb": tmdb(),
    })
}

/// What in atlas's answers is TMDB's own data, read at run time (`tmdb.rs`): usable here only to filter and sort,
/// and never to reach a model, so a client that hands answers to one (an MCP tool) drops exactly these. TMDB ids
/// and TMDB's genre id space are identifiers; the genre values are Wikidata's.
fn tmdb() -> Value {
    json!({
        "about": "Values atlas takes from TMDB at run time. They filter and sort inside atlas and must not be \
                  passed to a model, an MCP tool result included, nor stored.",
        "filterKinds": crate::filter::tmdb_kinds(),
        "fields": {
            "/index/query.json": [
                "hits[].f.pop: popularity from TMDB's vote count, else its export's popularity",
                "hits[].title where hits[].titleFrom is \"tmdb\": named by TMDB's daily export, the corpus having \
                 no card for it",
            ],
            "/index/filter/{type}/counts.json": [
                "kinds.<kind> and coverage.<kind> for a kind in filterKinds",
                "every count, when the selection names a kind in filterKinds",
            ],
            "/index/filter/{type}/titles.json": [
                "total and coverage, when the selection names a kind in filterKinds",
            ],
            "/index/filter/{type}/values/{kind}.json": [
                "the whole answer, for a kind in filterKinds or a selection naming one",
            ],
        },
        "order": "most voted, as rows and filter titles are ordered, is by TMDB's vote counts (else its \
                  popularity), used as a sort key",
        "identifiers": "id, tmdbId and people[].id are TMDB ids and genre ids are in TMDB's genre id space: \
                        identifiers, not TMDB data",
    })
}

fn sorted(mut values: Vec<(String, usize)>) -> Vec<(String, usize)> {
    values.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    values
}

fn param(name: &str, kind: &str, about: &str) -> Value {
    json!({ "name": name, "type": kind, "about": about })
}

fn with(mut value: Value, extra: Value) -> Value {
    if let (Value::Object(target), Value::Object(extra)) = (&mut value, extra) {
        target.extend(extra);
    }
    value
}

fn paging(default: usize, max: usize) -> [Value; 2] {
    [
        with(param("skip", "integer", "titles to skip before the page"), json!({ "default": 0 })),
        with(param("limit", "integer", "titles in the page"), json!({ "default": default, "max": max })),
    ]
}

/// Every public route an agent can call, with its parameters, what it returns and what its counts are out of —
/// enough to use the API without reading this code. `/metrics` (a token) and `/playground` (off in production)
/// are left out. `example` is a request that resolves, and the route tests call each one.
fn routes() -> Value {
    let floor = DISPLAY_CONFIDENCE_FLOOR;
    let [skip, limit] = paging(crate::search::PAGE, crate::search::MAX_PAGE);
    let query = json!({
        "method": "GET",
        "path": "/index/query.json",
        "example": "/index/query.json?q=bleak%20finnish%201980s%20films",
        "about": "Search in one request: the query is read for what it names (country, decade, type, genre, \
                  label, plot facet, person, adaptation source) and every candidate is scored on every signal.",
        "parameters": [
            param("q", "string", "the query; words it does not read as a constraint are in parse.leftover and \
                                  are matched as prose. Words after not, no, without, except or excluding \
                                  drop every title on record as having what they name (parse.excluded)"),
            with(param("type", "enum", "movie or series"), json!({ "field": "mediaType" })),
            skip,
            limit,
            with(param("year_min", "integer", "earliest release year, inclusive"), json!({ "field": "year" })),
            with(param("year_max", "integer", "latest release year, inclusive"), json!({ "field": "year" })),
            with(param("language", "string", "original language"), json!({ "field": "language" })),
            with(param("runtime_max", "integer", "longest runtime"), json!({ "field": "runtimeMinutes" })),
            with(param("broadcaster", "string", "Q-id, with or without the Q"), json!({ "field": "broadcaster" })),
        ],
        "returns": "{parse, people, hits, total, semantics, coverage, ignored, unknownValues?}",
        "counts": {
            "total": "resultTotal",
            "coverage": "each constraint the query applied, how it was applied, and how many titles have its \
                         field on record out of the titles of the type asked for",
            "people[].credits": "that person's titles in the corpus",
        },
        "ignored": "parameters the route does not read, or whose value it could not read: the answer is as \
                    though they were not sent",
        "unknownValues": "language:<code> for a language no title is in; it still filters, leaving the titles \
                          with no language on record",
    });
    use crate::handler::{
        FACET_LIMIT, MAX_NEIGHBOUR_K, MAX_ROW_PAGE, MAX_SEEDS, MAX_TITLES, NEIGHBOUR_K, ROW_PAGE, SEMANTIC_K,
        SIMILAR_PAGE,
    };
    let [row_skip, row_limit] = paging(ROW_PAGE, MAX_ROW_PAGE);
    json!([
        {
            "method": "GET", "path": "/index/schema.json", "example": "/index/schema.json",
            "about": "This document: fields, value vocabularies with counts, coverage, semantics and routes.",
        },
        query,
        {
            "method": "GET",
            "path": "/index/row/{type}.json",
            "example": "/index/row/movie.json?tone=bleak",
            "alias": "/index/plot/{type}.json",
            "about": "A browse row: the titles of one type carrying every constraint, most confident then most \
                      voted. The rows that count. Any other field (country, decade, language, …) is refused with \
                      a 400 naming /index/filter/{type}/titles.json, which is where the facts are filtered.",
            "parameters": [
                with(param("type", "enum", "movie or series"), json!({ "in": "path", "field": "mediaType" })),
                with(
                    param("{axis}", "string", "a plot-facet axis and one of its values, e.g. tone=bleak, or \
                                               one of its merged rows (fields.<axis>.merged), which \
                                               matches any of the values it is of; repeatable, one per \
                                               axis"),
                    json!({ "field": "any enum field of fields whose name is a plot-facet axis" }),
                ),
                with(param("subgenre", "string", "a subgenre label"), json!({ "field": "subgenre" })),
                with(param("mood", "string", "a mood label"), json!({ "field": "mood" })),
                row_skip.clone(),
                row_limit.clone(),
                param("tilt.liked", "string", "reorders for a household: comma-separated m<id>/t<id>, at most \
                                               500 (tilt.w.embedding, tilt.w.dislike, tilt.w.era and \
                                               tilt.w.square weigh it); never changes total"),
                param("tilt.disliked", "string", "as tilt.liked"),
                param("tilt.era", "string", "<center>,<spread> in calendar years"),
            ],
            "returns": "{titles, total, coverage}",
            "counts": { "total": "rowTotal", "coverage": "per field, out of the titles of {type}" },
        },
        {
            "method": "GET",
            "path": "/index/rows/{type}/{family}/{label}.json",
            "example": "/index/rows/movie/subgenre/Heist.json",
            "about": format!("The titles of one type carrying a label at confidence {floor} or more."),
            "parameters": [
                with(param("type", "enum", "movie or series"), json!({ "in": "path", "field": "mediaType" })),
                with(param("family", "enum", "subgenre or mood"), json!({ "in": "path" })),
                with(param("label", "string", "the label, percent-encoded"), json!({ "in": "path" })),
                row_skip,
                row_limit,
            ],
            "returns": "{ids, total, coverage}",
            "counts": { "total": "rowTotal", "coverage": "the label family, out of the titles of {type}" },
        },
        {
            "method": "GET",
            "path": "/index/similar/{type}/{tmdbId}.json",
            "example": "/index/similar/movie/1.json",
            "about": "More Like This for one title, best first.",
            "parameters": [
                with(param("type", "enum", "movie or series"), json!({ "in": "path" })),
                with(param("tmdbId", "integer", "TMDB id"), json!({ "in": "path" })),
                with(param("skip", "integer", "titles to skip"), json!({ "default": 0 })),
                with(param("limit", "integer", "titles in the page"),
                     json!({ "default": SIMILAR_PAGE, "max": den_index::MAX_ROW })),
            ],
            "returns": "{ids, total, mixed:[{type,id}], mixedTotal}: ids the title's own type; mixed the same row \
                        with films and series together, paged alike",
            "counts": {
                "total": format!("the length of the ranked list, at most {}", den_index::MAX_ROW),
                "mixedTotal": format!("the length of the mixed list, at most {}", den_index::MAX_ROW),
            },
        },
        {
            "method": "GET",
            "path": "/index/neighbours/{type}/{tmdbId}.json",
            "example": "/index/neighbours/movie/1.json",
            "about": "The plain plot-vector neighbours of one title.",
            "parameters": [
                with(param("type", "enum", "movie or series"), json!({ "in": "path" })),
                with(param("tmdbId", "integer", "TMDB id"), json!({ "in": "path" })),
                with(param("k", "integer", "neighbours"),
                     json!({ "default": NEIGHBOUR_K, "max": MAX_NEIGHBOUR_K })),
            ],
            "returns": "{ids}",
        },
        {
            "method": "GET",
            "path": "/index/studios.json",
            "example": "/index/studios.json",
            "about": "The iconic studios: production companies a viewer browses by, from a hand-kept list, each \
                      with its titles here, most titles first. A studio's row is \
                      /index/filter/{type}/titles.json?sel=studio:<id>. Empty for a dataset without them.",
            "returns": "{studios:[{id, name, movies, series}]}: id the studio's own Wikidata Q-id",
            "counts": { "movies": "films with a card crediting the studio", "series": "series likewise" },
        },
        {
            "method": "GET",
            "path": "/index/studios/{type}/{tmdbId}.json",
            "example": "/index/studios/movie/1.json",
            "about": "One title's iconic studios, in the order it credits them: what a detail header links to the \
                      studio's row. A title crediting a studio's TV or animation arm names the studio.",
            "parameters": [
                with(param("type", "enum", "movie or series"), json!({ "in": "path" })),
                with(param("tmdbId", "integer", "TMDB id"), json!({ "in": "path" })),
            ],
            "returns": "{studios:[{id, name}]}: empty for a title with none or not in the corpus",
        },
        {
            "method": "GET",
            "path": "/index/title/{type}/{tmdbId}.json",
            "example": "/index/title/movie/1.json",
            "about": "One title as the corpus describes it. A title with no card is {type, id, indexed: false}: \
                      Den has nothing on it, which does not mean it does not exist.",
            "parameters": [
                with(param("type", "enum", "movie or series"), json!({ "in": "path" })),
                with(param("tmdbId", "integer", "TMDB id"), json!({ "in": "path" })),
            ],
            "returns": "the card /index/row draws, with indexed, labels {primaryGenre, animated, subgenres, moods} \
                        at the display floor, plotFacets {axis: value} at the same floor, and from the facts \
                        countries, languages, runtimeMinutes, basedOn, makers and cast [{id: Q-id, name, tmdbId?}] \
                        (cast at most 60, castTotal all of them)",
        },
        {
            "method": "GET",
            "path": "/index/search.json",
            "example": "/index/search.json?q=heist",
            "about": "Semantic search alone: the plot vectors nearest to the query. 503 without the embedder.",
            "parameters": [
                param("q", "string", "the query"),
                with(param("type", "enum", "movie or series"), json!({ "field": "mediaType" })),
            ],
            "returns": format!("{{titles:[{{type,id,score}}], mean, sd}}: the {SEMANTIC_K} nearest, with the \
                                scan's mean and standard deviation"),
        },
        {
            "method": "GET",
            "path": "/index/facets.json",
            "example": "/index/facets.json?q=korean%20heist",
            "about": "The titles matching the country, decade and type the query names, most voted first.",
            "parameters": [param("q", "string", "the query")],
            "returns": format!("{{facet, titles}}: at most {FACET_LIMIT} titles"),
        },
        {
            "method": "GET",
            "path": "/index/filter/{type}/counts.json",
            "example": "/index/filter/movie/counts.json?sel=country:KR,genre:18",
            "about": "For every value of every listed kind, the titles of one type (of both, under all) carrying \
                      the selection and that value; `filter` in this document describes the kinds, the \
                      canonical form and all.",
            "parameters": [
                with(
                    param("type", "enum", "movie, series or all (films and series together)"),
                    json!({ "in": "path", "field": "mediaType" }),
                ),
                with(
                    param("sel", "string", "[-]<kind>:<id>, comma-separated, in canonical order"),
                    json!({ "max": crate::filter::MAX_SELECTION }),
                ),
            ],
            "returns": "{total, denominator, kinds: {<kind>: {mode, complete, values: {<id>: n}, denominator, \
                        labels?, selected?, excluded?}}, coverage, ignored, unknownValues?, kindsUnavailable?}",
            "counts": { "total": "filterTotal", "kinds.<kind>.values": "filterValueCount" },
        },
        {
            "method": "GET",
            "path": "/index/filter/{type}/titles.json",
            "example": "/index/filter/movie/titles.json?sel=genre:18&limit=40",
            "about": "The titles of one type (of both, under all: merged by rank within type) carrying the selection, \
                      most voted first (in similarity order with a like selected), as /index/row/{type}.json \
                      draws them.",
            "parameters": [
                with(
                    param("type", "enum", "movie, series or all (films and series together)"),
                    json!({ "in": "path", "field": "mediaType" }),
                ),
                with(param("sel", "string", "as counts.json"), json!({ "max": crate::filter::MAX_SELECTION })),
                with(param("skip", "integer", "a multiple of limit; left out when 0"), json!({ "default": 0 })),
                with(
                    param("limit", "integer", "titles in the page; left out when the default"),
                    json!({ "default": crate::filter::PAGE, "max": crate::filter::MAX_PAGE }),
                ),
            ],
            "returns": "{titles, total, denominator, order, coverage, ignored, unknownValues?, kindsUnavailable?}",
            "counts": { "total": "filterTotal" },
        },
        {
            "method": "GET",
            "path": "/index/filter/{type}/values/{kind}.json",
            "example": "/index/filter/movie/values/person.json?q=lead",
            "about": "One kind's values under the selection, labelled, most titles first; with q, those with a \
                      word starting q (a character: its name starting q).",
            "parameters": [
                with(
                    param("type", "enum", "movie, series or all (films and series together)"),
                    json!({ "in": "path", "field": "mediaType" }),
                ),
                with(param("kind", "enum", "any kind but like"), json!({ "in": "path" })),
                with(param("sel", "string", "as counts.json"), json!({ "max": crate::filter::MAX_SELECTION })),
                with(
                    param("q", "string", "a prefix, normalised as the kind's names are"),
                    json!({ "min": crate::filter::MIN_PREFIX, "minCharacter": crate::filter::CHARACTER_MIN_PREFIX }),
                ),
                with(
                    param("limit", "integer", "values returned"),
                    json!({ "default": crate::filter::VALUES_LIMIT, "max": crate::filter::VALUES_LIMIT,
                            "maxCharacter": crate::filter::CHARACTER_LIMIT }),
                ),
            ],
            "returns": "{kind, mode, values: [{id, name, count, tmdbId?}], complete, denominator, ignored, \
                        unknownValues?, kindsUnavailable?}",
            "counts": { "values[].count": "filterValueCount" },
        },
        {
            "method": "GET",
            "path": "/index/filter/{type}/people.json",
            "example": "/index/filter/movie/people.json?sel=decade:1980&traits=role:cast",
            "about": "The people credited on the titles of one type (of both, under all) carrying the selection, \
                      holding every person trait, most matching titles first, then most titles in the corpus, \
                      then by Q-id. filter.traits in this document describes the trait kinds.",
            "parameters": [
                with(
                    param("type", "enum", "movie, series or all (films and series together)"),
                    json!({ "in": "path", "field": "mediaType" }),
                ),
                with(
                    param("sel", "string", "the titles, as counts.json"),
                    json!({ "max": crate::filter::MAX_SELECTION }),
                ),
                with(
                    param("traits", "string", "[-]<trait>:<id>, comma-separated, in canonical order"),
                    json!({ "max": crate::filter::MAX_SELECTION }),
                ),
                with(param("skip", "integer", "a multiple of limit; left out when 0"), json!({ "default": 0 })),
                with(
                    param("limit", "integer", "people in the page; left out when the default"),
                    json!({ "default": crate::filter::PAGE, "max": crate::filter::MAX_PAGE }),
                ),
            ],
            "returns": "{people: [{id, name, tmdbId?, credits, roles, gender?, born?, died?, citizenship?, \
                        occupation?}], total, labels, coverage, ignored, ignoredTraits?, unknownValues?, \
                        unknownTraits?, kindsUnavailable?, traitsUnavailable?}",
            "counts": {
                "people[].credits": "matching titles the person's counted credits are on",
                "people[].born": "{precision: day|month|year|decade|century, date?, year?, century?}: as far as \
                                  Wikidata dates it",
                "labels": "the name of every gender, citizenship and occupation id on the page",
            },
        },
        {
            "method": "GET",
            "path": "/index/filter/{type}/people/counts.json",
            "example": "/index/filter/movie/people/counts.json?sel=decade:1980",
            "about": "For every value of every person trait, the people credited under the selection and the \
                      other traits holding it; a one-pick trait (gender, born) counted without its own pick.",
            "parameters": [
                with(
                    param("type", "enum", "movie, series or all (films and series together)"),
                    json!({ "in": "path", "field": "mediaType" }),
                ),
                with(
                    param("sel", "string", "the titles, as counts.json"),
                    json!({ "max": crate::filter::MAX_SELECTION }),
                ),
                with(param("traits", "string", "as people.json"), json!({ "max": crate::filter::MAX_SELECTION })),
            ],
            "returns": "{total, traits: {<trait>: {mode, complete, values: {<id>: n}, labels?, selected?, \
                        excluded?}}, traitCoverage, coverage, ignored, ignoredTraits?, unknownValues?, \
                        unknownTraits?, kindsUnavailable?, traitsUnavailable?}",
            "counts": {
                "total": "people credited on the matching titles and holding every trait",
                "traitCoverage": "per applied trait, the credited people it is on record for (count) out of \
                                  every person credited under the selection and role (denominator)",
            },
        },
        {
            "method": "GET", "path": "/index/taxonomy.json", "example": "/index/taxonomy.json",
            "about": "The label names alone, kept for the TV app. Use this document instead.",
        },
        {
            "method": "POST",
            "path": "/index/labels.json",
            "about": "Each named title's labels, or null.",
            "body": format!("{{titles:[{{type,id}}]}}, at most {MAX_TITLES}"),
            "returns": "{labels}",
        },
        {
            "method": "POST",
            "path": "/index/score.json",
            "about": "Each candidate's closeness to the liked and disliked titles' centroids.",
            "body": format!("{{space?: plot|premise, liked, disliked, candidates}}, each at most {MAX_TITLES} \
                             {{type,id}}"),
            "returns": "{space, scores:[{taste,dislike}]}: cosine, clamped at 0",
        },
        {
            "method": "POST",
            "path": "/index/suggest.json",
            "about": format!("More Like This for up to {MAX_SEEDS} seeds, per seed and pooled in seed order."),
            "body": "{seeds, exclude?, limit?}",
            "returns": "{perSeed:[{seed,ids,mixed:[{type,id}]}], pooled, pooledMixed:[{type,id}]}",
        },
        {
            "method": "POST",
            "path": "/recommend",
            "about": "The titles a featured surface leads with, for one household.",
            "body": "{surface?, service?, now?, services?, library, owned, hide?, candidates?, limit?}",
            "returns": "{slides, unjudged, unjudgedCount, libraryUnjudged, facts, scorer, datasetVersion}",
        },
        {
            "method": "GET",
            "path": "/catalog/{type}/{id}.json",
            "about": "Stremio catalog rows of the streaming services' most popular titles, and title search \
                      as the den-titles catalog (/catalog/{type}/den-titles/search={q}.json).",
            "returns": "{metas}",
        },
        { "method": "GET", "path": "/dataset.json", "about": "What the dataset is: versions, model, count." },
        { "method": "GET", "path": "/health", "about": "Always 200; status ok, or degraded with a reason." },
        { "method": "GET", "path": "/ready", "about": "503 when a whole feature is off." },
    ])
}

/// Coverage attached to a browse-row count. Each filtered field is reported separately because the intersection
/// may not be missing at random, and a single blended percentage would hide which axis is thin.
pub fn row_coverage(indexes: &Indexes, media_type: MediaType, constraints: &[(String, String)]) -> Value {
    let mut fields = Map::new();
    let scoped = population(indexes, Some(media_type));
    for (name, _) in constraints {
        // A row reads every other name as a plot-facet axis, and one the store does not have matches nothing.
        let known = match name.as_str() {
            "subgenre" | "mood" => known(indexes, name, Some(media_type)),
            axis => {
                indexes.plot_facets.as_ref().map_or(0, |facets| facets.coverage_for(axis, Some(media_type)))
            }
        };
        fields.insert(name.clone(), coverage(known, scoped));
    }
    json!({
        "population": indexes.population,
        "mediaType": type_name(media_type),
        "denominator": scoped,
        "fields": fields,
    })
}

/// How a search applied one constraint: `filter`, `discount` or `boost` (`semantics.applied` in the schema).
pub struct Applied {
    pub field: &'static str,
    pub value: Value,
    pub applied: &'static str,
    /// The one media type the constraint judges, when it judges only one: its coverage is out of that type.
    pub applies_to: Option<MediaType>,
}

/// Coverage for a search: the same shape as a row's, with each constraint's value and how it was applied, so a
/// client can tell a filter from a nudge and see how much of the corpus could answer either. Out of the titles
/// of the type asked for, or the corpus when none was.
pub fn query_coverage(indexes: &Indexes, scope: Option<MediaType>, constraints: &[Applied]) -> Value {
    let mut fields = Map::new();
    for constraint in constraints {
        let scope = constraint.applies_to.or(scope);
        let mut entry = coverage(known(indexes, constraint.field, scope), population(indexes, scope));
        entry["value"] = constraint.value.clone();
        entry["applied"] = json!(constraint.applied);
        if let Some(only) = constraint.applies_to {
            entry["appliesTo"] = json!(type_name(only));
        }
        fields.insert(constraint.field.to_owned(), entry);
    }
    json!({
        "population": indexes.population,
        "mediaType": scope.map(type_name),
        "denominator": population(indexes, scope),
        "fields": fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schema_counts_always_name_known_and_population() {
        let dir = std::env::temp_dir().join(format!("den-atlas-schema-{}", std::process::id()));
        let queries = crate::queries::IndexQueries::new(&crate::queries::write_fixture(&dir));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let schema = document(&indexes);
        // The fixture carries 12 titles and 8 of them have no labels at all, so `count` and `denominator`
        // differ — which is the whole point of this endpoint. A client asking "how many heist films" must be
        // told 4 OF 12, never 4, or it reports a fraction of the corpus as though it were all of it.
        assert_eq!(schema["population"]["count"], 12);
        assert_eq!(schema["fields"]["subgenre"]["coverage"]["count"], 4);
        assert_eq!(schema["fields"]["subgenre"]["coverage"]["denominator"], 12);
        assert_eq!(schema["fields"]["mood"]["coverage"]["count"], 1);
        assert_eq!(schema["fields"]["tone"]["coverage"]["count"], 3);
        assert_eq!(schema["fields"]["subgenre"]["values"][0]["count"]["population"], 12);
        assert_eq!(schema["semantics"]["groupBy"], true);
        let route = schema["semantics"]["groupByRoute"].as_str().unwrap();
        assert!(
            schema["routes"].as_array().unwrap().iter().any(|r| r["path"] == route),
            "{route} is a route"
        );
    }

    /// The kinds the schema names as TMDB's are exactly the ones whose values come from `tmdb.rs`, and each says
    /// so where it is described: a client handing answers to a model drops these and nothing else.
    #[test]
    fn the_schema_names_what_is_tmdbs() {
        let tmdb = tmdb();
        assert_eq!(tmdb["filterKinds"], json!(["rating", "character"]));
        let kinds = &crate::filter::schema()["kinds"];
        for (name, kind) in kinds.as_object().unwrap() {
            let named = tmdb["filterKinds"].as_array().unwrap().iter().any(|k| k == name);
            assert_eq!(kind.get("source") == Some(&json!("tmdb")), named, "{name}");
        }
        assert!(tmdb["fields"]["/index/query.json"][0].as_str().unwrap().starts_with("hits[].f.pop"));
    }

    /// The merged display rows are listed under their axis, apart from its values, each with the values it
    /// is of and how many titles carry any of them. The fixture's four titles all end bittersweet.
    #[tokio::test]
    async fn the_schema_lists_the_merged_rows_under_their_axis() {
        let dir = std::env::temp_dir().join(format!("den-atlas-schema-merged-{}", std::process::id()));
        let queries = crate::queries::IndexQueries::new(&crate::queries::write_fixture(&dir));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let schema = document(&indexes);
        let ending = &schema["fields"]["ending"];
        let merged = ending["merged"].as_array().expect("ending lists its merged rows");
        let named: Vec<&str> = merged.iter().map(|m| m["value"].as_str().unwrap()).collect();
        assert_eq!(named, ["unresolved", "unhappy"]);
        let unhappy = &merged[1];
        assert_eq!(unhappy["title"], "Not a happy ending");
        assert_eq!(unhappy["of"], json!(["tragic", "bittersweet"]));
        assert_eq!(unhappy["count"], json!({ "matched": 4, "known": 4, "population": 12 }));
        assert_eq!(merged[0]["count"]["matched"], 0, "no open, ambiguous or cyclical ending in the fixture");
        // Never among the values, whose counts partition the axis.
        let values = ending["values"].as_array().unwrap();
        assert!(values.iter().all(|v| v["value"] != "unhappy" && v["value"] != "unresolved"));
        assert!(schema["fields"]["tone"].get("merged").is_none(), "an axis with no merged row lists none");
    }

    /// An agent reading only this document must find every constraint `/index/query` accepts, in what unit,
    /// and out of how many titles — and each route's parameters must name fields the document describes.
    #[tokio::test]
    async fn the_schema_describes_every_field_and_route_a_client_can_use() {
        let dir = std::env::temp_dir().join(format!("den-atlas-schema-routes-{}", std::process::id()));
        let queries = crate::queries::IndexQueries::new(&crate::queries::write_fixture(&dir));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let schema = document(&indexes);
        let fields = &schema["fields"];

        // Movie 1 alone is on record as adapted, from a book and a play: 1 of 12, and each kind counted.
        assert_eq!(fields["basedOnKind"]["coverage"]["count"], 1);
        assert_eq!(fields["basedOnKind"]["coverage"]["denominator"], 12);
        let kinds: Vec<(&str, u64)> = fields["basedOnKind"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["value"].as_str().unwrap(), v["count"]["matched"].as_u64().unwrap()))
            .collect();
        assert_eq!(kinds, [("book", 1), ("play", 1)]);
        // A broadcaster is a series' fact, so it is out of the 9 series, not the 12 titles.
        assert_eq!(fields["broadcaster"]["coverage"]["denominator"], 9);
        assert_eq!(fields["broadcaster"]["appliesTo"], "series");
        assert_eq!(fields["runtimeMinutes"]["unit"], "minutes");
        assert_eq!(fields["runtimeMinutes"]["coverage"]["denominator"], 12);
        assert_eq!(fields["year"]["unit"], "calendar year");
        assert_eq!(fields["country"]["format"], "ISO 3166-1 alpha-2");
        // Crime (80) is movie 1's alone, from its facts.
        let crime =
            fields["genre"]["values"].as_array().unwrap().iter().find(|v| v["value"] == "80").unwrap();
        assert_eq!(crime["count"]["matched"], 1);
        assert!(schema["semantics"]["applied"]["filter"].is_string());

        let routes = schema["routes"].as_array().unwrap();
        let query = routes.iter().find(|r| r["path"] == "/index/query.json").expect("the query route");
        let names: Vec<&str> =
            query["parameters"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
        for name in
            ["q", "type", "skip", "limit", "year_min", "year_max", "language", "runtime_max", "broadcaster"]
        {
            assert!(names.contains(&name), "/index/query.json does not declare {name}");
        }
        for route in routes {
            for parameter in route["parameters"].as_array().into_iter().flatten() {
                if let Some(field) = parameter["field"].as_str().filter(|f| !f.contains(' ')) {
                    assert!(
                        fields.get(field).is_some(),
                        "{} names an undescribed field {field}",
                        route["path"]
                    );
                }
            }
        }
    }
}
