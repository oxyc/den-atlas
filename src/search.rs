//! `GET /index/query.json?q=&type=&skip=&limit=` — search in one request, so no client fuses lanes of its own.
//!
//! The query is read for what it names — a country, a decade, a type, a genre, one of atlas's labels, a plot facet —
//! and the words left over. Every lane then only proposes titles: a fuzzy title match (TMDB's export titles and the
//! display titles atlas draws with), the facet's titles, a label's or plot facet's titles, and the plot vectors'
//! nearest to the leftover. Every candidate is scored on every signal, whichever lane found it:
//!
//! `S = Φ · [2.0·T + w_sem·Sem + 0.25·L + 0.10·PF + 0.15·Pop·R]`, `w_sem = 0.6·(0.3 + 0.7·λ)·(1 − 0.7·exact)`
//!
//! A title match is weighted so that an exact title (T ≥ 0.6, so ≥ 1.2) always beats a match on theme alone (at
//! most 0.6 + 0.25 + 0.10 + 0.15 = 1.10); the plot vectors count for more the more of the query is left over (λ),
//! and far less once an exact title answered, since they hold no titles and would only add what sounds alike.
//! A near title match counts only for the leftover's share (T·λ): a query of words atlas reads ("bleak") asks for
//! the theme, not for "Leak" or "Bleach". Popularity counts only as far as the title is relevant at all (R, the
//! strongest of its other signals), so a famous title that merely sounds alike doesn't pass a closer one.
//! A candidate relevant to nothing is dropped. The weights are starting values, to be tuned against a judged
//! query set.

use crate::queries::Indexes;
use den_index::{FacetQuery, MediaType};
use den_titlesearch::{fold, trigram_keys, TitleIndex};
use std::collections::{HashMap, HashSet};

type Key = (MediaType, u32);

const W_TITLE: f64 = 2.0;
const W_LABEL: f64 = 0.25;
const W_PLOT_FACET: f64 = 0.10;
const W_POPULARITY: f64 = 0.15;
const W_SEMANTIC: f64 = 0.6;
/// How far above the scan's mean (in standard deviations) a vector match must stand to count at all, and the span
/// over which it rises to full weight: the nearest few dozen of any query stand well out, so a raw rank says
/// nothing about whether they mean anything.
const SEMANTIC_FLOOR_Z: f64 = 2.5;
const SEMANTIC_SPAN_Z: f64 = 3.5;
/// What a candidate keeps when the query names a country or decade it has no record of: unknown isn't a mismatch.
const UNKNOWN_FACET: f64 = 0.6;
/// A title typed exactly scores this plus popularity's share; a near match at most `FUZZY_CAP`.
const EXACT_TITLE: f64 = 0.6;
const FUZZY_CAP: f64 = 0.5;
/// The share of the query's trigrams a title must hold to count as a near match (a one-letter typo).
const MIN_COVERAGE: f64 = 0.6;
/// An exact title this popular shuts the vectors mostly out.
const EXACT_POPULAR: f64 = 0.25;
/// The confidence a label needs to match (the rows' own floor).
const LABEL_FLOOR: f64 = den_index::DISPLAY_CONFIDENCE_FLOOR;
/// Candidates each lane proposes.
const TITLE_LANE: usize = 50;
const FACET_LANE: usize = 500;
const LANE: usize = 200;
/// Titles like an exact title, drawn right after it.
const SIMILAR: usize = 12;
/// Votes (facets.bin) and TMDB popularity at which a title counts as fully popular.
const POPULAR_VOTES: f64 = 5000.0;
const POPULAR_POPULARITY: f64 = 50.0;
pub const PAGE: usize = 40;
pub const MAX_PAGE: usize = 100;

/// Genre words, as TMDB film genre ids.
const GENRES: &[(&str, u16)] = &[
    ("action", 28),
    ("adventure", 12),
    ("animated", 16),
    ("animation", 16),
    ("anime", 16),
    ("cartoon", 16),
    ("cartoons", 16),
    ("comedy", 35),
    ("comedies", 35),
    ("crime", 80),
    ("documentary", 99),
    ("documentaries", 99),
    ("drama", 18),
    ("dramas", 18),
    ("family", 10751),
    ("fantasy", 14),
    ("historical", 36),
    ("history", 36),
    ("horror", 27),
    ("musical", 10402),
    ("musicals", 10402),
    ("mystery", 9648),
    ("mysteries", 9648),
    ("romance", 10749),
    ("romantic", 10749),
    ("sci fi", 878),
    ("scifi", 878),
    ("science fiction", 878),
    ("thriller", 53),
    ("thrillers", 53),
    ("war", 10752),
    ("western", 37),
    ("westerns", 37),
];

/// Phrases that name a plot facet value (`plotrows.rs`). They only ever lift a title: the facets cover part of the
/// corpus, so a title without one is unknown, never a mismatch.
const PLOT_PHRASES: &[(&str, &str, &str)] = &[
    ("bittersweet", "ending", "bittersweet"),
    ("bittersweet ending", "ending", "bittersweet"),
    ("tragic", "ending", "tragic"),
    ("tragic ending", "ending", "tragic"),
    ("happy ending", "ending", "happy"),
    ("ambiguous ending", "ending", "ambiguous"),
    ("open ending", "ending", "open"),
    ("nonlinear", "structure", "nonlinear"),
    ("non linear", "structure", "nonlinear"),
    ("out of order", "structure", "nonlinear"),
    ("story within a story", "structure", "framed"),
    ("single day", "structure", "single-day"),
    ("slow burn", "pacing", "slow-burn"),
    ("fast paced", "pacing", "propulsive"),
    ("bleak", "tone", "bleak"),
    ("melancholy", "tone", "melancholy"),
    ("melancholic", "tone", "melancholy"),
    ("satire", "tone", "satirical"),
    ("satirical", "tone", "satirical"),
    ("dreamlike", "tone", "dreamlike"),
    ("surreal", "tone", "dreamlike"),
    ("near future", "era", "near-future"),
    ("far future", "era", "far-future"),
    ("19th century", "era", "19th-century"),
    ("medieval", "era", "medieval"),
    ("one location", "scope", "single-location"),
    ("single location", "scope", "single-location"),
    ("small town", "setting", "small-town"),
    ("road trip", "setting", "road"),
    ("at sea", "setting", "sea"),
    ("in space", "setting", "space"),
];

/// What a query names, and the words it leaves over.
pub struct Parsed<'a> {
    /// The whole query, folded: what titles are matched against.
    text: String,
    facet: FacetQuery,
    genres: Vec<u16>,
    /// Label names as the index spells them, and whether each is a mood.
    labels: Vec<(&'a str, bool)>,
    plot: Vec<(&'static str, &'static str)>,
    leftover: String,
    /// The share of the query's words left over: how thematic it is.
    lambda: f64,
}

impl Parsed<'_> {
    /// What the plot vectors are asked about: the leftover words, or the whole query when nothing is left over.
    /// `None` for a query too short to mean anything.
    pub fn embed_text(&self) -> Option<&str> {
        if self.text.chars().count() < 2 {
            return None;
        }
        Some(if self.leftover.is_empty() { &self.text } else { &self.leftover })
    }
}

/// Words, folded, with anything but letters and digits as the separator.
fn words(text: &str) -> Vec<String> {
    fold(text).split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// Read a query: the facets first (`FacetQuery`), then the longest phrases among the rest that name a plot facet,
/// a genre or a label; whatever is left is the leftover.
pub fn parse<'a>(text: &str, indexes: &'a Indexes) -> Parsed<'a> {
    let facet = FacetQuery::parse(text);
    let total = words(text).len().max(1);
    let tokens = words(&facet.leftover);
    let label_names: Vec<(String, &'a str, bool)> = indexes
        .plot
        .subgenre_labels()
        .into_iter()
        .map(|name| (name, false))
        .chain(indexes.plot.mood_labels().into_iter().map(|name| (name, true)))
        .map(|(name, mood)| (words(name).join(" "), name, mood))
        .collect();
    let (mut genres, mut labels, mut plot, mut rest) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut at = 0;
    'words: while at < tokens.len() {
        for span in (1..=4.min(tokens.len() - at)).rev() {
            let phrase = tokens[at..at + span].join(" ");
            let mut matched = false;
            for &(words, axis, value) in PLOT_PHRASES {
                if words == phrase && !plot.contains(&(axis, value)) {
                    plot.push((axis, value));
                    matched = true;
                }
            }
            for &(words, genre) in GENRES {
                if words == phrase && !genres.contains(&genre) {
                    genres.push(genre);
                    matched = true;
                }
            }
            for (folded, name, mood) in &label_names {
                if *folded == phrase && !labels.contains(&(*name, *mood)) {
                    labels.push((*name, *mood));
                    matched = true;
                }
            }
            if matched {
                at += span;
                continue 'words;
            }
        }
        rest.push(tokens[at].clone());
        at += 1;
    }
    Parsed {
        text: words(text).join(" "),
        lambda: rest.len() as f64 / total as f64,
        leftover: rest.join(" "),
        facet,
        genres,
        labels,
        plot,
    }
}

/// What the lanes found about a candidate before scoring.
#[derive(Default)]
struct Found {
    /// Its title in TMDB's export, and that export's popularity.
    export: Option<(String, f64)>,
    /// How far its vector stands above the scan's mean, in standard deviations.
    z: Option<f64>,
}

/// A candidate's features and score.
struct Scored {
    key: Key,
    score: f64,
    exact: bool,
    t: f64,
    sem: f64,
    lab: f64,
    pf: f64,
    pop: f64,
    phi: f64,
}

/// The answer to a parsed query: its reading, a page of hits best first, and how many there are.
pub fn answer(
    indexes: &Indexes,
    export: Option<&TitleIndex>,
    parsed: &Parsed<'_>,
    media_type: Option<MediaType>,
    vector: Option<&[i8]>,
    skip: usize,
    limit: usize,
) -> serde_json::Value {
    let wanted = |kind: MediaType| {
        media_type.is_none_or(|want| want == kind) && parsed.facet.media_type.is_none_or(|want| want == kind)
    };
    let mut found: HashMap<Key, Found> = HashMap::new();

    // Titles, by the name TMDB exports and the name atlas displays.
    let title_type = |kind: den_titlesearch::MediaType| match kind {
        den_titlesearch::MediaType::Movie => MediaType::Movie,
        den_titlesearch::MediaType::Tv => MediaType::Tv,
    };
    if let Some(export) = export {
        for hit in export.search_with(&parsed.text, None, TITLE_LANE, MIN_COVERAGE) {
            found.entry((title_type(hit.media_type), hit.tmdb_id)).or_default().export =
                Some((hit.title.to_owned(), hit.popularity));
        }
    }
    if let Some(display) = &indexes.display {
        for hit in display.search_with(&parsed.text, None, TITLE_LANE, MIN_COVERAGE) {
            found.entry((title_type(hit.media_type), hit.tmdb_id)).or_default();
        }
    }
    // A country or decade: its titles, most voted first.
    let facet_set: Option<HashSet<Key>> = (parsed.facet.has_strong_facet())
        .then(|| {
            indexes.facets.as_ref().map(|f| {
                f.filter(parsed.facet.media_type, parsed.facet.country, parsed.facet.decade)
                    .into_iter()
                    .map(|(id, kind)| (kind, id))
                    .collect()
            })
        })
        .flatten();
    if let (Some(facets), Some(set)) = (&indexes.facets, &facet_set) {
        for (id, kind) in facets
            .filter(parsed.facet.media_type, parsed.facet.country, parsed.facet.decade)
            .into_iter()
            .take(FACET_LANE)
        {
            debug_assert!(set.contains(&(kind, id)));
            found.entry((kind, id)).or_default();
        }
    }
    // Labels and plot facets the query names.
    for &(name, mood) in &parsed.labels {
        let titles = if mood {
            indexes.plot.titles_with_mood(name, None, LABEL_FLOOR, 0, LANE)
        } else {
            indexes.plot.titles_with_subgenre(name, None, LABEL_FLOOR, 0, LANE)
        };
        for (id, kind) in titles {
            found.entry((kind, id)).or_default();
        }
    }
    let mut plot_confidence: HashMap<Key, f64> = HashMap::new();
    if let Some(plot_facets) = &indexes.plot_facets {
        for &(axis, value) in &parsed.plot {
            for kind in [MediaType::Movie, MediaType::Tv] {
                for (key, confidence) in plot_facets.matching(kind, &[(axis.to_owned(), value.to_owned())]) {
                    let share = f64::from(confidence) / 3.0;
                    let best = plot_confidence.entry(key).or_insert(0.0);
                    *best = best.max(share);
                    found.entry(key).or_default();
                }
            }
        }
    }
    // The plot vectors' nearest to the leftover, within the facet when there is one.
    if let Some(vector) = vector {
        let (near, stats) = indexes.plot.scan_vector(
            vector,
            |id, kind| wanted(kind) && facet_set.as_ref().is_none_or(|set| set.contains(&(kind, id))),
            LANE,
        );
        if stats.sd > 0.0 {
            for n in near {
                found.entry((n.media_type, n.tmdb_id)).or_default().z =
                    Some((f64::from(n.score) - stats.mean) / stats.sd);
            }
        }
    }

    let mut scored: Vec<Scored> = found
        .iter()
        .filter(|(key, _)| wanted(key.0))
        .filter_map(|(&key, found)| features(indexes, parsed, key, found, &plot_confidence))
        .collect();
    let exact_answered = scored.iter().any(|s| s.exact && s.pop >= EXACT_POPULAR);
    let w_sem = W_SEMANTIC * (0.3 + 0.7 * parsed.lambda) * if exact_answered { 0.3 } else { 1.0 };
    for s in &mut scored {
        s.score = score(s, w_sem, facet_set.as_ref().is_some_and(|set| set.contains(&s.key)));
    }
    scored.retain(|s| s.score > 0.0);
    let votes =
        |(kind, id): Key| indexes.facets.as_ref().and_then(|f| f.title(id, kind)).map_or(0, |t| t.votes);
    scored.sort_by(|a, b| {
        b.score.total_cmp(&a.score).then(votes(b.key).cmp(&votes(a.key))).then(a.key.cmp(&b.key))
    });

    // An exact title leads the titles most like it.
    if let Some(top) = scored.first().filter(|s| s.exact) {
        let (kind, id) = top.key;
        let similar: Vec<Key> =
            den_index::more_like_this(Some(&indexes.plot), indexes.premise.as_ref(), id, kind)
                .into_iter()
                .take(SIMILAR)
                .map(|n| (kind, n))
                .filter(|key| wanted(key.0))
                .collect();
        let mut spliced: Vec<Scored> = Vec::with_capacity(scored.len() + similar.len());
        let mut rest: Vec<Scored> = Vec::new();
        let mut placed: HashSet<Key> = HashSet::new();
        let mut iter = scored.into_iter();
        let first = iter.next().expect("a top hit");
        placed.insert(first.key);
        spliced.push(first);
        let mut pending: HashMap<Key, Scored> = HashMap::new();
        for s in iter {
            if similar.contains(&s.key) {
                pending.insert(s.key, s);
            } else {
                rest.push(s);
            }
        }
        for key in similar {
            if !placed.insert(key) {
                continue;
            }
            let s = pending.remove(&key).or_else(|| {
                features(indexes, parsed, key, &Found::default(), &plot_confidence).map(|mut s| {
                    s.score = 0.0;
                    s
                })
            });
            spliced.extend(s);
        }
        // Best first among them; those found only as similar (score 0) keep More Like This's order.
        spliced[1..].sort_by(|a, b| b.score.total_cmp(&a.score));
        spliced.extend(rest.into_iter().filter(|s| placed.insert(s.key)));
        scored = spliced;
    }

    let total = scored.len();
    let round = |x: f64| (x * 10_000.0).round() / 10_000.0;
    let hits: Vec<serde_json::Value> = scored
        .iter()
        .skip(skip)
        .take(limit)
        .map(|s| {
            let (kind, id) = s.key;
            let card = indexes.cards.as_ref().and_then(|cards| cards.get(&s.key));
            let name = card
                .map(|c| c.title.clone())
                .or_else(|| found.get(&s.key).and_then(|f| f.export.clone()).map(|e| e.0));
            let mut hit = serde_json::json!({
                "type": if kind == MediaType::Tv { "series" } else { "movie" },
                "id": id,
                "score": round(s.score),
                "title": name,
                "posterPath": card.and_then(|c| c.poster_path.clone()),
                "year": card.and_then(|c| c.year),
                "genreIds": crate::plotrows::genres(indexes, s.key),
                "f": {"t": round(s.t), "sem": round(s.sem), "lab": round(s.lab), "pf": round(s.pf),
                      "pop": round(s.pop), "phi": s.phi},
            });
            if let Some(language) =
                indexes.facets.as_ref().and_then(|f| f.title(id, kind)).and_then(|t| t.language)
            {
                hit["originalLanguage"] = serde_json::json!(String::from_utf8_lossy(&language));
            }
            hit
        })
        .collect();
    serde_json::json!({
        "parse": {
            "mediaType": parsed.facet.media_type.map(|t| if t == MediaType::Tv { "series" } else { "movie" }),
            "country": parsed.facet.country,
            "decade": parsed.facet.decade,
            "genres": parsed.genres,
            "labels": parsed.labels.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "plotFacets": parsed.plot.iter().map(|(axis, value)| format!("{axis}={value}")).collect::<Vec<_>>(),
            "leftover": parsed.leftover,
            "lambda": round(parsed.lambda),
        },
        "hits": hits,
        "total": total,
    })
}

/// How popular a title is, 0 to 1: by its votes (facets.bin), else by TMDB's popularity in the daily export, for
/// a title facets.bin has no record of.
pub(crate) fn popularity(votes: u32, export: Option<f64>) -> f64 {
    attention(votes, export).min(1.0)
}

/// `popularity` without its ceiling, for ordering: past "fully popular", a title with more votes still comes first.
pub(crate) fn attention(votes: u32, export: Option<f64>) -> f64 {
    match export {
        _ if votes > 0 => f64::from(votes).ln_1p() / POPULAR_VOTES.ln_1p(),
        Some(popularity) => popularity.max(0.0).ln_1p() / POPULAR_POPULARITY.ln_1p(),
        None => 0.0,
    }
}

/// `S`, with `in_facet` for a title inside the country or decade the query names.
fn score(s: &Scored, w_sem: f64, in_facet: bool) -> f64 {
    let relevance = [s.t / EXACT_TITLE, s.sem, s.lab, s.pf, if in_facet { 1.0 } else { 0.0 }]
        .into_iter()
        .fold(0.0, f64::max)
        .min(1.0);
    s.phi
        * (W_TITLE * s.t
            + w_sem * s.sem
            + W_LABEL * s.lab
            + W_PLOT_FACET * s.pf
            + W_POPULARITY * s.pop * relevance)
}

/// A candidate's features, or `None` when a facet it has a record of contradicts the query.
fn features(
    indexes: &Indexes,
    parsed: &Parsed<'_>,
    key: Key,
    found: &Found,
    plot_confidence: &HashMap<Key, f64>,
) -> Option<Scored> {
    let (kind, id) = key;
    let facets = indexes.facets.as_ref().and_then(|f| f.title(id, kind));
    let record = indexes.facts.as_ref().and_then(|f| f.get(id, kind));

    // Φ: a country or decade the title is on record as not having removes it; one it has no record of discounts it.
    let mut phi = 1.0;
    if let Some(country) = parsed.facet.country {
        let code = [country.as_bytes()[0], country.as_bytes()[1]];
        let known: Vec<[u8; 2]> = facets
            .and_then(|f| f.country)
            .into_iter()
            .chain(record.map(|r| r.countries.clone()).unwrap_or_default())
            .collect();
        if known.is_empty() {
            phi = UNKNOWN_FACET;
        } else if !known.contains(&code) {
            return None;
        }
    }
    if let Some(decade) = parsed.facet.decade {
        let year = facets
            .and_then(|f| f.year)
            .map(i64::from)
            .or_else(|| record.and_then(|r| r.released).map(|r| r.year_of()));
        match year {
            Some(year) if year.div_euclid(10) * 10 != i64::from(decade) => return None,
            Some(_) => {}
            None => phi = UNKNOWN_FACET,
        }
    }

    let pop = popularity(facets.map_or(0, |f| f.votes), found.export.as_ref().map(|e| e.1));

    // T: the better of its export and display titles against the whole query.
    let card = indexes.cards.as_ref().and_then(|cards| cards.get(&key)).map(|c| c.title.as_str());
    let mut t: f64 = 0.0;
    let mut exact = false;
    for title in found.export.as_ref().map(|e| e.0.as_str()).into_iter().chain(card) {
        let (score, is_exact) = title_match(&parsed.text, title);
        exact |= is_exact;
        t = t.max(if is_exact { EXACT_TITLE + 0.4 * pop } else { score * parsed.lambda });
    }

    let sem = found.z.map_or(0.0, |z| ((z - SEMANTIC_FLOOR_Z) / SEMANTIC_SPAN_Z).clamp(0.0, 1.0));

    let mut lab: f64 = 0.0;
    if !parsed.genres.is_empty()
        && crate::plotrows::genres(indexes, key).iter().any(|g| parsed.genres.contains(g))
    {
        lab = 1.0;
    }
    if !parsed.labels.is_empty() {
        if let Some(labels) = indexes.plot.labels(id, kind) {
            for &(name, mood) in &parsed.labels {
                let pairs = if mood { &labels.moods } else { &labels.subgenres };
                if let Some(&(_, confidence)) = pairs.iter().find(|(n, _)| *n == name) {
                    if confidence >= LABEL_FLOOR {
                        lab = lab.max(confidence);
                    }
                }
            }
        }
    }
    let pf = plot_confidence.get(&key).copied().unwrap_or(0.0);

    Some(Scored { key, score: 0.0, exact, t, sem, lab, pf, pop, phi })
}

/// How well a title matches the query: exactly (folded, a leading English article dropped from both), or by the
/// share of the query's trigrams it holds, blended with their Dice overlap so a long title that merely contains
/// the query ("LEGO DC … Batman Be-Leaguered" for "batman") scores below the title itself.
fn title_match(query: &str, title: &str) -> (f64, bool) {
    let strip = |s: &str| -> String {
        let folded = words(s).join(" ");
        ["the ", "an ", "a "]
            .iter()
            .find_map(|article| folded.strip_prefix(article))
            .map_or(folded.clone(), str::to_owned)
    };
    let (q, a) = (strip(query), strip(title));
    if q.is_empty() {
        return (0.0, false);
    }
    if q == a {
        return (1.0, true);
    }
    let set = |s: &str| trigram_keys(s).into_iter().collect::<HashSet<u64>>();
    let (qs, ts) = (set(&q), set(&a));
    if qs.is_empty() || ts.is_empty() {
        return (0.0, false);
    }
    let shared = qs.intersection(&ts).count() as f64;
    let coverage = shared / qs.len() as f64;
    if coverage < MIN_COVERAGE {
        return (0.0, false);
    }
    let dice = 2.0 * shared / (qs.len() + ts.len()) as f64;
    (FUZZY_CAP * (0.5 * coverage + 0.5 * dice), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_matches_exactly_without_its_article_and_a_longer_one_scores_less() {
        assert_eq!(title_match("the matrix", "The Matrix"), (1.0, true));
        assert_eq!(title_match("matrix", "The Matrix"), (1.0, true));
        let (reloaded, exact) = title_match("matrix", "The Matrix Reloaded");
        assert!(!exact && reloaded > 0.0 && reloaded < FUZZY_CAP);
        let (lego, _) = title_match("batman", "LEGO DC Comics Super Heroes: Batman Be-Leaguered");
        let (typo, _) = title_match("batmn", "Batman");
        assert!(lego < typo, "{lego} {typo}");
        assert_eq!(title_match("marty supreme", "The Matrix"), (0.0, false));
    }

    #[test]
    fn the_exact_title_invariant_holds() {
        // An exact title with no popularity at all against a match on everything but the title.
        let exact = W_TITLE * EXACT_TITLE;
        let theme = W_SEMANTIC + W_LABEL + W_PLOT_FACET + W_POPULARITY;
        assert!(exact > theme, "{exact} {theme}");
        assert!(W_TITLE * FUZZY_CAP < exact);
    }

    #[test]
    fn popularity_reads_votes_and_else_the_export() {
        assert_eq!(popularity(5000, Some(1.0)), 1.0, "votes win");
        assert_eq!(popularity(0, Some(50.0)), 1.0);
        assert!(popularity(0, Some(1.0)) < 0.2);
        assert_eq!(popularity(0, None), 0.0);
        assert!(attention(30_000, None) > attention(5000, None), "ordering keeps apart what popularity caps");
    }

    #[test]
    fn popularity_counts_only_as_far_as_a_title_is_relevant() {
        let hit = |sem, pop| Scored {
            key: (MediaType::Movie, 1),
            score: 0.0,
            exact: false,
            t: 0.0,
            sem,
            lab: 0.0,
            pf: 0.0,
            pop,
            phi: 1.0,
        };
        // "movies about grief": a closer, less voted film against a famous one that merely sounds alike.
        let w_sem = W_SEMANTIC * (0.3 + 0.7 * 0.67);
        assert!(score(&hit(0.37, 0.67), w_sem, false) > score(&hit(0.30, 0.95), w_sem, false));
        assert_eq!(score(&hit(0.0, 1.0), w_sem, false), 0.0);
        assert!(score(&hit(0.0, 1.0), w_sem, true) > 0.0, "a facet's titles rank by popularity");
    }
}
