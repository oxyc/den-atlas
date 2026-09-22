//! `GET /index/facets/<movie|series>.json?sel=<kind>:<id>,…` — for a selection of facets, how many corpus
//! titles of the type carry the selection AND each value of every facet kind. Den Web's stackable Search
//! filters hide an option whose count is 0, instead of spending TMDB discover calls to find out
//! (oxyc/den#133).
//!
//! # What a count is
//!
//! Titles on record as carrying every selected value and the counted one. A title the corpus does not
//! describe is unknown, not a negative, so a count is a floor on the corpus and says nothing about TMDB's
//! long tail: 0 means "nothing Den would show you", which is what hiding needs. The facts' countries and
//! languages are counted under EVERY value a title lists, not only the first, so a count errs high rather
//! than hiding an option a title does carry.
//!
//! # Why bitsets
//!
//! One bit per store row for every value of every kind, built once per index load. A request is then an AND
//! of the selection's bitsets and a popcount per value — a few hundred values over ~750 words each — so it
//! costs well under a millisecond and nothing is scanned per call.
//!
//! # The URL is the cache key
//!
//! The answer depends on the type and the selection alone, so identical selections must be the same URL.
//! `Selection::parse` says whether a request is already in canonical form, and the handler redirects one that
//! is not.

use crate::queries::Indexes;
use den_index::MediaType;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

/// The most values one selection may name.
pub const MAX_SELECTION: usize = 16;
/// The longest query string accepted, in bytes: sixteen values with room for long labels.
pub const MAX_QUERY: usize = 2048;

/// The facet kinds besides the plot-facet axes, each of which is a kind of its own (`tone:bleak`).
const KINDS: [&str; 6] = ["country", "decade", "genre", "language", "mood", "subgenre"];

/// TMDB's series-only genres that name two film genres at once (Action & Adventure, Sci-Fi & Fantasy,
/// War & Politics).
const SERIES_COMPOSITE_GENRES: [u16; 3] = [10759, 10765, 10768];

/// A year before this is unknown, as the facet index reads it.
const FIRST_YEAR: i64 = 1870;

/// Every value of every kind the store could answer, as the titles carrying it.
pub struct FacetCounts {
    /// The titles of each type.
    movie: Vec<u64>,
    tv: Vec<u64>,
    /// kind → value → the titles carrying it. Sorted, so an answer's bytes — and its ETag — never vary.
    /// A kind is present only when its source loaded; one that did not is absent from every answer.
    kinds: BTreeMap<&'static str, BTreeMap<String, Vec<u64>>>,
}

impl FacetCounts {
    /// Every kind's values over the store's rows.
    ///
    /// Genre, country and language come from the facts, so they are present only when the facts loaded: the
    /// labels alone name one genre a title, and counting from them would hide options the facts would show.
    /// Decade reads the card's year, else the facts' release year. Mood and subgenre are the labels at the
    /// display floor `/index/row` uses; the plot axes are the facet rows' values at theirs.
    pub fn build(indexes: &Indexes) -> FacetCounts {
        let view = indexes.store.view();
        let keys = view.per_row::<u64>("keys").unwrap_or(&[]);
        let words = keys.len().div_ceil(64);
        let bits = || vec![0u64; words];
        let (mut movie, mut tv) = (bits(), bits());
        let mut kinds: BTreeMap<&'static str, BTreeMap<String, Vec<u64>>> = BTreeMap::new();
        let set = |kinds: &mut BTreeMap<&'static str, BTreeMap<String, Vec<u64>>>,
                   kind: &'static str,
                   value: String,
                   row: usize| {
            kinds.entry(kind).or_default().entry(value).or_insert_with(bits)[row / 64] |= 1 << (row % 64);
        };
        let facts = indexes.facts.as_ref();
        if facts.is_some() {
            for kind in ["country", "genre", "language"] {
                kinds.insert(kind, BTreeMap::new());
            }
        }
        if facts.is_some() || indexes.cards.is_some() {
            kinds.insert("decade", BTreeMap::new());
        }
        kinds.insert("mood", BTreeMap::new());
        kinds.insert("subgenre", BTreeMap::new());

        let mut rows: HashMap<(MediaType, u32), usize> = HashMap::with_capacity(keys.len());
        let floor = den_index::DISPLAY_CONFIDENCE_FLOOR;
        for (row, &packed) in keys.iter().enumerate() {
            let media_type = if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie };
            let id = packed as u32;
            let key = (media_type, id);
            rows.insert(key, row);
            let of_type = if media_type == MediaType::Tv { &mut tv } else { &mut movie };
            of_type[row / 64] |= 1 << (row % 64);

            let record = facts.and_then(|f| f.get(id, media_type));
            if facts.is_some() {
                let genres = crate::plotrows::genres(indexes, key);
                // The facts name a series' genres as films' (`recommend::fold_genre`), but TMDB's series
                // filters use its composite ids, so a series answers under those too.
                let composites = SERIES_COMPOSITE_GENRES.iter().filter(|&&composite| {
                    media_type == MediaType::Tv
                        && crate::recommend::fold_genre(composite).iter().any(|g| genres.contains(g))
                });
                for genre in genres.iter().chain(composites) {
                    set(&mut kinds, "genre", genre.to_string(), row);
                }
            }
            if let Some(record) = record {
                for code in &record.countries {
                    set(&mut kinds, "country", String::from_utf8_lossy(code).to_ascii_uppercase(), row);
                }
                for code in &record.languages {
                    set(&mut kinds, "language", String::from_utf8_lossy(code).to_ascii_lowercase(), row);
                }
            }
            let year = indexes
                .cards
                .as_ref()
                .and_then(|cards| cards.get(&key))
                .and_then(|card| card.year)
                .or_else(|| record.and_then(|r| r.released).map(|r| r.year_of()));
            if let Some(year) = year.filter(|&y| y >= FIRST_YEAR) {
                set(&mut kinds, "decade", (year.div_euclid(10) * 10).to_string(), row);
            }
            if let Some(labels) = indexes.plot.labels(id, media_type) {
                for (kind, pairs) in [("mood", &labels.moods), ("subgenre", &labels.subgenres)] {
                    for &(name, confidence) in pairs {
                        if confidence >= floor {
                            set(&mut kinds, kind, name.to_owned(), row);
                        }
                    }
                }
            }
        }
        if let Some(plot_facets) = &indexes.plot_facets {
            for &axis in den_store::FACET_AXES.iter() {
                kinds.insert(axis, BTreeMap::new());
            }
            for (axis, value, titles) in plot_facets.values() {
                let Some(axis) = den_store::FACET_AXES.iter().copied().find(|a| *a == axis) else { continue };
                for (key, _) in titles {
                    if let Some(&row) = rows.get(key) {
                        set(&mut kinds, axis, value.to_owned(), row);
                    }
                }
            }
        }
        FacetCounts { movie, tv, kinds }
    }

    /// Per kind, every value with a count above 0 among the titles of `media_type` carrying the selection.
    /// A value missing from a kind that is present counts 0. A selection naming a kind this load lacks can
    /// say nothing about anything, so every kind is absent then.
    pub fn answer(&self, media_type: MediaType, selection: &[(&'static str, String)]) -> Value {
        let mut matched = if media_type == MediaType::Tv { self.tv.clone() } else { self.movie.clone() };
        for (kind, value) in selection {
            let Some(values) = self.kinds.get(kind) else { return Value::Object(Map::new()) };
            match values.get(value) {
                Some(bits) => matched.iter_mut().zip(bits).for_each(|(m, b)| *m &= b),
                None => matched.fill(0),
            }
        }
        let any = matched.iter().any(|&w| w != 0);
        let mut answer = Map::new();
        for (kind, values) in &self.kinds {
            let mut counts = Map::new();
            if any {
                for (value, bits) in values {
                    let n: u32 = matched.iter().zip(bits).map(|(m, b)| (m & b).count_ones()).sum();
                    if n > 0 {
                        counts.insert(value.clone(), n.into());
                    }
                }
            }
            answer.insert((*kind).to_owned(), Value::Object(counts));
        }
        Value::Object(answer)
    }
}

/// A request's `sel`, read and checked before anything loads.
#[derive(Debug, PartialEq)]
pub struct Selection {
    /// The values, sorted by kind then id and de-duplicated.
    pub items: Vec<(&'static str, String)>,
    /// Whether the request already spelled exactly `items`, and nothing else, so its URL is the one every
    /// identical selection shares. `false` means redirect to `location`.
    pub canonical: bool,
}

impl Selection {
    /// `sel=<kind>:<id>,…` out of a query string. The value may arrive percent-encoded as a whole (a
    /// browser's URLSearchParams writes `genre%3A28%2Ccountry%3ASE`) or per id; both decode to the same.
    ///
    /// Canonical is: no parameter but `sel`, no `sel` at all for an empty selection, the kinds' own names,
    /// ids normalised (genre and decade as plain integers, a decade by its first year; language lowercase,
    /// country uppercase; a plot value lowercase; `structure` as the axis that answers it), sorted by kind
    /// and then id as strings, each once. The error is a malformed or oversized selection.
    pub fn parse(query: &str) -> Result<Selection, String> {
        if query.len() > MAX_QUERY {
            return Err(format!("a query of at most {MAX_QUERY} bytes"));
        }
        let mut raw_sel = None;
        let mut others = false;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("sel", value)) if raw_sel.is_none() => raw_sel = Some(value),
                _ => others = true,
            }
        }
        let decoded =
            raw_sel.map(|v| crate::handler::percent_decode(&v.replace('+', " "))).unwrap_or_default();
        let raw: Vec<&str> = if decoded.is_empty() { Vec::new() } else { decoded.split(',').collect() };
        if raw.len() > MAX_SELECTION {
            return Err(format!("at most {MAX_SELECTION} selected values"));
        }
        let mut items = Vec::with_capacity(raw.len());
        for item in raw.iter().filter(|i| !i.is_empty()) {
            let (kind, id) = item.split_once(':').ok_or_else(|| format!("{item:?} is not <kind>:<id>"))?;
            items.push(normalise(kind, id)?);
        }
        items.sort();
        items.dedup();
        let spelled: Vec<String> = items.iter().map(|(kind, id)| format!("{kind}:{id}")).collect();
        // An empty selection is spelled with no `sel` at all.
        let empty_sel = raw_sel.is_some() && items.is_empty();
        let canonical = !others && !empty_sel && raw == spelled;
        Ok(Selection { items, canonical })
    }

    /// The canonical URL's query, `?sel=…`, or nothing for an empty selection: what a redirect appends to the
    /// path. Ids are percent-encoded; the `:` and `,` between them are not.
    pub fn query(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let items: Vec<String> =
            self.items.iter().map(|(kind, id)| format!("{kind}:{}", encode(id))).collect();
        format!("?sel={}", items.join(","))
    }
}

/// One `<kind>:<id>` as its canonical pair.
fn normalise(kind: &str, id: &str) -> Result<(&'static str, String), String> {
    let kind = kind.trim().to_ascii_lowercase();
    let id = id.trim();
    if id.is_empty() {
        return Err(format!("{kind}: an empty id"));
    }
    let number = || id.parse::<u32>().map_err(|_| format!("{kind}: {id:?} is not an integer"));
    if let Some(&name) = KINDS.iter().find(|k| **k == kind) {
        let id = match name {
            "genre" => number()?.to_string(),
            "decade" => (number()? / 10 * 10).to_string(),
            "language" => id.to_ascii_lowercase(),
            "country" => id.to_ascii_uppercase(),
            _ => id.to_owned(),
        };
        return Ok((name, id));
    }
    let id = id.to_ascii_lowercase();
    let axis = crate::plotrows::resolve_axis(&kind, &id);
    match den_store::FACET_AXES.iter().copied().find(|a| *a == axis) {
        Some(axis) => Ok((axis, id)),
        None => Err(format!("unknown kind {kind:?}")),
    }
}

/// Everything but the unreserved characters, percent-encoded.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use den_index::MediaType::{Movie, Tv};
    use serde_json::json;

    fn fixture(name: &str) -> Indexes {
        let dir = std::env::temp_dir().join(format!("den-atlas-facetcounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::queries::load_for_tools(&crate::queries::write_fixture(&dir)).expect("the fixture loads")
    }

    fn sel(query: &str) -> Vec<(&'static str, String)> {
        Selection::parse(query).expect("a well-formed selection").items
    }

    /// Movies 1–3 of the fixture: 1 is Korean and Danish from 1985 with genres 80 and 18, Tense and a Heist;
    /// 2 is Korean from 1995, a Heist; 3 is Spanish from 1985, a Heist and Campy/Cult.
    #[test]
    fn counts_titles_carrying_the_selection_and_each_value() {
        let counts = FacetCounts::build(&fixture("and"));

        let all = counts.answer(Movie, &[]);
        assert_eq!(all["country"], json!({ "DK": 1, "ES": 1, "KR": 2 }), "every country a title lists");
        assert_eq!(all["decade"], json!({ "1980": 2, "1990": 1 }), "the card's year, not the facts' 2026");
        assert_eq!(all["subgenre"], json!({ "Campy/Cult": 1, "Heist": 3 }));
        assert_eq!(all["mood"], json!({ "Tense": 1 }));
        assert_eq!(all["genre"]["80"], json!(1));
        assert_eq!(all["ending"], json!({ "bittersweet": 3 }));

        let korean = counts.answer(Movie, &sel("sel=country:KR"));
        assert_eq!(korean["country"], json!({ "DK": 1, "KR": 2 }), "Spain has nothing left: absent, i.e. 0");
        assert_eq!(korean["decade"], json!({ "1980": 1, "1990": 1 }));
        assert_eq!(korean["subgenre"], json!({ "Heist": 2 }));

        let stacked = counts.answer(Movie, &sel("sel=country:KR,decade:1980"));
        assert_eq!(stacked["mood"], json!({ "Tense": 1 }));
        assert_eq!(stacked["decade"], json!({ "1980": 1 }));

        let plot = counts.answer(Movie, &sel("sel=tone:bleak"));
        assert_eq!(plot["country"], json!({ "DK": 1, "KR": 2 }), "movies 1 and 2 are bleak");

        // The type is part of the selection: the one Korean series is not among the movies.
        assert_eq!(counts.answer(Tv, &sel("sel=country:KR"))["country"], json!({ "KR": 1 }));
    }

    /// A value the corpus does not hold is a selection nothing carries: every kind is present and empty.
    #[test]
    fn an_unknown_id_counts_nothing() {
        let counts = FacetCounts::build(&fixture("unknown"));
        for query in ["sel=genre:9999", "sel=mood:Nope", "sel=tone:sunny", "sel=country:ZZ"] {
            let answer = counts.answer(Movie, &sel(query));
            assert_eq!(answer["country"], json!({}), "{query}");
            assert_eq!(answer["subgenre"], json!({}), "{query}");
        }
    }

    /// A kind whose source did not load is absent, never a column of zeros: the web reads a missing kind as
    /// "can't say" and hides nothing, where zeros would hide every option.
    #[test]
    fn a_kind_the_load_lacks_is_absent() {
        let mut indexes = fixture("absent");
        indexes.facts = None;
        indexes.plot_facets = None;
        let answer = FacetCounts::build(&indexes).answer(Movie, &[]);
        for kind in ["genre", "country", "language", "tone", "ending"] {
            assert!(answer.get(kind).is_none(), "{kind} is present without its source");
        }
        assert_eq!(answer["decade"], json!({ "1980": 2, "1990": 1 }), "the cards still date them");
        assert_eq!(answer["subgenre"]["Heist"], json!(3));
        // And a selection naming one can say nothing about anything.
        let answer = FacetCounts::build(&indexes).answer(Movie, &sel("sel=country:KR"));
        assert_eq!(answer, json!({}));
    }

    #[test]
    fn a_selection_is_canonical_only_when_sorted_unique_and_normalised() {
        let canonical = |q: &str| Selection::parse(q).unwrap().canonical;
        assert!(canonical(""), "no selection, no query");
        assert!(canonical("sel=country:SE,genre:28"));
        assert!(canonical("sel=country%3ASE%2Cgenre%3A28"), "encoded whole, as URLSearchParams writes it");
        assert!(canonical("sel=subgenre:Campy%2FCult"));
        assert!(!canonical("sel=genre:28,country:SE"), "unsorted");
        assert!(!canonical("sel=genre:28,genre:28"), "repeated");
        assert!(!canonical("sel=country:se"), "country is uppercase");
        assert!(!canonical("sel=language:SV"), "language is lowercase");
        assert!(!canonical("sel=decade:1995"), "a decade is its first year");
        assert!(!canonical("sel=genre:028"));
        assert!(!canonical("sel=structure:single-day"), "the alias names another axis");
        assert!(!canonical("sel="), "an empty selection has no sel");
        assert!(!canonical("sel=genre:28&x=1"), "nothing but sel");

        let parsed =
            Selection::parse("sel=structure:single-day,language:SV,decade:1995,genre:28,genre:28").unwrap();
        assert_eq!(
            parsed.items,
            vec![
                ("decade", "1990".to_owned()),
                ("genre", "28".to_owned()),
                ("language", "sv".to_owned()),
                ("timespan", "single-day".to_owned()),
            ]
        );
        assert_eq!(parsed.query(), "?sel=decade:1990,genre:28,language:sv,timespan:single-day");
        assert_eq!(
            Selection::parse("sel=subgenre:Campy/Cult").unwrap().query(),
            "?sel=subgenre:Campy%2FCult"
        );
        assert_eq!(Selection::parse("sel=").unwrap().query(), "");
        // The redirect's own target is canonical, so it can never loop.
        assert!(canonical(&parsed.query()[1..]));
    }

    #[test]
    fn malformed_and_oversized_selections_are_refused() {
        for query in ["sel=genre", "sel=nope:1", "sel=genre:action", "sel=decade:199x", "sel=genre:"] {
            assert!(Selection::parse(query).is_err(), "{query}");
        }
        let many: Vec<String> = (0..=MAX_SELECTION).map(|g| format!("genre:{g}")).collect();
        assert!(Selection::parse(&format!("sel={}", many.join(","))).is_err(), "over the selection cap");
        let most: Vec<String> = (0..MAX_SELECTION).map(|g| format!("genre:{g}")).collect();
        assert!(Selection::parse(&format!("sel={}", most.join(","))).is_ok(), "at the cap");
        let long = format!("sel=mood:{}", "a".repeat(MAX_QUERY));
        assert!(Selection::parse(&long).is_err(), "over the length cap");
    }

    /// The counts over the REAL corpus, and what they cost. Opt-in, like every test that needs it:
    /// `DEN_STORE` names a store whose directory holds its `dataset.meta.json`.
    #[test]
    fn real_corpus_counts_and_timing() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to measure this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let indexes = crate::queries::load_for_tools(&ds).expect("the indexes load");
        let started = std::time::Instant::now();
        let counts = FacetCounts::build(&indexes);
        let built = started.elapsed();
        let values: usize = counts.kinds.values().map(BTreeMap::len).sum();
        let bytes = values * counts.movie.len() * 8;
        eprintln!(
            "built {values} values over {} kinds in {built:?}, {} KB of bitsets",
            counts.kinds.len(),
            bytes / 1024
        );
        for query in ["", "sel=genre:28", "sel=country:SE,genre:28", "sel=country:SE,decade:1990,genre:28"] {
            let selection = sel(query);
            let rounds = 200;
            let started = std::time::Instant::now();
            let mut body = String::new();
            for _ in 0..rounds {
                body = counts.answer(Movie, &selection).to_string();
            }
            eprintln!(
                "{query:?}: {:?} a request (answer + JSON), {} bytes",
                started.elapsed() / rounds,
                body.len()
            );
        }
        for (kind, values) in &counts.kinds {
            eprintln!("  {kind}: {} values", values.len());
            // `sel` separates values with `,` and a kind from its id with the first `:`, so an id may hold a
            // `:` but never a `,`.
            assert!(values.keys().all(|value| !value.contains(',')), "a {kind} value holds a comma");
        }
        assert!(values > 100, "the real corpus has hundreds of values");
        let series = counts.answer(Tv, &[]);
        eprintln!("series, no selection: {} bytes", series.to_string().len());
        for composite in SERIES_COMPOSITE_GENRES {
            assert!(series["genre"].get(composite.to_string()).is_some(), "series answer under {composite}");
            let movies = counts.answer(Movie, &[]);
            assert!(movies["genre"].get(composite.to_string()).is_none(), "films never carry {composite}");
        }
    }
}
