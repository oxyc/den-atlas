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

use crate::facts::SourceKinds;
use crate::queries::Indexes;
use den_index::{FacetQuery, MediaType};
use den_titlesearch::{fold, trigram_keys, TitleIndex};
use std::collections::{HashMap, HashSet};

type Key = (MediaType, u32);

const W_TITLE: f64 = 2.0;
const W_LABEL: f64 = 0.25;
/// A title by someone the query names: in full when they worked on it in the role they mostly work in, a little
/// less in their other one, so an actor's films come before the few they produced and a director's before their
/// cameos.
const W_PERSON: f64 = 0.8;
const OTHER_ROLE: f64 = 0.9;
/// People an answer names at most.
const PEOPLE: usize = 5;
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

/// The longest phrase the parser will consider. "based on a video game" is five words; matching only four
/// meant the most natural way to ask for one silently did nothing.
const MAX_PHRASE_WORDS: usize = 5;

/// Ways people ask for an adaptation, and the kind each names.
///
/// "based on a book" used to fall through to the semantic lane and be matched as prose, which returned
/// *Books of Blood* — a film with the word in its title — instead of adaptations. It is a FACET, like a
/// country or a decade: the facts state it outright, and no amount of reading the plot text can.
///
/// Phrases are matched against up to `MAX_PHRASE_WORDS` folded words, so articles are spelled out rather
/// than stripped; both forms are listed because both get typed. A phrase longer than that limit can never
/// match — `every_source_phrase_is_reachable` holds the two together.
const SOURCE_PHRASES: &[(&str, u16)] = &[
    ("based on a book", SourceKinds::BOOK),
    ("based on book", SourceKinds::BOOK),
    ("based on a novel", SourceKinds::BOOK),
    ("based on novel", SourceKinds::BOOK),
    ("book adaptation", SourceKinds::BOOK),
    ("novel adaptation", SourceKinds::BOOK),
    ("literary adaptation", SourceKinds::BOOK),
    ("from a book", SourceKinds::BOOK),
    ("from a novel", SourceKinds::BOOK),
    ("based on a comic", SourceKinds::COMIC),
    ("based on comic", SourceKinds::COMIC),
    ("based on a manga", SourceKinds::COMIC),
    ("based on manga", SourceKinds::COMIC),
    ("comic adaptation", SourceKinds::COMIC),
    ("manga adaptation", SourceKinds::COMIC),
    ("graphic novel", SourceKinds::COMIC),
    ("based on a play", SourceKinds::PLAY),
    ("based on play", SourceKinds::PLAY),
    ("stage adaptation", SourceKinds::PLAY),
    ("based on a video game", SourceKinds::GAME),
    ("based on a game", SourceKinds::GAME),
    ("video game adaptation", SourceKinds::GAME),
    ("game adaptation", SourceKinds::GAME),
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
pub struct Parsed {
    /// The whole query, folded: what titles are matched against.
    text: String,
    /// The whole query as titles are compared with it.
    title_query: TitleQuery,
    facet: FacetQuery,
    genres: Vec<u16>,
    /// Label names as the index spells them, and whether each is a mood.
    labels: Vec<(String, bool)>,
    plot: Vec<(&'static str, &'static str)>,
    /// The source kinds the query names ("based on a book"), as a `SourceKinds` mask.
    source_kinds: u16,
    /// The people the query names, by Q-id, most credited first.
    people: Vec<u32>,
    /// Those of them who mostly make titles (direct or create) rather than appear in them.
    makers: Vec<u32>,
    /// Each of them's titles, and whether they made each (`Facts::credits`): read off the facts once per query.
    credits: HashMap<u32, Vec<(Key, bool)>>,
    leftover: String,
    /// The share of the query's words left over: how thematic it is.
    lambda: f64,
}

/// A query as `Parsed` reads it, before anything is read out of it: folded words joined by single spaces.
pub fn normalized(text: &str) -> String {
    words(text).join(" ")
}

impl Parsed {
    /// The whole query, as `normalized` gives it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What the plot vectors are asked about: the leftover words, or the whole query when nothing is left over.
    /// `None` for a query too short to mean anything.
    pub fn embed_text(&self) -> Option<&str> {
        if self.text.chars().count() < 2 {
            return None;
        }
        // NOTHING LEFT OVER MEANS NOTHING TO ASK THE VECTORS.
        //
        // When every word was claimed, handing the whole query back embeds the FACET PHRASE as prose — and
        // the plot vectors hold no people, no decades, no countries and no source kinds, so they can only
        // add what sounds alike. "1980s" scored Ho Fatto Splash at 0.93 and pushed Back to the Future to
        // fourth; "based on a book" returned films with the word in their titles, which is the very failure
        // SOURCE_PHRASES was added to fix — the parse was corrected and this line let the words back in
        // through the other door.
        //
        // The rule was already here for people alone. It is the same rule; it was just written once.
        if self.leftover.is_empty() && self.names_something() {
            return None;
        }
        Some(if self.leftover.is_empty() { &self.text } else { &self.leftover })
    }

    /// Set the release-year window from a caller that computed it, overriding whatever the text implied.
    pub fn set_year_min(&mut self, year: u16) {
        self.facet.year_min = Some(year);
    }

    pub fn set_year_max(&mut self, year: u16) {
        self.facet.year_max = Some(year);
    }

    /// True when the query named anything the facts or labels can answer directly.
    fn names_something(&self) -> bool {
        !self.people.is_empty()
            || !self.genres.is_empty()
            || !self.labels.is_empty()
            || !self.plot.is_empty()
            || self.source_kinds != 0
            || self.facet.country.is_some()
            || self.facet.decade.is_some()
            || self.facet.year_min.is_some()
            || self.facet.year_max.is_some()
    }
}

/// Words, folded, with anything but letters and digits as the separator.
fn words(text: &str) -> Vec<String> {
    fold(text).split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// Read a query: the facets first (`FacetQuery`), then the longest phrases among the rest that name a plot facet,
/// a genre or a label; whatever is left is the leftover.
pub fn parse(text: &str, indexes: &Indexes) -> Parsed {
    let facet = FacetQuery::parse(text);
    let total = words(text).len().max(1);
    let tokens = words(&facet.leftover);
    let label_names: Vec<(String, &str, bool)> = indexes
        .plot
        .subgenre_labels()
        .into_iter()
        .map(|name| (name, false))
        .chain(indexes.plot.mood_labels().into_iter().map(|name| (name, true)))
        .map(|(name, mood)| (words(name).join(" "), name, mood))
        .collect();
    let (mut genres, mut labels, mut plot, mut rest) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut source_kinds: u16 = 0;
    let mut people: Vec<u32> = Vec::new();
    let mut at = 0;
    'words: while at < tokens.len() {
        for span in (1..=MAX_PHRASE_WORDS.min(tokens.len() - at)).rev() {
            let phrase = tokens[at..at + span].join(" ");
            let mut matched = false;
            for &(words, kind) in SOURCE_PHRASES {
                if words == phrase {
                    source_kinds |= kind;
                    matched = true;
                }
            }
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
                if *folded == phrase && !labels.iter().any(|(n, m)| n == name && m == mood) {
                    labels.push(((*name).to_owned(), *mood));
                    matched = true;
                }
            }
            // A one-word name ("Nolan", "Common") counts only as the whole query: inside a longer one it is more
            // likely just a word.
            if let Some(facts) = indexes.facts.as_ref().filter(|_| span >= 2 || tokens.len() == 1) {
                let going_by = facts.people_named(&phrase);
                matched |= !going_by.is_empty();
                for qid in going_by {
                    if !people.contains(&qid) {
                        people.push(qid);
                    }
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
    let credits: HashMap<u32, Vec<(Key, bool)>> = indexes
        .facts
        .as_ref()
        .map(|facts| people.iter().map(|&qid| (qid, facts.credits(qid))).collect())
        .unwrap_or_default();
    let makers = people
        .iter()
        .copied()
        .filter(|qid| {
            credits.get(qid).is_some_and(|c| 2 * c.iter().filter(|(_, made)| *made).count() >= c.len())
        })
        .collect();
    let whole = normalized(text);
    Parsed {
        title_query: TitleQuery::new(&whole),
        text: whole,
        credits,
        lambda: rest.len() as f64 / total as f64,
        leftover: rest.join(" "),
        facet,
        genres,
        labels,
        plot,
        source_kinds,
        people,
        makers,
    }
}

/// What the lanes found about a candidate before scoring.
#[derive(Default)]
struct Found {
    /// Its title in TMDB's export, and that export's popularity.
    export: Option<(String, f64)>,
    /// The names it matched by in the display title index: its displayed title, or another name it goes by.
    names: Vec<String>,
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
    person: f64,
    pop: f64,
    phi: f64,
}

/// The answer to a parsed query: its reading, a page of hits best first, and how many there are.
pub fn answer(
    indexes: &Indexes,
    export: Option<&TitleIndex>,
    parsed: &Parsed,
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
            found
                .entry((title_type(hit.media_type), hit.tmdb_id))
                .or_default()
                .names
                .push(hit.title.to_owned());
        }
    }
    // A country or decade: its titles, most voted first. All of them bound the plot vectors; the first
    // `FACET_LANE` join the candidates.
    let facet_titles: Option<Vec<Key>> =
        indexes.facets.as_ref().filter(|_| parsed.facet.has_strong_facet()).map(|f| {
            f.filter_years(
                parsed.facet.media_type,
                parsed.facet.country,
                parsed.facet.decade,
                parsed.facet.year_min,
                parsed.facet.year_max,
            )
            .into_iter()
            .map(|(id, kind)| (kind, id))
            .collect()
        });
    for &key in facet_titles.iter().flatten().take(FACET_LANE) {
        found.entry(key).or_default();
    }
    // SOURCE KIND AND GENRE PROPOSE TOO. Without a lane of their own they could only filter or boost titles
    // some other lane had already found, so a query naming nothing else was answered by the plot vectors and
    // Fight Club could not rank as a book adaptation because it was never a candidate.
    //
    // Ordered by votes, like the country/decade lane: a facet names a SET, not an order within it, so the
    // order has to come from a prior, and popularity is the one Den uses everywhere else.
    let mut named_titles: Vec<Key> = Vec::new();
    if let Some(facts) = indexes.facts.as_ref() {
        if parsed.source_kinds != 0 {
            named_titles.extend(facts.titles_with_source_kind(parsed.source_kinds));
        }
        for &genre in &parsed.genres {
            named_titles.extend(facts.titles_with_genre(genre));
        }
    }
    if !named_titles.is_empty() {
        named_titles.sort_unstable();
        named_titles.dedup();
        if let Some(media_type) = parsed.facet.media_type {
            named_titles.retain(|(kind, _)| *kind == media_type);
        }
        let votes =
            |key: &Key| indexes.facets.as_ref().and_then(|f| f.title(key.1, key.0)).map_or(0, |t| t.votes);
        named_titles.sort_by_key(|key| std::cmp::Reverse(votes(key)));
        named_titles.truncate(FACET_LANE);
        for &key in &named_titles {
            found.entry(key).or_default();
        }
    }
    let named_set: HashSet<Key> = named_titles.into_iter().collect();
    let facet_set: Option<HashSet<Key>> = facet_titles.map(|titles| titles.into_iter().collect());
    // Labels and plot facets the query names.
    for (name, mood) in &parsed.labels {
        let titles = if *mood {
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
    // The titles of the people the query names.
    for credits in parsed.credits.values() {
        for &(key, _) in credits {
            found.entry(key).or_default();
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
        // A title the query NAMED — by country, decade, source kind or genre — is relevant for that reason
        // alone. Without this, a book adaptation with no other signal scores 0 and is dropped by the retain
        // below, which is how `based on a book` returned 200 prose neighbours and none of its 4,750 titles.
        let named = facet_set.as_ref().is_some_and(|set| set.contains(&s.key)) || named_set.contains(&s.key);
        s.score = score(s, w_sem, named);
    }
    // A title nothing names — no card, no export title: a facts-only record — has nothing to draw it by.
    let drawable = |key: &Key| {
        indexes.cards.as_ref().is_some_and(|cards| cards.contains_key(key))
            || found.get(key).is_some_and(|f| f.export.is_some())
    };
    scored.retain(|s| s.score > 0.0 && drawable(&s.key));
    let votes =
        |(kind, id): Key| indexes.facets.as_ref().and_then(|f| f.title(id, kind)).map_or(0, |t| t.votes);
    scored.sort_by(|a, b| {
        b.score.total_cmp(&a.score).then(votes(b.key).cmp(&votes(a.key))).then(a.key.cmp(&b.key))
    });

    // An exact title leads the titles most like it.
    if let Some(top) = scored.first().filter(|s| s.exact) {
        let (kind, id) = top.key;
        let similar: Vec<Key> = indexes
            .more_like_this(id, kind)
            .iter()
            .take(SIMILAR)
            .map(|&n| (kind, n))
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
            spliced.extend(s.filter(|s| drawable(&s.key)));
        }
        // Best first among them; those found only as similar (score 0) keep More Like This's order.
        spliced[1..].sort_by(|a, b| b.score.total_cmp(&a.score));
        spliced.extend(rest.into_iter().filter(|s| placed.insert(s.key)));
        scored = spliced;
    }

    let total = scored.len();
    let people: Vec<serde_json::Value> = indexes
        .facts
        .as_ref()
        .map(|facts| {
            parsed
                .people
                .iter()
                .filter_map(|&qid| {
                    let person = facts.person(qid)?;
                    Some(serde_json::json!({
                        "qid": format!("Q{qid}"),
                        "id": person.tmdb_id,
                        "name": person.name,
                        "credits": parsed.credits.get(&qid).map_or(0, Vec::len),
                    }))
                })
                .take(PEOPLE)
                .collect()
        })
        .unwrap_or_default();
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
                "f": {"t": round(s.t), "sem": round(s.sem), "lab": round(s.lab), "pf": round(s.pf), "p": s.person,
                      "pop": round(s.pop), "phi": s.phi},
            });
            // Its IMDb id, which a client's availability check keys streams by: without it the client asks TMDB
            // for it, a request a card.
            if let Some(imdb) = indexes.facts.as_ref().and_then(|f| f.get(id, kind)).and_then(|r| r.imdb_id.as_deref())
            {
                hit["imdbId"] = serde_json::json!(imdb);
            }
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
            "basedOnKind": SourceKinds::names(parsed.source_kinds),
            "yearMin": parsed.facet.year_min,
            "yearMax": parsed.facet.year_max,
            "leftover": parsed.leftover,
            "lambda": round(parsed.lambda),
        },
        "people": people,
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

/// `S`, with `in_facet` for a title the query NAMED — inside its country or decade, carrying its genre, or
/// adapted from the kind it asked for. Such a title is relevant on that basis alone, so popularity applies
/// to it: a facet names a set, and within that set popularity is the only order available.
fn score(s: &Scored, w_sem: f64, in_facet: bool) -> f64 {
    let relevance = [s.t / EXACT_TITLE, s.sem, s.lab, s.pf, s.person, if in_facet { 1.0 } else { 0.0 }]
        .into_iter()
        .fold(0.0, f64::max)
        .min(1.0);
    s.phi
        * (W_TITLE * s.t
            + w_sem * s.sem
            + W_LABEL * s.lab
            + W_PLOT_FACET * s.pf
            + W_PERSON * s.person
            + W_POPULARITY * s.pop * relevance)
}

/// A candidate's features, or `None` when a facet it has a record of contradicts the query.
fn features(
    indexes: &Indexes,
    parsed: &Parsed,
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
    // The release-year window. Same rule as country and decade: a title ON RECORD outside it is not what was
    // asked for; one with no year at all is unknown, and unknown is not a mismatch.
    if parsed.facet.year_min.is_some() || parsed.facet.year_max.is_some() {
        let year = facets
            .and_then(|f| f.year)
            .filter(|&y| y > 0)
            .map(i64::from)
            .or_else(|| record.and_then(|r| r.released).map(|r| r.year_of()));
        match year {
            Some(year) => {
                if parsed.facet.year_min.is_some_and(|min| year < i64::from(min))
                    || parsed.facet.year_max.is_some_and(|max| year > i64::from(max))
                {
                    return None;
                }
            }
            None => phi *= UNKNOWN_FACET,
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

    // A source kind the query names. The facts state it outright, so a title ON RECORD as adapted from
    // something else is genuinely not what was asked for and drops out — the same rule as country.
    //
    // A title with NO basedOn statement is unknown, not an original work: Wikidata is open-world, and only
    // 19% of records carry the property at all. Discounting rather than dropping keeps the other 81%
    // reachable, which is the difference between a working row and an empty one.
    if parsed.source_kinds != 0 {
        match record.map(|r| r.source_kinds) {
            Some(kinds) if kinds.is_empty() => phi *= UNKNOWN_FACET,
            Some(kinds) if kinds.raw() & parsed.source_kinds != 0 => {}
            Some(_) => return None,
            None => phi *= UNKNOWN_FACET,
        }
    }

    let pop = popularity(facets.map_or(0, |f| f.votes), found.export.as_ref().map(|e| e.1));

    // T: the better of its export and display titles against the whole query.
    let card = indexes.cards.as_ref().and_then(|cards| cards.get(&key)).map(|c| c.title.as_str());
    let mut t: f64 = 0.0;
    let mut exact = false;
    let also_named = found.names.iter().map(String::as_str);
    for title in found.export.as_ref().map(|e| e.0.as_str()).into_iter().chain(card).chain(also_named) {
        let (score, is_exact) = parsed.title_query.score(title);
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
            for (name, mood) in &parsed.labels {
                let pairs = if *mood { &labels.moods } else { &labels.subgenres };
                if let Some(&(_, confidence)) = pairs.iter().find(|(n, _)| *n == name.as_str()) {
                    if confidence >= LABEL_FLOOR {
                        lab = lab.max(confidence);
                    }
                }
            }
        }
    }
    let pf = plot_confidence.get(&key).copied().unwrap_or(0.0);
    // A maker's title they made, an actor's title they appear in: their own role. Anyone named, otherwise: the other.
    let in_role = |qids: &[u32], making: bool| {
        qids.iter().any(|q| parsed.people.contains(q) && parsed.makers.contains(q) == making)
    };
    let person = match record {
        Some(r) if in_role(&r.makers, true) || in_role(&r.cast, false) => 1.0,
        Some(r) if r.makers.iter().chain(&r.cast).any(|q| parsed.people.contains(q)) => OTHER_ROLE,
        _ => 0.0,
    };

    Some(Scored { key, score: 0.0, exact, t, sem, lab, pf, person, pop, phi })
}

/// The query as titles are matched against it — folded, a leading English article dropped, and its trigrams —
/// worked out once, since every candidate's titles are compared with it.
struct TitleQuery {
    stripped: String,
    trigrams: HashSet<u64>,
}

impl TitleQuery {
    fn new(query: &str) -> TitleQuery {
        let stripped = without_article(query);
        let trigrams = trigram_keys(&stripped).into_iter().collect();
        TitleQuery { stripped, trigrams }
    }

    /// How well a title matches the query: exactly (folded, a leading English article dropped from both), or by
    /// the share of the query's trigrams it holds, blended with their Dice overlap so a long title that merely
    /// contains the query ("LEGO DC … Batman Be-Leaguered" for "batman") scores below the title itself.
    fn score(&self, title: &str) -> (f64, bool) {
        if self.stripped.is_empty() {
            return (0.0, false);
        }
        let title = without_article(title);
        if title == self.stripped {
            return (1.0, true);
        }
        let theirs: HashSet<u64> = trigram_keys(&title).into_iter().collect();
        if self.trigrams.is_empty() || theirs.is_empty() {
            return (0.0, false);
        }
        let shared = self.trigrams.intersection(&theirs).count() as f64;
        let coverage = shared / self.trigrams.len() as f64;
        if coverage < MIN_COVERAGE {
            return (0.0, false);
        }
        let dice = 2.0 * shared / (self.trigrams.len() + theirs.len()) as f64;
        (FUZZY_CAP * (0.5 * coverage + 0.5 * dice), false)
    }
}

/// Folded words, a leading English article dropped.
fn without_article(text: &str) -> String {
    let folded = normalized(text);
    ["the ", "an ", "a "]
        .iter()
        .find_map(|article| folded.strip_prefix(article))
        .map_or(folded.clone(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "based on a book" fell through to the semantic lane and matched prose, returning Books of Blood —
    /// a film with the word in its title. It names a FACET; the facts answer it outright.
    #[test]
    fn asking_for_an_adaptation_names_a_source_kind() {
        let kinds = |query: &str| {
            let tokens = words(query);
            let mut mask = 0u16;
            let mut at = 0;
            while at < tokens.len() {
                let mut hit = false;
                for span in (1..=MAX_PHRASE_WORDS.min(tokens.len() - at)).rev() {
                    let phrase = tokens[at..at + span].join(" ");
                    for &(words, kind) in SOURCE_PHRASES {
                        if words == phrase {
                            mask |= kind;
                            hit = true;
                        }
                    }
                    if hit {
                        at += span;
                        break;
                    }
                }
                if !hit {
                    at += 1;
                }
            }
            SourceKinds::names(mask)
        };
        assert_eq!(kinds("based on a book"), vec!["book"]);
        assert_eq!(kinds("recent movies based on a novel"), vec!["book"]);
        assert_eq!(kinds("manga adaptation"), vec!["comic"]);
        assert_eq!(kinds("based on a video game"), vec!["game"]);
        // "based on a video game" must win over the shorter "based on a game" inside it.
        assert_eq!(kinds("based on a video game"), vec!["game"]);
        assert_eq!(kinds("the shawshank redemption"), Vec::<&str>::new());
    }

    /// A phrase longer than the matcher's span can never fire. "based on a video game" is five words and was
    /// dead on arrival against a four-word limit — invisible, because an unmatched phrase just falls through
    /// to the semantic lane and returns something plausible.
    #[test]
    fn every_source_phrase_is_reachable() {
        for &(phrase, _) in SOURCE_PHRASES {
            let n = phrase.split_whitespace().count();
            assert!(n <= MAX_PHRASE_WORDS, "{phrase:?} is {n} words, past the {MAX_PHRASE_WORDS}-word span");
        }
        for &(phrase, _, _) in PLOT_PHRASES {
            let n = phrase.split_whitespace().count();
            assert!(n <= MAX_PHRASE_WORDS, "{phrase:?} is {n} words, past the {MAX_PHRASE_WORDS}-word span");
        }
    }

    #[test]
    fn a_title_matches_exactly_without_its_article_and_a_longer_one_scores_less() {
        let title_match = |query: &str, title: &str| TitleQuery::new(query).score(title);
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
        // And a title merely by someone the query names.
        assert!(W_PERSON + W_POPULARITY < exact);
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
            person: 0.0,
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
