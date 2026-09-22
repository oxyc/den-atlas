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
    /// Each store row's title, so a bit can be named.
    keys: Vec<(MediaType, u32)>,
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
            // The merged display rows `/index/row` answers (`ending:unhappy`), as the union of their members.
            for merged in crate::plotrows::MERGED_ROWS {
                let Some(values) = kinds.get_mut(merged.axis) else { continue };
                let mut union = bits();
                for member in merged.members.iter().filter_map(|m| values.get(*m)) {
                    union.iter_mut().zip(member).for_each(|(u, m)| *u |= m);
                }
                values.insert(merged.value.to_owned(), union);
            }
        }
        let keys = keys
            .iter()
            .map(|&packed| {
                (if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie }, packed as u32)
            })
            .collect();
        FacetCounts { movie, tv, keys, kinds }
    }

    /// Per kind, every value with a count above 0 among the titles of `media_type` carrying the selection.
    /// A value missing from a kind that is present counts 0. A selection naming a kind this load lacks can
    /// say nothing about anything, so every kind is absent then.
    pub fn answer(&self, media_type: MediaType, selection: &[(&'static str, String)]) -> Value {
        let Some(matched) = self.selected(media_type, selection) else { return Value::Object(Map::new()) };
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

    /// The titles of `media_type` carrying every selected value, in store order; `None` when the selection
    /// names a kind this load lacks.
    pub fn matching(
        &self,
        media_type: MediaType,
        selection: &[(&'static str, String)],
    ) -> Option<Vec<(MediaType, u32)>> {
        let matched = self.selected(media_type, selection)?;
        let mut keys = Vec::new();
        for (word, &bits) in matched.iter().enumerate() {
            let mut rest = bits;
            while rest != 0 {
                keys.push(self.keys[word * 64 + rest.trailing_zeros() as usize]);
                rest &= rest - 1;
            }
        }
        Some(keys)
    }

    /// The selection's bitset over the titles of `media_type`; `None` when it names a kind this load lacks.
    fn selected(&self, media_type: MediaType, selection: &[(&'static str, String)]) -> Option<Vec<u64>> {
        let mut matched = if media_type == MediaType::Tv { self.tv.clone() } else { self.movie.clone() };
        for (kind, value) in selection {
            match self.kinds.get(kind)?.get(value) {
                Some(bits) => matched.iter_mut().zip(bits).for_each(|(m, b)| *m &= b),
                None => matched.fill(0),
            }
        }
        Some(matched)
    }
}

/// `GET /index/browse/<type>.json`: the titles of `media_type` carrying every selected value, most voted
/// first (the order `/index/row` falls back to after confidence), `skip` then `limit` of them, each as the
/// card `/index/row` draws, and how many there are. An empty selection is every title of the type. A title
/// with no card is left out, as a row leaves it out; so is every title when the selection names a kind this
/// load lacks, since nothing can be said to carry it.
///
/// The order is worked out once per type and selection (`Indexes::row_order`), so every page is a slice of
/// one order and a scroll neither repeats nor skips.
pub fn browse(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    media_type: MediaType,
    selection: &[(&'static str, String)],
    skip: usize,
    limit: usize,
) -> Value {
    let Some(cards) = indexes.cards.as_ref() else {
        return serde_json::json!({ "titles": [], "total": 0 });
    };
    let spelled: Vec<String> = selection.iter().map(|(kind, id)| format!("{kind}:{id}")).collect();
    let kind = if media_type == MediaType::Tv { "tv" } else { "movie" };
    let order = indexes.row_order(format!("browse:{kind}?{}", spelled.join(",")), || {
        let Some(keys) = indexes.facet_counts().matching(media_type, selection) else { return Vec::new() };
        let mut ranked: Vec<((MediaType, u32), f64)> = keys
            .into_iter()
            .filter(|key| cards.contains_key(key))
            .map(|key| (key, crate::plotrows::popularity(indexes, export, key)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0 .1.cmp(&b.0 .1)));
        // A store that repeats a title would list it twice.
        ranked.dedup_by_key(|(key, _)| *key);
        ranked.into_iter().map(|(key, _)| key).collect()
    });
    let titles: Vec<Value> = order
        .iter()
        .skip(skip)
        .take(limit)
        .map(|&key| crate::plotrows::title_json(indexes, key, &cards[&key]))
        .collect();
    serde_json::json!({ "titles": titles, "total": order.len() })
}

/// A request's `sel`, read and checked before anything loads.
#[derive(Debug, Default, PartialEq)]
pub struct Selection {
    /// The values, sorted by kind then id and de-duplicated.
    pub items: Vec<(&'static str, String)>,
    /// A browse page: titles to skip, and how many to return (`parse_paged`). A facet count takes neither,
    /// and has 0 and `PAGE` here.
    pub skip: usize,
    pub limit: usize,
    /// Whether the request already spelled exactly this, and nothing else, so its URL is the one every
    /// identical request shares. `false` means redirect to `query`.
    pub canonical: bool,
}

/// A browse page's size when the request names none, and the most one page returns.
pub const PAGE: usize = crate::handler::ROW_PAGE;
pub const MAX_PAGE: usize = crate::handler::MAX_ROW_PAGE;

impl Selection {
    /// `sel=<kind>:<id>,…` out of a query string. The value may arrive percent-encoded as a whole (a
    /// browser's URLSearchParams writes `genre%3A28%2Ccountry%3ASE`) or per id; both decode to the same.
    ///
    /// Canonical is: no parameter but `sel`, no `sel` at all for an empty selection, the kinds' own names,
    /// ids normalised (genre and decade as plain integers, a decade by its first year; language lowercase,
    /// country uppercase; a plot value lowercase; `structure` as the axis that answers it), sorted by kind
    /// and then id as strings, each once. The error is a malformed or oversized selection.
    pub fn parse(query: &str) -> Result<Selection, String> {
        Self::read(query, false)
    }

    /// `parse`, plus `skip` and `limit`, which follow `sel` in that order, each only when it is not its
    /// default (0, and `PAGE`), as a plain decimal. A `limit` over `MAX_PAGE` or under 1 is redirected to
    /// the nearest one allowed; one that is not a number is refused.
    pub fn parse_paged(query: &str) -> Result<Selection, String> {
        Self::read(query, true)
    }

    fn read(query: &str, paged: bool) -> Result<Selection, String> {
        if query.len() > MAX_QUERY {
            return Err(format!("a query of at most {MAX_QUERY} bytes"));
        }
        let (mut raw_sel, mut raw_skip, mut raw_limit) = (None, None, None);
        let mut others = false;
        let mut names = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            names.push(name);
            match name {
                "sel" if raw_sel.is_none() => raw_sel = Some(value),
                "skip" if paged && raw_skip.is_none() => raw_skip = Some(value),
                "limit" if paged && raw_limit.is_none() => raw_limit = Some(value),
                _ => others = true,
            }
        }
        let number = |name: &str, raw: Option<&str>, default: usize| {
            raw.map_or(Ok(default), |v| {
                v.parse::<usize>().map_err(|_| format!("{name}: {v:?} is not a count"))
            })
        };
        let skip = number("skip", raw_skip, 0)?;
        let asked = number("limit", raw_limit, PAGE)?;
        let limit = asked.clamp(1, MAX_PAGE);
        let order: Vec<&str> = [("sel", raw_sel), ("skip", raw_skip), ("limit", raw_limit)]
            .iter()
            .filter(|(_, raw)| raw.is_some())
            .map(|(name, _)| *name)
            .collect();
        let paging_canonical = raw_skip.is_none_or(|v| skip != 0 && v == skip.to_string())
            && raw_limit.is_none_or(|v| limit == asked && limit != PAGE && v == limit.to_string())
            && names == order;
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
        let canonical = !others && !empty_sel && raw == spelled && paging_canonical;
        Ok(Selection { items, skip, limit, canonical })
    }

    /// The canonical URL's query — `?sel=…&skip=…&limit=…`, each part only when it is not its default, or
    /// nothing at all: what a redirect appends to the path. Ids are percent-encoded; the `:` and `,` between
    /// them are not.
    pub fn query(&self) -> String {
        let mut parts = Vec::new();
        if !self.items.is_empty() {
            let items: Vec<String> =
                self.items.iter().map(|(kind, id)| format!("{kind}:{}", encode(id))).collect();
            parts.push(format!("sel={}", items.join(",")));
        }
        if self.skip != 0 {
            parts.push(format!("skip={}", self.skip));
        }
        if self.limit != PAGE {
            parts.push(format!("limit={}", self.limit));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
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
        assert_eq!(all["ending"], json!({ "bittersweet": 3, "unhappy": 3 }), "a merged row is its members");
        assert_eq!(all["tone"], json!({ "bleak": 2, "comic": 1 }));

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

    /// Browsing lists the titles carrying EVERY selected value — two genres are both genres — most voted
    /// first (movie 2 has 500 votes, 1 has 100, 3 has 50), paged, with the whole count beside the page.
    #[test]
    fn browse_lists_titles_carrying_every_value_most_voted_first() {
        let indexes = fixture("browse");
        let ids = |answer: &Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };
        let page = |query: &str, skip, limit| browse(&indexes, None, Movie, &sel(query), skip, limit);

        let top = page("", 0, PAGE);
        assert_eq!((ids(&top), &top["total"]), (vec![2, 1, 3], &json!(3)), "no selection is every title");
        assert_eq!(ids(&page("sel=country:KR", 0, PAGE)), vec![2, 1]);
        assert_eq!(ids(&page("sel=country:KR,decade:1980", 0, PAGE)), vec![1]);
        assert_eq!(ids(&page("sel=genre:18", 0, PAGE)), vec![2, 1]);
        assert_eq!(ids(&page("sel=genre:18,genre:80", 0, PAGE)), vec![1], "genres AND");
        assert_eq!(ids(&page("sel=genre:18,genre:35", 0, PAGE)), Vec::<u64>::new());
        assert_eq!(ids(&page("sel=mood:Tense,tone:bleak", 0, PAGE)), vec![1], "labels AND plot axes");
        assert_eq!(
            ids(&page("sel=ending:unhappy,tone:comic", 0, PAGE)),
            vec![3],
            "a merged row, as /index/row"
        );

        let second = page("sel=subgenre:Heist", 1, 1);
        assert_eq!((ids(&second), &second["total"]), (vec![1], &json!(3)), "a page, and the whole count");
        let past = page("sel=subgenre:Heist", 9, 1);
        assert_eq!((ids(&past), &past["total"]), (vec![], &json!(3)));

        let series = browse(&indexes, None, Tv, &sel("sel=country:KR"), 0, PAGE);
        assert_eq!(ids(&series), vec![4], "the type is part of the selection");

        let mut lacking = fixture("browse-lacking");
        lacking.facts = None;
        // Built again: the load built its own before the facts were taken away.
        let counts = FacetCounts::build(&lacking);
        assert_eq!(counts.matching(Movie, &sel("sel=country:KR")), None, "a kind the load lacks");
        assert_eq!(counts.matching(Movie, &sel("sel=subgenre:Heist")).map(|keys| keys.len()), Some(3));
    }

    #[test]
    fn a_page_is_canonical_only_with_its_defaults_left_out_and_in_order() {
        let paged = |q: &str| Selection::parse_paged(q).unwrap();
        for query in ["", "sel=genre:28", "sel=genre:28&skip=24", "sel=genre:28&skip=24&limit=40", "limit=40"]
        {
            assert!(paged(query).canonical, "{query}");
        }
        for query in [
            "sel=genre:28&skip=0",
            "sel=genre:28&limit=24",
            "limit=40&sel=genre:28",
            "sel=genre:28&limit=500",
            "sel=genre:28&limit=0",
            "sel=genre:28&skip=024",
        ] {
            assert!(!paged(query).canonical, "{query}");
        }
        let capped = paged("limit=500&sel=genre:28,country:se&skip=0");
        assert_eq!((capped.skip, capped.limit), (0, MAX_PAGE));
        assert_eq!(capped.query(), format!("?sel=country:SE,genre:28&limit={MAX_PAGE}"));
        assert!(paged(&capped.query()[1..]).canonical, "the redirect's target is canonical");
        assert_eq!(paged("limit=0").limit, 1);
        assert!(Selection::parse_paged("skip=x").is_err());
        assert!(Selection::parse_paged("limit=-1").is_err());
        // Paging is a browse parameter only: on the facet counts it is a parameter to redirect away.
        assert!(!Selection::parse("sel=genre:28&skip=24").unwrap().canonical);
        assert_eq!(Selection::parse("sel=genre:28&skip=24").unwrap().query(), "?sel=genre:28");
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
        for query in [
            "",
            "sel=genre:28",
            "sel=country:US,decade:1990,genre:28",
            "sel=country:KR,decade:2000,tone:bleak",
        ] {
            let selection = sel(query);
            let started = std::time::Instant::now();
            let first = browse(&indexes, None, Movie, &selection, 0, 40).to_string();
            let cold = started.elapsed();
            let started = std::time::Instant::now();
            let next = browse(&indexes, None, Movie, &selection, 40, 40).to_string();
            eprintln!(
                "browse {query:?}: first page {cold:?} (orders the selection), next page {:?}; limit=40 is {} \
                 bytes of {} titles",
                started.elapsed(),
                first.len(),
                serde_json::from_str::<Value>(&next).unwrap()["total"]
            );
        }
        let series = counts.answer(Tv, &[]);
        eprintln!("series, no selection: {} bytes", series.to_string().len());
        for composite in SERIES_COMPOSITE_GENRES {
            assert!(series["genre"].get(composite.to_string()).is_some(), "series answer under {composite}");
            let movies = counts.answer(Movie, &[]);
            assert!(movies["genre"].get(composite.to_string()).is_none(), "films never carry {composite}");
        }
    }
}
