//! `GET /index/query.json?q=&type=&skip=&limit=` — search in one request, so no client fuses lanes of its own.
//!
//! The query is read for what it names — a country, a decade, a type, a genre, one of atlas's labels, a plot facet —
//! and the words left over. Every lane then only proposes titles: a fuzzy title match (TMDB's export titles and the
//! display titles atlas draws with), the facet's titles, a label's or plot facet's titles, and the plot and
//! premise vectors' nearest to the leftover. Every candidate is scored on every signal, whichever lane found it:
//!
//! `S = Φ · [2.0·T + w_sem·max(Sem_plot, Sem_premise) + 0.25·L + 0.10·PF + 0.15·Pop·R]`,
//! `w_sem = 0.6·(0.3 + 0.7·λ)·(1 − 0.7·exact)`
//!
//! A title match is weighted so that an exact title (T ≥ 0.6, so ≥ 1.2) always beats a match on theme alone (at
//! most 0.6 + 0.25 + 0.10 + 0.15 = 1.10); semantic vectors count for more the more of the query is left over (λ),
//! and far less once an exact title answered, since they hold no titles and would only add what sounds alike.
//! A country or date inferred from the same words cannot discount an exact title: ambiguous text keeps both
//! readings alive, while explicit request constraints still filter normally. When those words are, whole, the
//! name of a popular title (`russian doll`), the facet reading is contested: it still proposes and lifts its
//! titles, but discounts nothing and bounds nothing, and the vectors read the whole query.
//! A near title match counts only for the share of words no table reads (T·λₜ): a query of words atlas reads
//! ("bleak") asks for the theme, not for "Leak" or "Bleach". Popularity counts only as far as the title is relevant at all (R, the
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
/// What a candidate keeps when it CONTRADICTS a facet that was guessed from the query's words rather than
/// given as a parameter. Heavy enough that the facet still orders the answer. It is not applied to an exact
/// title: `brazil` may name both a country and Gilliam's film, and neither reading should erase the other.
const WRONG_TEXT_FACET: f64 = 0.15;
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
/// Titles a ruled-out name is looked up among: enough for every film in a long series ("harry potter").
const EXCLUDED_TITLES: usize = 200;
const FACET_LANE: usize = 500;
const LANE: usize = 200;
/// The vote count at which a title counts as fully popular.
pub(crate) const POPULAR_VOTES: f64 = 5000.0;
/// Votes a point of TMDB export popularity is worth, for the titles the store has no count for.
///
/// Measured, not chosen: over the 47,547 store rows that have both a vote count and an entry in the
/// 2026-09-20 export, a log-log fit gives `votes = 39.8 * popularity^0.997` — near enough linear to use a
/// single factor. It is a weak relation and deliberately used only where nothing better exists: the two
/// quantities measure different things (popularity is a ~30-day activity score, the count is
/// cumulative-forever), and the ratio is not constant across the range — the median falls from ~37 votes
/// per point below popularity 5 to ~7 above 100. That tail is why `attention` clamps the conversion at
/// POPULAR_VOTES rather than trusting it to extrapolate.
pub(crate) const VOTES_PER_POPULARITY: f64 = 39.8;
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
    // A kind of film, not a title: `love stories` answered with six films called Love Story.
    ("love story", 10749),
    ("love stories", 10749),
    ("musical", 10402),
    ("musicals", 10402),
    ("mystery", 9648),
    ("mysteries", 9648),
    ("romance", 10749),
    ("romances", 10749),
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
///
/// The bare plurals `books` and `novels` name the kind too: nobody browsing films types `books` for titles with
/// the word in them, and `q=books` answered with Circus of Books, Book Club and Booksmart. The singular stays out
/// (The Book Thief, Book Club), and so do `comics` (comedians) and `video games` (films about them).
const SOURCE_PHRASES: &[(&str, u16)] = &[
    ("based on a book", SourceKinds::BOOK),
    ("based on book", SourceKinds::BOOK),
    ("based on books", SourceKinds::BOOK),
    ("based on a novel", SourceKinds::BOOK),
    ("based on novel", SourceKinds::BOOK),
    ("based on novels", SourceKinds::BOOK),
    ("book adaptation", SourceKinds::BOOK),
    ("book adaptations", SourceKinds::BOOK),
    ("novel adaptation", SourceKinds::BOOK),
    ("novel adaptations", SourceKinds::BOOK),
    ("literary adaptation", SourceKinds::BOOK),
    ("literary adaptations", SourceKinds::BOOK),
    ("from a book", SourceKinds::BOOK),
    ("from a novel", SourceKinds::BOOK),
    ("books", SourceKinds::BOOK),
    ("novels", SourceKinds::BOOK),
    ("based on a comic", SourceKinds::COMIC),
    ("based on comic", SourceKinds::COMIC),
    ("based on comics", SourceKinds::COMIC),
    ("based on a comic book", SourceKinds::COMIC),
    ("based on a manga", SourceKinds::COMIC),
    ("based on manga", SourceKinds::COMIC),
    ("comic adaptation", SourceKinds::COMIC),
    ("comic adaptations", SourceKinds::COMIC),
    ("manga adaptation", SourceKinds::COMIC),
    ("manga adaptations", SourceKinds::COMIC),
    ("comic book", SourceKinds::COMIC),
    ("comic books", SourceKinds::COMIC),
    ("graphic novel", SourceKinds::COMIC),
    ("graphic novels", SourceKinds::COMIC),
    ("based on a play", SourceKinds::PLAY),
    ("based on play", SourceKinds::PLAY),
    ("stage adaptation", SourceKinds::PLAY),
    ("based on a video game", SourceKinds::GAME),
    ("based on a game", SourceKinds::GAME),
    ("based on video games", SourceKinds::GAME),
    ("video game adaptation", SourceKinds::GAME),
    ("video game adaptations", SourceKinds::GAME),
    ("game adaptation", SourceKinds::GAME),
    ("game adaptations", SourceKinds::GAME),
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
    /// An upper bound on runtime, from a parameter. Minutes.
    runtime_max: Option<u32>,
    /// A broadcaster's Q-id, from a parameter. Series-only.
    broadcaster: Option<u32>,
    /// Whether the year window was GIVEN by the caller rather than read out of the words.
    ///
    /// Only an explicit parameter may drop a title. A facet guessed from prose may discount, never erase:
    /// `russian doll` read RU and lost Russian Doll, `brazil` lost Gilliam's Brazil, `1917` lost the 2019
    /// film. The caller who sends `year_min=2015` meant it; the person who typed a word that happens to be
    /// a country did not.
    year_from_param: bool,
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
    /// The share of the query's words no table reads, which is what a near title match counts for. A genre,
    /// label or plot-facet word stays in the leftover for the vectors, but a query made of such words asks for
    /// the theme: `bleak` is not asking for *Leak*, nor `love stories` for five films called *Love Story*.
    title_lambda: f64,
    /// True when the words that named a country, decade or year are also, whole, the name of a popular title
    /// (`russian doll`, `the french connection`). Both readings are then kept: the facet proposes its titles
    /// and lifts them, but no longer bounds the vectors or discounts what it does not hold, and the vectors are
    /// asked about the whole query.
    contested: bool,
    /// The words asked for — the query without what it rules out — folded.
    kept: String,
    excluded: Excluded,
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

    /// What the plot vectors are asked about: the leftover words, or the words asked for when nothing is left
    /// over — never a ruled-out word, which the vectors would read as the very thing it rules out. `None` for a
    /// query too short to mean anything.
    pub fn embed_text(&self) -> Option<&str> {
        if self.kept.chars().count() < 2 {
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
        Some(if self.leftover.is_empty() { &self.kept } else { &self.leftover })
    }

    /// Set the release-year window from a caller that computed it, overriding whatever the text implied.
    /// The original language, ISO 639-1. A parameter only, so it DROPS a title on record in another —
    /// coverage is 93.5%, and unlike a demonym read from prose the caller meant exactly this.
    pub fn set_language(&mut self, code: &str) {
        self.facet.language = Some(code.to_ascii_lowercase());
    }

    /// An upper bound on runtime in minutes. Answers "something short tonight", which atlas could not.
    pub fn set_runtime_max(&mut self, minutes: u32) {
        self.runtime_max = Some(minutes);
    }

    /// The network or service a series first aired on (P449), by Q-id.
    pub fn set_broadcaster(&mut self, qid: u32) {
        self.broadcaster = Some(qid);
    }

    pub fn set_year_min(&mut self, year: u16) {
        self.facet.year_min = Some(year);
        self.year_from_param = true;
        // An explicit window REPLACES a decade the words implied, rather than intersecting with it: asking
        // for `80s horror` with year_min=2020 should not quietly answer nothing.
        self.facet.decade = None;
    }

    pub fn set_year_max(&mut self, year: u16) {
        self.facet.year_max = Some(year);
        self.year_from_param = true;
        self.facet.decade = None;
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

/// What a run of words names: the tables' matches, and the words no table consumed.
#[derive(Default)]
struct Names {
    genres: Vec<u16>,
    labels: Vec<(String, bool)>,
    plot: Vec<(&'static str, &'static str)>,
    source_kinds: u16,
    people: Vec<u32>,
    rest: Vec<String>,
    /// How many of `rest` no genre, label or plot-facet phrase covers.
    unread: usize,
}

impl Names {
    fn names_anything(&self) -> bool {
        !self.genres.is_empty()
            || !self.labels.is_empty()
            || !self.plot.is_empty()
            || self.source_kinds != 0
            || !self.people.is_empty()
    }
}

/// What a query rules out ("not", "without" — `split_negation`). Each is a DROP: the person said they do not
/// want it, which is a statement, not a reading of ambiguous words.
#[derive(Default)]
struct Excluded {
    /// The ruled-out phrases as written, for the parse report.
    phrases: Vec<String>,
    countries: Vec<&'static str>,
    decades: Vec<u16>,
    media_types: Vec<MediaType>,
    names: Names,
    /// Titles CALLED what was ruled out, and the rest of a franchise one of them leads.
    titles: HashSet<Key>,
}

impl Excluded {
    fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }
}

/// Leading and trailing words that cannot name a title on their own: "not from the 80s" leaves "from the"
/// once the decade is read, and that must not rule out *From the Earth to the Moon*.
const FILLER: &[&str] = &["a", "an", "the", "any", "from", "of", "in", "with", "too", "much", "so", "very"];

/// Read a query: the facets first (`FacetQuery`), then the longest phrases among the rest that name a plot facet,
/// a genre or a label; whatever is left is the leftover. Words after a negation are read the same way and rule
/// out what they name.
pub fn parse(text: &str, indexes: &Indexes) -> Parsed {
    let whole = normalized(text);
    let title_query = TitleQuery::new(&whole);
    let mut negation = den_index::split_negation(&whole);
    // A title that contains a negation is a title: `do not disturb` asks for the film, not for things without
    // a disturbance.
    if !negation.excluded.is_empty() && !exact_titles(indexes, &whole).is_empty() {
        negation = den_index::Negation { kept: whole.clone(), excluded: Vec::new() };
    }
    let facet = FacetQuery::parse(&negation.kept);
    let contested = facet.has_strong_facet()
        && exact_titles(indexes, &negation.kept).into_iter().any(|pop| pop >= EXACT_POPULAR);
    let total = words(&negation.kept).len().max(1);
    let label_names: Vec<(String, &str, bool)> = indexes
        .plot
        .subgenre_labels()
        .into_iter()
        .map(|name| (name, false))
        .chain(indexes.plot.mood_labels().into_iter().map(|name| (name, true)))
        .map(|(name, mood)| (words(name).join(" "), name, mood))
        .collect();
    let tokens = words(&facet.leftover);
    let one_word_names = tokens.len() == 1;
    let Names { genres, labels, plot, source_kinds, people, mut rest, unread } =
        read_names(&tokens, one_word_names, indexes, &label_names);
    if contested {
        // A contested facet gives its words back to the leftover, so the vectors read `russian doll` rather
        // than `doll`. Nothing else is read from them — `the italian` is also somebody's alias — and they stay
        // out of `unread`: `the french connection` is not asking for every title with "french" in it.
        let mut consumed = tokens;
        for word in &rest {
            if let Some(at) = consumed.iter().position(|c| c == word) {
                consumed.remove(at);
            }
        }
        rest = words(&negation.kept)
            .into_iter()
            .filter(|word| match consumed.iter().position(|c| c == word) {
                Some(at) => {
                    consumed.remove(at);
                    false
                }
                None => true,
            })
            .collect();
    }
    let excluded = read_excluded(&negation.excluded, indexes, &label_names);
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
    Parsed {
        title_query,
        text: whole,
        kept: normalized(&negation.kept),
        credits,
        lambda: rest.len() as f64 / total as f64,
        title_lambda: unread as f64 / total as f64,
        contested,
        leftover: rest.join(" "),
        facet,
        genres,
        labels,
        plot,
        source_kinds,
        runtime_max: None,
        broadcaster: None,
        year_from_param: false,
        people,
        makers,
        excluded,
    }
}

/// The ruled-out phrases, each read by the facet parser and the same tables as the words asked for.
fn read_excluded(phrases: &[String], indexes: &Indexes, label_names: &[(String, &str, bool)]) -> Excluded {
    let mut out = Excluded { phrases: phrases.to_vec(), ..Excluded::default() };
    for phrase in phrases {
        let facet = FacetQuery::parse(phrase);
        out.countries.extend(facet.country);
        out.decades.extend(facet.decade);
        out.media_types.extend(facet.media_type);
        // A one-word name is never read here: "without batman" means the character, not someone called Batman.
        let names = read_names(&words(&facet.leftover), false, indexes, label_names);
        let named_facet = facet.country.is_some() || facet.decade.is_some() || facet.media_type.is_some();
        let franchises = !names.names_anything() && !named_facet;
        let called = trim_filler(&names.rest);
        if !called.is_empty() {
            out.titles.extend(titles_called(indexes, called, franchises));
        }
        let into = &mut out.names;
        into.genres.extend(names.genres);
        into.labels.extend(names.labels);
        into.plot.extend(names.plot);
        into.source_kinds |= names.source_kinds;
        into.people.extend(names.people);
    }
    out
}

/// The popularity of every title the text is exactly the name of (a leading article aside), by the displayed
/// titles and the names they also go by.
fn exact_titles(indexes: &Indexes, text: &str) -> Vec<f64> {
    let Some(display) = &indexes.display else { return Vec::new() };
    let title_query = TitleQuery::new(text);
    let votes = |kind, id| indexes.facets.as_ref().and_then(|f| f.title(id, kind)).map_or(0, |t| t.votes);
    display
        .search_with(text, None, TITLE_LANE, MIN_COVERAGE)
        .iter()
        .filter(|h| title_query.score(h.title).1)
        .map(|h| popularity(votes(title_type(h.media_type), h.tmdb_id), None))
        .collect()
}

fn trim_filler(phrase: &[String]) -> &[String] {
    let filler = |w: &String| FILLER.contains(&w.as_str());
    let start = phrase.iter().position(|w| !filler(w)).unwrap_or(phrase.len());
    let end = phrase.iter().rposition(|w| !filler(w)).map_or(start, |at| at + 1);
    &phrase[start..end]
}

/// Titles the phrase ruled out BY NAME: every title whose words contain the phrase's words in order ("star wars"
/// → *Lego Star Wars: Revenge of the Brick*). When `franchises`, a title that LEADS with the phrase also rules
/// out the rest of its series (P179) — *Batman Begins* takes *The Dark Knight* with it, which no title match
/// can — but one that merely contains it does not: *The Lego Batman Movie* must not take *The Lego Movie*.
fn titles_called(indexes: &Indexes, phrase: &[String], franchises: bool) -> HashSet<Key> {
    let mut titles = HashSet::new();
    let Some(display) = &indexes.display else { return titles };
    let mut series: HashSet<u32> = HashSet::new();
    for hit in display.search_with(&phrase.join(" "), None, EXCLUDED_TITLES, MIN_COVERAGE) {
        let title = words(hit.title);
        let Some(at) = title.windows(phrase.len()).position(|w| w == phrase) else { continue };
        let key = (title_type(hit.media_type), hit.tmdb_id);
        titles.insert(key);
        let leads = at == 0 || (at == 1 && ["the", "a", "an"].contains(&title[0].as_str()));
        // Its most specific series only: a broader one ("Batman in film") would take titles the words
        // never named.
        if franchises && leads {
            let record = indexes.facts.as_ref().and_then(|f| f.get(key.1, key.0));
            series.extend(record.and_then(|r| r.franchise.first().copied()));
        }
    }
    if let (false, Some(facts)) = (series.is_empty(), indexes.facts.as_ref()) {
        let in_series = |&(kind, id): &Key| {
            facts.get(id, kind).is_some_and(|r| r.franchise.iter().any(|f| series.contains(f)))
        };
        titles.extend(facts.keys().filter(in_series));
    }
    titles
}

fn title_type(kind: den_titlesearch::MediaType) -> MediaType {
    match kind {
        den_titlesearch::MediaType::Movie => MediaType::Movie,
        den_titlesearch::MediaType::Tv => MediaType::Tv,
    }
}

/// The longest phrases among `tokens` that name a source kind, plot facet, genre, label or person. A person is read
/// from one word only when `one_word_names`.
fn read_names(
    tokens: &[String],
    one_word_names: bool,
    indexes: &Indexes,
    label_names: &[(String, &str, bool)],
) -> Names {
    let (mut genres, mut labels, mut plot, mut rest) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut source_kinds: u16 = 0;
    let mut people: Vec<u32> = Vec::new();
    // Where the last boost-only phrase read so far ends: a word before it was read, even though it stays.
    let mut read_until = 0;
    let mut unread = 0;
    let mut at = 0;
    // A matcher that can only LIFT a title claims its facet without eating the word; one that decides which
    // titles are eligible at all consumes it.
    //
    // Consuming everything was quietly expensive. `rest` is what the plot vectors are asked about and what
    // sets λ, so a single word claimed by a boost-only table emptied both: `q=medieval` answered with the 43
    // titles on that facet, led by the 2022 film *Medieval* and twelve of its neighbours, where `q=medieval
    // knights` — one word longer, vectors alive — answered with A Knight's Tale. And since a non-exact title
    // scores `t = score · λ`, `q=drama` gave EVERY fuzzy title match a score of zero.
    //
    // Genres, labels and plot facets only ever add weight, so nothing is lost by leaving their words in play;
    // the facet still lifts, and the query still means what it says.
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
            let mut read = false;
            for &(words, axis, value) in PLOT_PHRASES {
                if words == phrase {
                    read = true;
                    if !plot.contains(&(axis, value)) {
                        plot.push((axis, value));
                    }
                }
            }
            for &(words, genre) in GENRES {
                if words == phrase {
                    read = true;
                    if !genres.contains(&genre) {
                        genres.push(genre);
                    }
                }
            }
            for (folded, name, mood) in label_names {
                if *folded == phrase {
                    read = true;
                    if !labels.iter().any(|(n, m)| n == name && m == mood) {
                        labels.push(((*name).to_owned(), *mood));
                    }
                }
            }
            if read {
                read_until = read_until.max(at + span);
            }
            // A one-word name ("Nolan", "Common") counts only as the whole query: inside a longer one it is more
            // likely just a word. Even then it only lifts, and keeps the word: names are matched with their
            // aliases, so `dinosaur movies` read a one-credit person called that and answered with the single
            // film they appear in.
            if let Some(facts) = indexes.facts.as_ref().filter(|_| span >= 2 || one_word_names) {
                let going_by = facts.people_named(&phrase);
                matched |= span >= 2 && !going_by.is_empty();
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
        if at >= read_until {
            unread += 1;
        }
        rest.push(tokens[at].clone());
        at += 1;
    }
    Names { genres, labels, plot, source_kinds, people, rest, unread }
}

/// What the lanes found about a candidate before scoring.
#[derive(Default)]
struct Found {
    /// Its title in TMDB's export, and that export's popularity.
    export: Option<(String, f64)>,
    /// The names it matched by in the display title index: its displayed title, or another name it goes by.
    names: Vec<String>,
    /// How far its plot and premise vectors stand above their own scans' means, in standard deviations.
    /// The two distributions are normalised separately: their raw dot products are not one score space.
    plot_z: Option<f64>,
    premise_z: Option<f64>,
}

/// A candidate's features and score.
struct Scored {
    key: Key,
    score: f64,
    exact: bool,
    t: f64,
    plot_sem: f64,
    premise_sem: f64,
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
    // A `type=` PARAMETER wins outright; the word "show" or "movie" in the query is only a default for when
    // none was given. ANDing the two let them annihilate — `?q=the show&type=movie` answered with nothing at
    // all, because the words said series and the caller said film.
    let wanted = |kind: MediaType| match media_type.or(parsed.facet.media_type) {
        Some(want) => want == kind,
        None => !parsed.excluded.media_types.contains(&kind),
    };
    // What the query ruled out by title or plot facet, worked out once; the rest is read per title.
    let mut ruled_out: HashSet<Key> = parsed.excluded.titles.clone();
    if let Some(plot_facets) = &indexes.plot_facets {
        for &(axis, value) in &parsed.excluded.names.plot {
            for kind in [MediaType::Movie, MediaType::Tv] {
                let facet = [(axis.to_owned(), value.to_owned())];
                ruled_out.extend(plot_facets.matching(kind, &facet).into_iter().map(|(key, _)| key));
            }
        }
    }
    // Every lane drops what was ruled out BEFORE it truncates, or a lane of the 500 most-voted comedies is
    // mostly American and "comedies not american" is left with what the vectors happen to find.
    let allowed = |key: Key| !rules_out(indexes, parsed, &ruled_out, key);
    let mut found: HashMap<Key, Found> = HashMap::new();

    // Titles, by the name TMDB exports and the name atlas displays.
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
            f.filter_all(
                parsed.facet.media_type,
                parsed.facet.country,
                parsed.facet.language.as_deref(),
                parsed.facet.decade,
                parsed.facet.year_min,
                parsed.facet.year_max,
            )
            .into_iter()
            .map(|(id, kind)| (kind, id))
            .collect()
        });
    for &key in facet_titles.iter().flatten().filter(|&&key| allowed(key)).take(FACET_LANE) {
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
        if let Some(qid) = parsed.broadcaster {
            named_titles.extend(facts.titles_on_broadcaster(qid));
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
        named_titles.retain(|&key| allowed(key));
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
    // The plot and premise vectors' nearest to the leftover, within the facet when there is one and it is not
    // contested. Each scan is standardised against its own corpus distribution; taking their stronger normalised
    // answer below lets the premise representation propose a title without doubling the semantic term's weight.
    //
    // A contested facet does not bound them: `russian doll` read RU, and the time-loop shows the words also
    // describe were unreachable because none of them is Russian.
    if let Some(vector) = vector {
        let bound = facet_set.as_ref().filter(|_| !parsed.contested);
        let eligible = |id: u32, kind: MediaType| {
            wanted(kind) && bound.is_none_or(|set| set.contains(&(kind, id))) && allowed((kind, id))
        };
        let (near, stats) = indexes.plot.scan_vector(vector, eligible, LANE);
        if stats.sd > 0.0 {
            for n in near {
                found.entry((n.media_type, n.tmdb_id)).or_default().plot_z =
                    Some((f64::from(n.score) - stats.mean) / stats.sd);
            }
        }
        if let Some(premise) = &indexes.premise {
            let (near, stats) = premise.scan_vector(vector, eligible, LANE);
            if stats.sd > 0.0 {
                for n in near {
                    found.entry((n.media_type, n.tmdb_id)).or_default().premise_z =
                        Some((f64::from(n.score) - stats.mean) / stats.sd);
                }
            }
        }
    }

    let mut scored: Vec<Scored> = found
        .iter()
        .filter(|(key, _)| wanted(key.0) && allowed(**key))
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

    // An exact title leads the other titles the query NAMED, then the theme-only tail. More Like This used to
    // insert twelve additional neighbours here with score 0.0, after the retain above had correctly dropped
    // every irrelevant candidate. That made a full-looking result list by contradicting the score contract.
    if scored.first().is_some_and(|s| s.exact) {
        let mut iter = scored.into_iter();
        let first = iter.next().expect("a top hit");
        let ranked: Vec<Scored> = iter.collect();
        scored = order_around_exact(first, ranked);
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
                "primaryGenre": crate::plotrows::primary_genre(indexes, s.key),
                "f": {"t": round(s.t), "sem": round(s.sem), "semPlot": round(s.plot_sem),
                      "semPremise": round(s.premise_sem), "lab": round(s.lab), "pf": round(s.pf),
                      "p": s.person, "pop": round(s.pop), "phi": s.phi},
            });
            // Its IMDb id, which a client's availability check keys streams by: without it the client asks TMDB
            // for it, a request a card.
            if let Some(imdb) =
                indexes.facts.as_ref().and_then(|f| f.get(id, kind)).and_then(|r| r.imdb_id.as_deref())
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
            "language": parsed.facet.language,
            "runtimeMax": parsed.runtime_max,
            "broadcaster": parsed.broadcaster,
            "leftover": parsed.leftover,
            "lambda": round(parsed.lambda),
            "titleLambda": round(parsed.title_lambda),
            "facetContested": parsed.contested,
            "excluded": excluded_json(&parsed.excluded),
        },
        "people": people,
        "hits": hits,
        "total": total,
        // Repeated from the schema so a client reading only this answer cannot take `total` for a count of the
        // corpus: it is the pool the ranking drew from, and a constraint read from words only discounts.
        "semantics": { "total": "retrievedCandidatesNotCorpusCount", "missing": "unknown" },
        "coverage": crate::schema::query_coverage(
            indexes,
            media_type.or(parsed.facet.media_type),
            &applied(parsed, media_type),
        ),
    })
}

/// What the query ruled out, as `parse.excluded` reports it; `null` when it ruled out nothing.
fn excluded_json(excluded: &Excluded) -> serde_json::Value {
    if excluded.is_empty() {
        return serde_json::Value::Null;
    }
    let names = &excluded.names;
    let people: Vec<String> = names.people.iter().map(|qid| format!("Q{qid}")).collect();
    serde_json::json!({
        "phrases": excluded.phrases,
        "countries": excluded.countries,
        "decades": excluded.decades,
        "mediaTypes": excluded.media_types.iter()
            .map(|&t| if t == MediaType::Tv { "series" } else { "movie" }).collect::<Vec<_>>(),
        "genres": names.genres,
        "labels": names.labels.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "plotFacets": names.plot.iter().map(|(axis, value)| format!("{axis}={value}")).collect::<Vec<_>>(),
        "basedOnKind": SourceKinds::names(names.source_kinds),
        "people": people,
        "titles": excluded.titles.len(),
    })
}

/// Every constraint the query applied, and how: a parameter filters, a constraint read from the words discounts,
/// and what only lifts a title boosts — the same rules `features` scores by.
fn applied(parsed: &Parsed, media_type: Option<MediaType>) -> Vec<crate::schema::Applied> {
    use crate::schema::Applied;
    use serde_json::json;
    let kind = |t: MediaType| if t == MediaType::Tv { "series" } else { "movie" };
    let mut out = Vec::new();
    let mut push =
        |field, value, applied, applies_to| out.push(Applied { field, value, applied, applies_to });
    if let Some(t) = media_type.or(parsed.facet.media_type) {
        push("mediaType", json!(kind(t)), "filter", None);
    }
    // Read from the words, a facet discounts what it does not hold; contested by a title, it only lifts.
    let inferred = if parsed.contested { "boost" } else { "discount" };
    if let Some(country) = parsed.facet.country {
        push("country", json!(country), inferred, None);
    }
    if let Some(decade) = parsed.facet.decade {
        push("decade", json!(decade), inferred, None);
    }
    if parsed.facet.year_min.is_some() || parsed.facet.year_max.is_some() {
        let window = json!({ "min": parsed.facet.year_min, "max": parsed.facet.year_max });
        push("year", window, if parsed.year_from_param { "filter" } else { inferred }, None);
    }
    if let Some(language) = &parsed.facet.language {
        push("language", json!(language), "filter", None);
    }
    if let Some(minutes) = parsed.runtime_max {
        // Films on record as longer drop; every series is only discounted, its runtime being per episode.
        push("runtimeMinutes", json!({ "max": minutes }), "filter", Some(MediaType::Movie));
    }
    if let Some(qid) = parsed.broadcaster {
        // A series with no broadcaster on record drops too; films are not judged at all.
        push("broadcaster", json!(format!("Q{qid}")), "require", Some(MediaType::Tv));
    }
    if parsed.source_kinds != 0 {
        push("basedOnKind", json!(SourceKinds::names(parsed.source_kinds)), "filter", None);
    }
    if !parsed.genres.is_empty() {
        push("genre", json!(parsed.genres), "boost", None);
    }
    for (field, mood) in [("subgenre", false), ("mood", true)] {
        let names: Vec<&str> =
            parsed.labels.iter().filter(|(_, m)| *m == mood).map(|(name, _)| name.as_str()).collect();
        if !names.is_empty() {
            push(field, json!(names), "boost", None);
        }
    }
    let mut axes: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for &(axis, value) in &parsed.plot {
        axes.entry(axis).or_default().push(value);
    }
    for (axis, values) in axes {
        push(axis, json!(values), "boost", None);
    }
    if !parsed.people.is_empty() {
        let qids: Vec<String> = parsed.people.iter().map(|qid| format!("Q{qid}")).collect();
        push("people", json!(qids), "boost", None);
    }
    out
}

/// How popular a title is, 0 to 1: by its votes (the store), else by TMDB's popularity in the daily export, for
/// a title the store has no count for.
pub(crate) fn popularity(votes: u32, export: Option<f64>) -> f64 {
    attention(votes, export).min(1.0)
}

/// `popularity` without its ceiling, for ordering: past "fully popular", a title with more votes still comes first.
///
/// One scale. The export branch is converted to an equivalent vote count first, rather than divided by a
/// "fully popular" constant of its own — two separate normalisations agree at the anchor by construction
/// and nowhere else, because a log ratio grows faster the smaller its denominator. Measured against the
/// 2026-09-20 export, that let three titles with no vote count at all outrank the corpus's most-voted
/// one: Resident Evil scored 1.608 on popularity 556 against Inception's 1.245 on 40,175 votes.
pub(crate) fn attention(votes: u32, export: Option<f64>) -> f64 {
    let votes = match export {
        _ if votes > 0 => f64::from(votes),
        // Never above the anchor: a title we have no count for may reach "fully popular" on the export's
        // word, but it may not pass a title that earned the same place with votes. The clamp is also
        // where the conversion is least trustworthy — see VOTES_PER_POPULARITY.
        Some(popularity) => (popularity.max(0.0) * VOTES_PER_POPULARITY).min(POPULAR_VOTES),
        None => return 0.0,
    };
    votes.ln_1p() / POPULAR_VOTES.ln_1p()
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

/// Plot prose and premise tags are different representations with different dot-product distributions. Each
/// becomes a 0...1 signal against its own scan, then the stronger one spends the existing semantic budget —
/// never their sum, which would count the same query twice and break the exact-title invariant.
fn semantic_evidence(found: &Found) -> (f64, f64, f64) {
    let strength =
        |z: Option<f64>| z.map_or(0.0, |z| ((z - SEMANTIC_FLOOR_Z) / SEMANTIC_SPAN_Z).clamp(0.0, 1.0));
    let plot = strength(found.plot_z);
    let premise = strength(found.premise_z);
    (plot, premise, plot.max(premise))
}

/// Whether the query ruled a title out: it is on record as having something a negation named, or is in
/// `ruled_out` (called by a ruled-out name, or carrying a ruled-out plot facet). A title with no record of the
/// thing is kept — unknown is not a match, here as everywhere.
fn rules_out(indexes: &Indexes, parsed: &Parsed, ruled_out: &HashSet<Key>, key: Key) -> bool {
    let excluded = &parsed.excluded;
    if excluded.is_empty() {
        return false;
    }
    if ruled_out.contains(&key) || excluded.media_types.contains(&key.0) {
        return true;
    }
    let (kind, id) = key;
    let record = indexes.facts.as_ref().and_then(|f| f.get(id, kind));
    let facets = indexes.facets.as_ref().and_then(|f| f.title(id, kind));
    if !excluded.countries.is_empty() {
        let recorded = record.map(|r| r.countries.as_slice()).unwrap_or_default();
        let mut known = facets.and_then(|f| f.country).into_iter().chain(recorded.iter().copied());
        if known.any(|code| excluded.countries.iter().any(|c| c.as_bytes() == code)) {
            return true;
        }
    }
    if !excluded.decades.is_empty() {
        let year = facets
            .and_then(|f| f.year)
            .map(i64::from)
            .or_else(|| record.and_then(|r| r.released).map(|r| r.year_of()));
        if year.is_some_and(|y| excluded.decades.iter().any(|&d| y.div_euclid(10) * 10 == i64::from(d))) {
            return true;
        }
    }
    let names = &excluded.names;
    if !names.genres.is_empty()
        && crate::plotrows::genres(indexes, key).iter().any(|g| names.genres.contains(g))
    {
        return true;
    }
    if !names.labels.is_empty() {
        if let Some(labels) = indexes.plot.labels(id, kind) {
            let carries = |(name, mood): &(String, bool)| {
                let pairs = if *mood { &labels.moods } else { &labels.subgenres };
                pairs.iter().any(|(n, confidence)| *n == name.as_str() && *confidence >= LABEL_FLOOR)
            };
            if names.labels.iter().any(carries) {
                return true;
            }
        }
    }
    if let Some(r) = record {
        if names.source_kinds != 0 && r.source_kinds.raw() & names.source_kinds != 0 {
            return true;
        }
        if r.makers.iter().chain(&r.cast).any(|q| names.people.contains(q)) {
            return true;
        }
    }
    false
}

/// A facet guessed from query text is an interpretation, not an explicit constraint. Keep its mismatch penalty
/// for ordinary candidates, but never let it break the exact-title floor, nor apply it when the same words also
/// name a title (`spared`). Any penalty already in `phi` came from another constraint or unknown data and
/// remains intact.
fn discount_text_facet(phi: f64, spared: bool) -> f64 {
    if spared {
        phi
    } else {
        phi * WRONG_TEXT_FACET
    }
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

    let pop = popularity(facets.map_or(0, |f| f.votes), found.export.as_ref().map(|e| e.1));

    // T is known before applying inferred facet penalties because an exact name resolves that ambiguity. A
    // caller-supplied constraint still filters below; only a facet guessed from these same words yields to it.
    let card = indexes.cards.as_ref().and_then(|cards| cards.get(&key)).map(|c| c.title.as_str());
    let mut near: f64 = 0.0;
    let mut exact = false;
    let also_named = found.names.iter().map(String::as_str);
    for title in found.export.as_ref().map(|e| e.0.as_str()).into_iter().chain(card).chain(also_named) {
        let (score, is_exact) = parsed.title_query.score(title);
        exact |= is_exact;
        near = near.max(score);
    }
    // Neither an exact title nor any title of a query whose facet reading is contested is discounted for
    // contradicting a facet read from the words.
    let spared = exact || parsed.contested;

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
            // Read out of the words, so it may only discount. A demonym is as often part of a TITLE as it is
            // a claim about origin, and the corpus shows the guess is wrong about half the time: "spanish" as
            // a country misses 1,138 Spanish-LANGUAGE titles. An exact title keeps both readings alive.
            phi = discount_text_facet(phi, spared);
        }
    }
    // The broadcaster, from a parameter, so it drops — but only for series, since a film has no such fact
    // and would otherwise be judged against a statement that cannot exist for it.
    if let Some(qid) = parsed.broadcaster {
        if key.0 == MediaType::Tv && !record.is_some_and(|r| r.broadcasters.contains(&qid)) {
            return None;
        }
    }

    // The original language, from a parameter, so it drops. 93.5% coverage, and the caller meant exactly
    // this — unlike a demonym read from prose, which is why that one only discounts.
    //
    // Applied HERE and not only in the facet lane: the source-kind, label and semantic lanes propose titles
    // of their own, and without this `?q=based on a book&language=es` answered with Fight Club and Shawshank
    // — the filter was building a candidate set nobody was checking against.
    if let Some(want) = parsed.facet.language.as_deref() {
        let known: Vec<String> = facets
            .and_then(|f| f.language)
            .map(|code| String::from_utf8_lossy(&code).to_string())
            .into_iter()
            .chain(
                record
                    .map(|r| {
                        r.languages.iter().map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            )
            .collect();
        if known.is_empty() {
            phi *= UNKNOWN_FACET;
        } else if !known.iter().any(|code| code.eq_ignore_ascii_case(want)) {
            return None;
        }
    }

    // An upper bound on runtime, from a parameter, so it drops — on FILMS. Wikidata states a series' runtime
    // per EPISODE, so the same number means a 45-minute drama episode and not a short film; a 20-episode
    // season is not shorter than a feature.
    //
    // A series is therefore DISCOUNTED rather than passed free. Leaving it unjudged looked like the humble
    // choice and was the wrong one: with every long film dropped, `runtime_max=95` answered with Chernobyl,
    // Sherlock and Dexter — series outranking the films the question was about. Unknown must cost something,
    // or it wins by default.
    if let Some(max) = parsed.runtime_max {
        match record.and_then(|r| r.runtime_minutes) {
            _ if key.0 != MediaType::Movie => phi *= UNKNOWN_FACET,
            Some(minutes) if minutes > max => return None,
            Some(_) => {}
            None => phi *= UNKNOWN_FACET,
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
                let outside = parsed.facet.year_min.is_some_and(|min| year < i64::from(min))
                    || parsed.facet.year_max.is_some_and(|max| year > i64::from(max));
                if outside {
                    // A window the CALLER gave is a real constraint; one read out of the words is a guess.
                    if parsed.year_from_param {
                        return None;
                    }
                    phi = discount_text_facet(phi, spared);
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
            Some(year) if year.div_euclid(10) * 10 != i64::from(decade) => {
                phi = discount_text_facet(phi, spared);
            }
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
    //
    // Read from the words like a country, so it yields to an exact title the same way: `black books` names
    // the sitcom, which is adapted from nothing.
    if parsed.source_kinds != 0 && !exact {
        match record.map(|r| r.source_kinds) {
            Some(kinds) if kinds.is_empty() => phi *= UNKNOWN_FACET,
            Some(kinds) if kinds.raw() & parsed.source_kinds != 0 => {}
            Some(_) => return None,
            None => phi *= UNKNOWN_FACET,
        }
    }

    let (plot_sem, premise_sem, sem) = semantic_evidence(found);

    let mut lab: f64 = 0.0;
    if !parsed.genres.is_empty()
        && crate::plotrows::genres(indexes, key).iter().any(|g| parsed.genres.contains(g))
    {
        lab = 1.0;
    }
    let mut labelled = false;
    if !parsed.labels.is_empty() {
        if let Some(labels) = indexes.plot.labels(id, kind) {
            for (name, mood) in &parsed.labels {
                let pairs = if *mood { &labels.moods } else { &labels.subgenres };
                if let Some(&(_, confidence)) = pairs.iter().find(|(n, _)| *n == name.as_str()) {
                    if confidence >= LABEL_FLOOR {
                        lab = lab.max(confidence);
                        labelled = true;
                    }
                }
            }
        }
    }
    let pf = plot_confidence.get(&key).copied().unwrap_or(0.0);
    // A near title match counts for the words no table reads — unless the title also carries the label or plot
    // facet those words named, when name and theme agree: The Time Traveler's Wife is a time-travel film, where
    // Leak is not bleak. A genre is too broad to agree with: every film called Love Story is a romance.
    let share = if labelled || pf > 0.0 { parsed.lambda } else { parsed.title_lambda };
    let t = if exact { EXACT_TITLE + 0.4 * pop } else { near * share };
    // A maker's title they made, an actor's title they appear in: their own role. Anyone named, otherwise: the other.
    let in_role = |qids: &[u32], making: bool| {
        qids.iter().any(|q| parsed.people.contains(q) && parsed.makers.contains(q) == making)
    };
    let person = match record {
        Some(r) if in_role(&r.makers, true) || in_role(&r.cast, false) => 1.0,
        Some(r) if r.makers.iter().chain(&r.cast).any(|q| parsed.people.contains(q)) => OTHER_ROLE,
        _ => 0.0,
    };

    Some(Scored { key, score: 0.0, exact, t, plot_sem, premise_sem, sem, lab, pf, person, pop, phi })
}

/// The order around an exact title: the title itself, then everything else the query matched by name in the
/// order its score earned, then the theme-only tail. `ranked` contains only positive-score candidates: this
/// function must never make a query look fuller by inventing zero-score More Like This answers.
fn order_around_exact(first: Scored, ranked: Vec<Scored>) -> Vec<Scored> {
    let (named, tail): (Vec<Scored>, Vec<Scored>) = ranked.into_iter().partition(|s| s.t > 0.0);
    let mut ordered = Vec::with_capacity(1 + named.len() + tail.len());
    ordered.push(first);
    ordered.extend(named);
    ordered.extend(tail);
    ordered
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

    /// A boost-only matcher must claim its facet WITHOUT eating the word. `rest` is what the vectors are
    /// asked about and what sets λ, so consuming it emptied both — `q=medieval` answered with 43 titles led
    /// by the 2022 film of that name, and `q=drama` gave every fuzzy title match a score of zero.
    #[test]
    fn a_genre_or_theme_word_stays_in_the_query() {
        // Standing in for `parse`, which needs the indexes: the loop's contract is that SOURCE_PHRASES
        // consume and the boost-only tables do not.
        let consumed = |query: &str| {
            let tokens = words(query);
            let mut rest: Vec<String> = Vec::new();
            let mut at = 0;
            'words: while at < tokens.len() {
                for span in (1..=MAX_PHRASE_WORDS.min(tokens.len() - at)).rev() {
                    let phrase = tokens[at..at + span].join(" ");
                    if SOURCE_PHRASES.iter().any(|&(w, _)| w == phrase) {
                        at += span;
                        continue 'words;
                    }
                }
                rest.push(tokens[at].clone());
                at += 1;
            }
            rest.join(" ")
        };
        // Boost-only words survive, so the vectors still see them and λ stays > 0.
        assert_eq!(consumed("medieval"), "medieval");
        assert_eq!(consumed("drama"), "drama");
        assert_eq!(consumed("slow burn"), "slow burn");
        assert_eq!(consumed("bleak thriller"), "bleak thriller");
        // A source phrase decides eligibility, so it is still eaten.
        assert_eq!(consumed("based on a book"), "");
        assert_eq!(consumed("recent based on a novel"), "recent");
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
    fn an_inferred_facet_cannot_discount_an_exact_title() {
        let (_, exact) = TitleQuery::new("brazil").score("Brazil");
        assert!(exact);
        assert_eq!(discount_text_facet(1.0, exact), 1.0);
        assert_eq!(
            discount_text_facet(UNKNOWN_FACET, exact),
            UNKNOWN_FACET,
            "an unrelated unknown-data penalty remains"
        );
        assert_eq!(discount_text_facet(1.0, false), WRONG_TEXT_FACET);
    }

    /// The fixture store's indexes, with movie 1 (Korean and Danish, adapted from a book and a play, a Heist
    /// film carrying tone=bleak) displayed as `movie_one`.
    fn fixture(name: &str, movie_one: &str) -> Indexes {
        let dir = std::env::temp_dir().join(format!("den-atlas-search-{name}-{}", std::process::id()));
        crate::queries::load_for_tools(&crate::queries::write_fixture_titled(&dir, movie_one)).unwrap()
    }

    fn hit<'a>(answer: &'a serde_json::Value, kind: &str, id: u32) -> &'a serde_json::Value {
        let hits = answer["hits"].as_array().unwrap();
        hits.iter().find(|h| h["type"] == kind && h["id"] == id).unwrap_or_else(|| panic!("{answer}"))
    }

    /// `spanish heist` reads ES. When no title is called that, a Korean heist contradicts the facet and is
    /// discounted; when movie 1 is called Spanish Heist, the words are a title as much as a facet, so nothing
    /// is discounted for not being Spanish and the vectors are asked about both words.
    #[test]
    fn a_facet_the_words_share_with_a_popular_title_is_contested() {
        let plain = fixture("uncontested", "One");
        let parsed = parse("spanish heist", &plain);
        assert!(!parsed.contested);
        assert_eq!((parsed.facet.country, parsed.leftover.as_str()), (Some("ES"), "heist"));
        let discounted = answer(&plain, None, &parsed, None, None, 0, PAGE);
        assert_eq!(hit(&discounted, "movie", 2)["f"]["phi"], WRONG_TEXT_FACET);

        let titled = fixture("contested", "Spanish Heist");
        let parsed = parse("spanish heist", &titled);
        assert!(parsed.contested);
        assert_eq!(parsed.facet.country, Some("ES"), "the facet reading is kept");
        assert_eq!(parsed.embed_text(), Some("spanish heist"));
        let both = answer(&titled, None, &parsed, None, None, 0, PAGE);
        assert_eq!(both["hits"][0]["id"], 1, "{both}");
        assert_eq!(hit(&both, "movie", 2)["f"]["phi"], 1.0);
        assert_eq!(both["coverage"]["fields"]["country"]["applied"], "boost");
    }

    /// A near title match counts for the words no table reads: `bleak` asks for the theme, not for Leak.
    #[test]
    fn words_a_table_reads_do_not_ask_for_titles_that_contain_them() {
        let indexes = fixture("title-lambda", "One");
        let bleak = parse("bleak", &indexes);
        assert_eq!((bleak.lambda, bleak.title_lambda), (1.0, 0.0), "the word stays for the vectors");
        assert_eq!(parse("bleak zzzz", &indexes).title_lambda, 0.5);
        assert_eq!(parse("slow burn zzzz", &indexes).title_lambda, 1.0 / 3.0);
        assert_eq!(parse("zzzz", &indexes).title_lambda, 1.0);
    }

    /// `books` and `love stories` name kinds of film.
    #[test]
    fn a_kind_of_film_is_read_as_one() {
        let indexes = fixture("kinds", "One");
        let books = parse("books", &indexes);
        assert_eq!((books.source_kinds, books.leftover.as_str()), (SourceKinds::BOOK, ""));
        assert_eq!(books.embed_text(), None, "a source kind is nothing to ask the vectors");
        assert_eq!(parse("comic book movies", &indexes).source_kinds, SourceKinds::COMIC);
        assert_eq!(parse("love stories", &indexes).genres, vec![10749]);
    }

    /// A source kind read from the words yields to an exact title like a country does: movie 1 is adapted
    /// from a book and a play, and used to be dropped outright for `graphic novels`.
    #[test]
    fn a_source_kind_read_from_the_words_keeps_an_exact_title() {
        let indexes = fixture("source-exact", "Graphic Novels");
        let parsed = parse("graphic novels", &indexes);
        assert_eq!(parsed.source_kinds, SourceKinds::COMIC);
        let found = answer(&indexes, None, &parsed, None, None, 0, PAGE);
        assert_eq!(found["hits"][0]["id"], 1, "{found}");
        assert!(found["hits"][0]["score"].as_f64().unwrap() >= W_TITLE * EXACT_TITLE, "{found}");
    }

    #[test]
    fn premise_and_plot_are_normalised_separately_and_share_one_semantic_budget() {
        let plot_only = Found { plot_z: Some(3.2), ..Found::default() };
        let (plot, premise, combined) = semantic_evidence(&plot_only);
        assert!((plot - 0.2).abs() < 1e-9 && premise == 0.0 && combined == plot);

        let both = Found { plot_z: Some(3.2), premise_z: Some(4.6), ..Found::default() };
        let (plot, premise, combined) = semantic_evidence(&both);
        assert!((plot - 0.2).abs() < 1e-9);
        assert!((premise - 0.6).abs() < 1e-9);
        assert_eq!(combined, premise, "the stronger lane wins; the two lanes are not added");
    }

    #[test]
    fn popularity_reads_votes_and_else_the_export() {
        assert_eq!(popularity(5000, Some(1.0)), 1.0, "votes win");
        assert_eq!(popularity(0, Some(1000.0)), 1.0, "the export can reach fully popular");
        assert!(popularity(0, Some(1.0)) < popularity(0, Some(10.0)), "and orders below it");
        assert_eq!(popularity(0, None), 0.0);
        assert!(attention(30_000, None) > attention(5000, None), "ordering keeps apart what popularity caps");
    }

    /// The export branch used to be normalised by a "fully popular" constant of its own, which agreed with
    /// the vote branch at 1.0 and diverged above it. Against the 2026-09-20 export that put three titles
    /// with no vote count at all above the most-voted title in the corpus — Resident Evil's popularity of
    /// 556 scored 1.608 where Inception's 40,175 votes scored 1.245.
    #[test]
    fn a_title_with_no_votes_cannot_outrank_one_with_forty_thousand() {
        let inception = attention(40_175, None);
        for trending in [556.7, 167.0, 133.0, 5_000.0] {
            assert!(
                attention(0, Some(trending)) < inception,
                "popularity {trending} outranked 40,175 votes ({} vs {inception})",
                attention(0, Some(trending))
            );
        }
        // ...and it still orders them against each other below that line.
        assert!(attention(0, Some(10.0)) > attention(0, Some(1.0)));
    }

    /// `hobbit` is exactly the 1977 animated film. Search used to put twelve More Like This neighbours carrying
    /// score 0 directly behind it. Exact-title ordering may rearrange real query matches; it must not manufacture
    /// candidates that the scoring contract already rejected.
    #[test]
    fn exact_title_ordering_contains_only_the_queries_scored_candidates() {
        let hit = |id, score, t| Scored {
            key: (MediaType::Movie, id),
            score,
            exact: false,
            t,
            plot_sem: 0.0,
            premise_sem: 0.0,
            sem: 0.0,
            lab: 0.0,
            pf: 0.0,
            person: 0.0,
            pop: 1.0,
            phi: 1.0,
        };
        let anchor = Scored { exact: true, ..hit(1362, 2.0055, 0.8775) };
        // As the ranking hands them over: sorted by score, named and theme-only alike.
        let ranked = vec![
            hit(122917, 1.0625, 0.4722), // The Hobbit: The Battle of the Five Armies
            hit(49051, 0.8127, 0.3241),  // The Hobbit: An Unexpected Journey
            hit(672, 0.2682, 0.0),       // Harry Potter and the Chamber of Secrets — theme only
        ];
        let ordered = order_around_exact(anchor, ranked);
        let ids: Vec<u32> = ordered.iter().map(|s| s.key.1).collect();
        assert_eq!(ids, vec![1362, 122917, 49051, 672]);
        assert!(ordered.iter().all(|s| s.score > 0.0));
    }

    #[test]
    fn popularity_counts_only_as_far_as_a_title_is_relevant() {
        let hit = |sem, pop| Scored {
            key: (MediaType::Movie, 1),
            score: 0.0,
            exact: false,
            t: 0.0,
            plot_sem: sem,
            premise_sem: 0.0,
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
