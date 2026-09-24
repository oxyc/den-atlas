//! `/index/filter/<movie|series|all>/…` — stackable filters over the corpus, for Den Web's Search
//! (oxyc/den#133, oxyc/den#134): a selection of values from many kinds (genre, language, decade, a mood, a plot
//! axis, a person, a studio, a rating, …) AND-ed together — an item may OR several values of its kind,
//! `country:FR|IT` — and three questions about it. `all` asks them of films and series together (`Scope`).
//!
//! - `counts.json` — for every value of every kind, how many titles carry the selection AND that value, so an
//!   option that would leave nothing is hidden instead of spending TMDB discover calls to find out.
//! - `titles.json` — the titles carrying the selection, paged, as the cards `/index/row` draws.
//! - `values/<kind>.json` — one kind's values under the selection, labelled, with a prefix search: the
//!   typeahead for the kinds too big to list whole (people, studios, characters).
//! - `people.json` and `people/counts.json` — the people credited on those titles, filtered by their own
//!   traits, and the traits' counts (`people.rs`).
//!
//! # What a count is
//!
//! Titles of the type (of both, under `all`), with a card, on record as carrying every selected value and the
//! counted one. Every kind's values are held over the store's rows of both types, so `all` is the same count
//! over both types' rows: a film genre id matches the series filed under it too (a series' genres are read as
//! films'), a series-only genre or kind (`network`) matches only series, and a composite id only series. A title
//! the corpus does not describe is unknown, not a negative, so a count is a floor on the corpus: 0 means
//! "nothing Den would show you". The facts' countries and languages count under EVERY value a title lists, so
//! a count errs high rather than hiding an option a title does carry. An exclusion (`-kind:id`) keeps only the
//! titles KNOWN not to carry the value — a title with nothing on record for the kind is dropped, never assumed
//! clean — and every answer reports how much of the type the kinds it applied are known for (`coverage`).
//!
//! # How it is stored
//!
//! Kinds with few values (≲2k) are one bitset per value over the store's rows, built once per index load; a
//! selection is an AND of bitsets and a count a popcount. Entity kinds (people, studios, subjects, places)
//! have far too many values for that: they keep posting lists (entity → rows) for selecting, and count by
//! walking the matched rows' own entity lists — read in place from the mapped store — then take the top
//! `TOP_K`. The empty selection's counts are worked out once. What depends on the rating provider (the
//! `rating` kind and the popularity order `titles.json` walks) is rebuilt when it swaps in new votes.
//!
//! # The providers are a seam
//!
//! Votes and ratings are read through `Indexes::ratings` (a `ratings::Ratings`, whose `RatingsIndex` answers
//! `votes(row)` and `of(row)`), and character names through `Indexes::characters`. `tmdb.rs` fills both from
//! TMDB, and this module reads nothing else. Either signal is only a filter and a sort here, never learned
//! from or published — the rules at the top of `tmdb.rs`.
//!
//! # The URL is the cache key
//!
//! An answer depends on the type, the route and its parameters alone, so identical questions must be the
//! same URL (`Request::parse`). One request whose spelling is not the canonical one is still answered — den-edge's
//! relay drops a redirect's `Location` — but privately and briefly, naming the canonical URL in
//! `Content-Location`.

use crate::characters::CharacterIndex;
use crate::queries::Indexes;
use crate::ratings::RatingsIndex;
use den_index::MediaType;
use den_titlesearch::TitleIndex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

mod people;

type Bits = Vec<u64>;
type Key = (MediaType, u32);
/// A kind table's reading of `<kind>:<id>` as its canonical pair.
type Normalise<'f> = &'f dyn Fn(&str, &str) -> Result<(String, String), String>;
/// Whether a kind table refuses a group of these normalised ids as one item.
type OneValue<'f> = &'f dyn Fn(&str, &[String]) -> bool;

/// The most values one selection may name.
pub const MAX_SELECTION: usize = 16;
/// The longest query string accepted, in bytes: sixteen values with room for long labels.
pub const MAX_QUERY: usize = 2048;
/// A titles page when the request names none, and the most one returns.
pub const PAGE: usize = crate::handler::ROW_PAGE;
pub const MAX_PAGE: usize = crate::handler::MAX_ROW_PAGE;
/// How many values an entity kind lists in `counts.json`, besides any selected.
pub const TOP_K: usize = 30;
/// The most values `values/<kind>.json` returns, and for `character`, which is search-only.
pub const VALUES_LIMIT: usize = 10;
pub const CHARACTER_LIMIT: usize = 5;
/// The shortest prefix a values search takes, and a character search.
pub const MIN_PREFIX: usize = 2;
pub const CHARACTER_MIN_PREFIX: usize = 3;
/// A rating counts only on this many votes.
pub const MIN_VOTES: u32 = 10;
/// The `rating` kind's values: the provider's average, out of 10, at or above each.
const RATING_THRESHOLDS: [u32; 3] = [6, 7, 8];
/// The `runtime` kind's buckets, in minutes: [from, to). A series' runtime is per episode.
const RUNTIME_BUCKETS: [(&str, u32, u32); 4] =
    [("under-90", 1, 90), ("90-120", 90, 120), ("120-150", 120, 150), ("over-150", 150, u32::MAX)];
/// TMDB's series-only genres that name two film genres at once (Action & Adventure, Sci-Fi & Fantasy,
/// War & Politics).
const SERIES_COMPOSITE_GENRES: [u16; 3] = [10759, 10765, 10768];
/// A year before this is unknown, as the facet index reads it.
const FIRST_YEAR: i64 = 1870;

/// How a kind's values combine within a selection. `And`: a title holds several (genres, people), so two
/// selected values both apply and each value is counted under the whole selection. `Single`: a title holds one
/// (its decade, its runtime), so a client picks one, and each value is counted as the alternative pick —
/// under the selection WITHOUT this kind's own values. Either takes an OR group (`decade:1980|1990`), and an
/// `And` kind with one counts without it (`counted_apart`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    And,
    Single,
}

/// Whether a kind's values are counted without this item of it: any item of a one-pick kind, whose values are
/// alternatives to its pick; a positive OR group of an and kind, whose other values are what the group could
/// add. An and kind's single values and exclusions narrow what its values are counted under.
fn counted_apart(mode: Mode, item: &Item) -> bool {
    mode == Mode::Single || (item.group() && !item.exclude)
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::And => "and",
            Mode::Single => "single",
        }
    }
}

/// How a kind's ids are written in canonical form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Id {
    /// A plain decimal integer.
    Integer,
    /// A decade by its first year: 1995 is written 1990.
    Decade,
    Lower,
    Upper,
    /// The label exactly as atlas names it, case and all.
    Label,
    /// A Wikidata item: `Q` and digits.
    Qid,
    /// A character name, normalised (`characters::normalise`), its spaces written `-`.
    Character,
    /// A title: its TMDB id, and under `all`, which type it is as well — `movie-550`, `series-1396`, the
    /// `{type, id}` a mixed More Like This row names.
    Title,
}

impl Id {
    fn format(self) -> &'static str {
        match self {
            Id::Integer => "integer",
            Id::Decade => "the decade's first year: 1990 is 1990-1999",
            Id::Lower => "lowercase",
            Id::Upper => "uppercase",
            Id::Label => "the label exactly as atlas names it",
            Id::Qid => "Wikidata Q-id",
            Id::Character => "the normalised name, lowercase, words joined by -",
            Id::Title => "a TMDB id of the route's type; under all, movie-<TMDB id> or series-<TMDB id>",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Data {
    /// A bitset per value, built at load.
    Bits,
    /// The rating provider's thresholds, rebuilt when it swaps in new votes.
    Rating,
    /// Posting lists into `ENTITY_KINDS[i]`.
    Entity(usize),
    /// The character provider's names (`characters.rs`), rebuilt with each build of its links. Search-only,
    /// and not offered until the provider has built them.
    Character,
    /// More Like This for a title: under one type the set `/index/similar` answers as `ids`, under `all` the
    /// one it answers as `mixed`.
    Like,
}

/// One kind a selection may name.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub name: &'static str,
    pub mode: Mode,
    id: Id,
    data: Data,
    about: &'static str,
}

impl Spec {
    /// Whether `counts.json` lists the kind's values; the others appear there only as selected.
    fn listed(&self) -> bool {
        !matches!(self.data, Data::Character | Data::Like)
    }

    /// Whether `values/<kind>.json` answers for it.
    pub fn searchable(&self) -> bool {
        !matches!(self.data, Data::Like)
    }

    fn min_prefix(&self) -> usize {
        if self.data == Data::Character {
            CHARACTER_MIN_PREFIX
        } else {
            MIN_PREFIX
        }
    }

    pub fn values_limit(&self) -> usize {
        if self.data == Data::Character {
            CHARACTER_LIMIT
        } else {
            VALUES_LIMIT
        }
    }

    /// Whether its values are TMDB's (`tmdb.rs`): the rating provider's averages and the credits' character
    /// names, which may filter and sort here but may not reach a model.
    fn tmdbs(&self) -> bool {
        matches!(self.data, Data::Rating | Data::Character)
    }
}

/// The kinds whose values are TMDB's, as the schema's `tmdb` names them.
pub fn tmdb_kinds() -> Vec<&'static str> {
    SPECS.iter().filter(|spec| spec.tmdbs()).map(|spec| spec.name).collect()
}

/// An entity kind: the store's entity-list sections it reads, the fewest titles a value needs to be listed,
/// and whether it exists for series alone. A kind whose sections a store lacks is unavailable; a later one —
/// director, writer, award, franchise — slots in as a row here once the store carries its section.
struct EntitySpec {
    name: &'static str,
    sections: &'static [(&'static str, &'static str)],
    min_titles: usize,
    series_only: bool,
    /// When not empty, the only values listed and searched (any may still be selected).
    only: &'static [u32],
    about: &'static str,
}

/// The formats worth offering, from `instance_of`: the ones that tell titles apart. Left out on the shipped
/// store are the near-universal (film Q11424 on 36,102 titles, television series Q5398426 on 5,517, television
/// program Q15416), the bookkeeping (a series' episode or season, "conflation", "video work") and everything
/// under twenty titles.
const FORMATS: &[u32] = &[
    506_240,     // television film
    202_866,     // animated film
    63_952_888,  // anime television series
    24_862,      // short film
    117_467_246, // animated television series
    1_259_759,   // miniseries
    17_517_379,  // animated short film
    20_650_540,  // anime film
    526_877,     // web series
    1_261_214,   // television special
    98_701_476,  // television film broadcast in two parts
    20_667_187,  // silent short film
    113_671_041, // original net animation series
    98_807_719,  // animated television film
    123_126_551, // animated television special
];

const ENTITY_KINDS: &[EntitySpec] = &[
    EntitySpec {
        name: "person",
        sections: &[("makers_v", "makers_o"), ("cast_v", "cast_o")],
        min_titles: 1,
        series_only: false,
        only: &[],
        about: "anyone credited: director, creator, screenwriter or cast",
    },
    EntitySpec {
        name: "made",
        sections: &[("makers_v", "makers_o")],
        min_titles: 1,
        series_only: false,
        only: &[],
        about: "a director, creator or screenwriter",
    },
    EntitySpec {
        name: "cast",
        sections: &[("cast_v", "cast_o")],
        min_titles: 1,
        series_only: false,
        only: &[],
        about: "a cast member",
    },
    EntitySpec {
        name: "company",
        sections: &[("companies_v", "companies_o")],
        min_titles: 5,
        series_only: false,
        only: &[],
        about: "a production company (P272)",
    },
    EntitySpec {
        name: "network",
        sections: &[("broadcasters_v", "broadcasters_o")],
        min_titles: 1,
        series_only: true,
        only: &[],
        about: "the network or service a series first aired on (P449)",
    },
    EntitySpec {
        name: "subject",
        sections: &[("subjects_v", "subjects_o")],
        min_titles: 5,
        series_only: false,
        only: &[],
        about: "a main subject (P921)",
    },
    EntitySpec {
        name: "place",
        sections: &[("locations_v", "locations_o")],
        min_titles: 5,
        series_only: false,
        only: &[],
        about: "a narrative location (P840)",
    },
    EntitySpec {
        name: "format",
        sections: &[("instance_of_v", "instance_of_o")],
        min_titles: 5,
        series_only: false,
        only: FORMATS,
        about: "what the title is an instance of (P31): feature film, miniseries, animated series, …",
    },
];

/// The dense score tables a kind reads: kind, table, its vocabulary, and the floor in hundredths a title's
/// score must reach to carry the value.
///
/// The floors are read off the shipped store (5b1c3213b6a1), where every title carries every axis and the
/// scores are spread very differently per table. `technique` at 0.4: at 0.6 only live action survives (anime
/// keeps 36 titles, hand-drawn none), at 0.4 anime has 963, CG 681, hand-drawn 692 and live action 42,884.
/// `audience` at 0.5, between made-for-children's 4,965 titles at 0.4 and 3,303 at 0.6. `critique` at 0.6: at
/// 0.4 "the self" claims 24,513 titles, half the corpus; at 0.6, 9,083 and the rest 1–7k. `warning` at 0.5
/// finds nothing on that store — its highest score on any title is 0.28 (graphic violence 0.17) — so it is
/// not offered there rather than answering every exclusion with "all clean". Those were title-only
/// scores; a store with the article-based ones reaches the floor and offers `warning` on load, with nothing to
/// switch — availability is read off the loaded table, never declared.
const DENSE_KINDS: [(&str, &str, &str, u8, &str); 4] = [
    ("technique", "technique", "technique_names", 40, "how it is made: live action, anime, CG, …"),
    ("audience", "audience", "audience_names", 50, "who it is made for"),
    ("critique", "critique", "critique_names", 60, "what it critiques"),
    ("warning", "depicts", "depicts_names", 50, "what it depicts; exclude one with -warning:<id>"),
];

/// Every kind, in the order `counts.json` lists them.
static SPECS: LazyLock<Vec<Spec>> = LazyLock::new(|| {
    use Data::{Bits as B, Character, Entity, Like, Rating};
    let spec = |name, mode, id, data, about| Spec { name, mode, id, data, about };
    let mut specs = vec![
        spec(
            "genre",
            Mode::And,
            Id::Integer,
            B,
            "TMDB genre id; a series also under TMDB's composites, and under all a film id matches the \
             series filed under it too",
        ),
        spec("language", Mode::And, Id::Lower, B, "ISO 639-1, every original language"),
        spec("country", Mode::And, Id::Upper, B, "ISO 3166-1, every country of origin"),
        spec(
            "region",
            Mode::Single,
            Id::Lower,
            B,
            "a notable region by slug (`regions`): any of its countries of origin",
        ),
        spec("decade", Mode::Single, Id::Decade, B, "the release, or a series' first air date"),
        spec("mood", Mode::And, Id::Label, B, "a mood label at 0.55 or more"),
        spec("subgenre", Mode::And, Id::Label, B, "a subgenre label at 0.55 or more"),
        spec("primary", Mode::Single, Id::Label, B, "the labels' primary genre"),
        spec("animated", Mode::Single, Id::Lower, B, "yes or no"),
        spec(
            "runtime",
            Mode::Single,
            Id::Lower,
            B,
            "under-90, 90-120, 120-150, over-150; a series per episode",
        ),
        spec("source", Mode::And, Id::Lower, B, "what it was adapted from: book, play, …"),
        spec(
            "rating",
            Mode::Single,
            Id::Integer,
            Rating,
            "the rating provider's average at or above 6, 7 or 8 out of 10, on 10+ votes",
        ),
    ];
    for (name, _, _, _, about) in DENSE_KINDS {
        specs.push(spec(name, Mode::And, Id::Lower, B, about));
    }
    for &axis in den_store::FACET_AXES.iter() {
        specs.push(spec(axis, Mode::Single, Id::Lower, B, "a plot-facet axis"));
    }
    for (i, entity) in ENTITY_KINDS.iter().enumerate() {
        specs.push(spec(entity.name, Mode::And, Id::Qid, Entity(i), entity.about));
    }
    specs.push(spec(
        "studio",
        Mode::And,
        Id::Qid,
        B,
        "an iconic studio by its own item (/index/studios.json): a title crediting any item the studio is, \
         its TV or animation arm included",
    ));
    specs.push(spec("character", Mode::And, Id::Character, Character, "a character; search-only"));
    specs.push(spec(
        "like",
        Mode::Single,
        Id::Title,
        Like,
        "More Like This for a title of the route's type; under all, for a typed title, films and series mixed",
    ));
    specs
});

/// A kind by name.
pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

/// One `[-]<kind>:<id>[|<id>…]` of a selection, normalised: a value, or a group of values of one kind OR-ed
/// together (`country:FR|IT`, French or Italian).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub kind: String,
    /// `-kind:id`: the titles known NOT to carry the value — for a group, none of its values.
    pub exclude: bool,
    /// Sorted, each once, never empty; more than one is an OR group.
    pub ids: Vec<String>,
}

impl Item {
    fn spelled(&self) -> String {
        let ids: Vec<String> = self.ids.iter().map(|id| encode(id)).collect();
        format!("{}{}:{}", if self.exclude { "-" } else { "" }, self.kind, ids.join("|"))
    }

    /// One of its values as a lone item is written, as `unknownValues` names it.
    fn spelled_value(&self, id: &str) -> String {
        format!("{}{}:{}", if self.exclude { "-" } else { "" }, self.kind, encode(id))
    }

    /// Whether it is an OR group of several values.
    fn group(&self) -> bool {
        self.ids.len() > 1
    }

    /// The canonical order: by kind, then positive before excluded, then the ids joined by `|`, as strings.
    fn order_key(&self) -> (&str, bool, String) {
        (&self.kind, self.exclude, self.ids.join("|"))
    }
}

impl Ord for Item {
    fn cmp(&self, other: &Item) -> std::cmp::Ordering {
        self.order_key().cmp(&other.order_key())
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Item) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Which titles a filter route asks about: one type's, or films and series together (`all`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Type(MediaType),
    All,
}

impl Scope {
    /// The type segment of a filter path, shared by every filter route: `movie`, `series` or `all`.
    pub fn parse(segment: &str) -> Option<Scope> {
        match segment {
            "movie" => Some(Scope::Type(MediaType::Movie)),
            "series" => Some(Scope::Type(MediaType::Tv)),
            "all" => Some(Scope::All),
            _ => None,
        }
    }

    /// Its slot in the per-scope tables: [movie, series, all].
    fn index(self) -> usize {
        match self {
            Scope::Type(media_type) => type_index(media_type),
            Scope::All => 2,
        }
    }
}

impl From<MediaType> for Scope {
    fn from(media_type: MediaType) -> Scope {
        Scope::Type(media_type)
    }
}

/// A typed title as `Id::Title` writes it under `all`: `movie-550`, `series-1396`.
fn typed_title(id: &str) -> Option<Key> {
    let (media, tmdb_id) = id.split_once('-')?;
    let media_type = match media {
        "movie" => MediaType::Movie,
        "series" => MediaType::Tv,
        _ => return None,
    };
    Some((media_type, tmdb_id.parse().ok()?))
}

/// A kind and an id as their canonical pair. An unknown kind is kept — lowercased, its id as sent — so a
/// client built for a newer atlas still gets an answer, with the kind reported as ignored.
fn normalise(kind: &str, id: &str, scope: Scope) -> Result<(String, String), String> {
    let kind = kind.trim().to_ascii_lowercase();
    let id = id.trim();
    if kind.is_empty() {
        return Err(format!(":{id}: an empty kind"));
    }
    if id.is_empty() {
        return Err(format!("{kind}: an empty id"));
    }
    // The old sidecar's `structure` is three axes; the value decides which.
    let kind = if kind == "structure" {
        crate::plotrows::resolve_axis(&kind, &id.to_ascii_lowercase())
    } else {
        kind
    };
    let Some(spec) = spec(&kind) else { return Ok((kind, id.to_owned())) };
    let id = normalise_id(&kind, id, spec.id, scope)?;
    Ok((kind, id))
}

/// An id in its canonical form, as its kind's format says.
fn normalise_id(kind: &str, id: &str, format: Id, scope: Scope) -> Result<String, String> {
    let number = || id.parse::<u32>().map_err(|_| format!("{kind}: {id:?} is not an integer"));
    let id = match format {
        Id::Integer => number()?.to_string(),
        Id::Decade => (number()? / 10 * 10).to_string(),
        Id::Lower => id.to_ascii_lowercase(),
        Id::Upper => id.to_ascii_uppercase(),
        Id::Label => id.to_owned(),
        Id::Qid => {
            let digits = id.strip_prefix(['Q', 'q']).unwrap_or(id);
            match digits.parse::<u32>() {
                Ok(n) if digits.bytes().all(|b| b.is_ascii_digit()) => format!("Q{n}"),
                _ => return Err(format!("{kind}: {id:?} is not a Wikidata Q-id")),
            }
        }
        Id::Character => {
            let name = crate::characters::normalise(id);
            if name.is_empty() {
                return Err(format!("{kind}: {id:?} names no character"));
            }
            name.replace(' ', "-")
        }
        Id::Title => match scope {
            Scope::Type(_) => number()?.to_string(),
            // Both types share TMDB ids' number space, so a bare id names no title here.
            Scope::All => match typed_title(&id.to_ascii_lowercase()) {
                Some((MediaType::Movie, n)) => format!("movie-{n}"),
                Some((MediaType::Tv, n)) => format!("series-{n}"),
                None => {
                    return Err(format!(
                        "{kind}: {id:?} names no typed title; under all, movie-<TMDB id> or series-<TMDB id>"
                    ))
                }
            },
        },
    };
    Ok(id)
}

/// Which question a request asks.
#[derive(Clone, Copy, Debug)]
pub enum Route {
    Counts,
    Titles,
    Values(&'static Spec),
    /// `people.json`: the people credited on the matching titles (`people.rs`).
    People,
    /// `people/counts.json`: their traits' counts.
    PeopleCounts,
    /// `people/values/<trait>.json`: one entity trait's values, with a prefix search (`people_values_kind`).
    PeopleValues(&'static str),
}

/// The person trait `people/values/<trait>.json` answers for: those whose values are Wikidata items.
pub fn people_values_kind(name: &str) -> Option<&'static str> {
    people::values_kind(name)
}

/// A request's parameters, read and checked before anything loads.
#[derive(Debug, Default)]
pub struct Request {
    /// Sorted by kind, then positive before excluded, then id; each once.
    pub items: Vec<Item>,
    /// The people routes' person traits, ordered like `items`.
    pub traits: Vec<Item>,
    /// `people.json`'s order.
    order: people::Order,
    pub skip: usize,
    pub limit: usize,
    /// A values search's prefix, normalised as the kind's names are.
    pub q: Option<String>,
    /// Whether the request spelled exactly `query()`: the one URL every identical request shares.
    pub canonical: bool,
    canonical_query: String,
}

impl Request {
    /// A route's query string. The canonical spelling, byte for byte:
    ///
    /// - `sel`, then (people) `traits` and `order`, then (titles, people) `skip` and `limit`, or (values, people
    ///   values) `q` and `limit`; each only when it is not its default (no selection, `prominence`, 0, the route's page)
    ///   and nothing else. No query at all when everything is. `traits` is written as `sel` is, with the
    ///   person-trait kinds (`people.rs`); `order` lowercased.
    /// - `sel`: the items `[-]<kind>:<id>` joined by `,`, each id normalised as its kind says (`Id`), sorted by
    ///   kind, then positive before excluded, then id (compared as strings), each once. `:`, `,` and `-` are
    ///   literal; an id is percent-encoded as JavaScript's `encodeURIComponent` does. An item may name several
    ///   ids of its kind joined by `|` (literal; `%7C` reads the same), OR-ed: they are sorted and each kept
    ///   once, one left is written without `|`, and the item sorts by its ids joined by `|`.
    /// - `skip` and `limit` plain decimals, `limit` within 1..=100 and `skip` a multiple of it.
    /// - `q` normalised as the kind's names are (folded and lowercased, words joined by single spaces), encoded
    ///   like an id.
    ///
    /// The error is a request that cannot be answered as sent: a malformed item, an id its kind cannot read
    /// (a `like` under `all` without its type), too many values (a group counts each of its values), a group
    /// whose values resolve to different kinds or of a kind that takes one value an item (`like`, a `born`
    /// range), a query too long, a `skip` that is not a page boundary, a prefix too short, an order
    /// `people.json` does not know.
    pub fn parse(route: Route, scope: Scope, query: &str) -> Result<Request, String> {
        if query.len() > MAX_QUERY {
            return Err(format!("a query of at most {MAX_QUERY} bytes"));
        }
        let allowed: &[&str] = match route {
            Route::Counts => &["sel"],
            Route::Titles => &["sel", "skip", "limit"],
            Route::Values(_) => &["sel", "q", "limit"],
            Route::People => &["sel", "traits", "order", "skip", "limit"],
            Route::PeopleCounts => &["sel", "traits"],
            Route::PeopleValues(_) => &["sel", "traits", "q", "limit"],
        };
        let mut params: HashMap<&str, &str> = HashMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if allowed.contains(&name) {
                params.entry(name).or_insert(value);
            }
        }
        let decode = |v: &str| crate::handler::percent_decode(&v.replace('+', " "));

        // `sel` and `traits` share one grammar; each kind table normalises its own ids, and says which of its
        // values cannot be OR-ed.
        let read_items =
            |name: &str, normalise: Normalise<'_>, one_value: OneValue<'_>| -> Result<Vec<Item>, String> {
                let list = params.get(name).map(|v| decode(v)).unwrap_or_default();
                let raw: Vec<&str> = list.split(',').filter(|i| !i.is_empty()).collect();
                if raw.iter().map(|item| item.split('|').count()).sum::<usize>() > MAX_SELECTION {
                    return Err(format!(
                        "{name}: at most {MAX_SELECTION} values, a group counting each of its values"
                    ));
                }
                let mut items = Vec::with_capacity(raw.len());
                for item in raw {
                    let (exclude, item) = match item.strip_prefix('-') {
                        Some(rest) => (true, rest),
                        None => (false, item),
                    };
                    let (kind, group) =
                        item.split_once(':').ok_or_else(|| format!("{item:?} is not <kind>:<id>"))?;
                    let mut resolved: Option<String> = None;
                    let mut ids = Vec::new();
                    for id in group.split('|') {
                        let (kind, id) = normalise(kind, id)?;
                        if resolved.as_ref().is_some_and(|r| *r != kind) {
                            return Err(format!(
                                "{item:?}: its values resolve to different kinds; name the kind"
                            ));
                        }
                        resolved = Some(kind);
                        ids.push(id);
                    }
                    ids.sort();
                    ids.dedup();
                    let kind = resolved.unwrap_or_default();
                    if ids.len() > 1 && one_value(&kind, &ids[..]) {
                        return Err(format!("{item:?}: {kind} takes one value an item, not a group"));
                    }
                    items.push(Item { kind, exclude, ids });
                }
                items.sort();
                items.dedup();
                Ok(items)
            };
        // `like` orders `titles.json` by one title's similar set, so it names one title an item.
        let items = read_items("sel", &|kind, id| normalise(kind, id, scope), &|kind, _| kind == "like")?;
        let traits =
            read_items("traits", &|kind, id| people::normalise(kind, id, scope), &people::one_value)?;
        people::check(&traits)?;
        let order = match params.get("order").map(|v| decode(v)) {
            Some(order) if !order.trim().is_empty() => people::Order::parse(&order)?,
            _ => people::Order::default(),
        };

        let count = |name: &str| -> Result<Option<usize>, String> {
            params
                .get(name)
                .map(|v| v.parse::<usize>().map_err(|_| format!("{name}: {v:?} is not a count")))
                .transpose()
        };
        let (default_limit, max_limit) = match route {
            Route::Counts | Route::PeopleCounts => (0, 0),
            Route::Titles | Route::People => (PAGE, MAX_PAGE),
            Route::Values(spec) => (spec.values_limit(), spec.values_limit()),
            Route::PeopleValues(_) => (VALUES_LIMIT, VALUES_LIMIT),
        };
        let limit = count("limit")?.unwrap_or(default_limit).clamp(default_limit.min(1), max_limit);
        let skip = count("skip")?.unwrap_or(0);
        if limit > 0 && skip % limit != 0 {
            return Err(format!("skip {skip} is not a multiple of limit {limit}"));
        }
        let q = match (route, params.get("q").map(|v| decode(v))) {
            (Route::Values(spec), Some(q)) if !q.trim().is_empty() => {
                let q = if spec.data == Data::Character {
                    crate::characters::normalise(&q)
                } else {
                    crate::facts::name_key(&q)
                };
                if q.chars().count() < spec.min_prefix() {
                    return Err(format!("q: a prefix of at least {} characters", spec.min_prefix()));
                }
                Some(q)
            }
            (Route::Values(spec), _) if spec.data == Data::Character => {
                return Err("q: a character is found by name, at least 3 characters".to_owned());
            }
            (Route::PeopleValues(_), Some(q)) if !q.trim().is_empty() => {
                let q = crate::facts::name_key(&q);
                if q.chars().count() < MIN_PREFIX {
                    return Err(format!("q: a prefix of at least {MIN_PREFIX} characters"));
                }
                Some(q)
            }
            _ => None,
        };

        let mut parts = Vec::new();
        if !items.is_empty() {
            parts.push(format!("sel={}", items.iter().map(Item::spelled).collect::<Vec<_>>().join(",")));
        }
        if !traits.is_empty() {
            parts.push(format!("traits={}", traits.iter().map(Item::spelled).collect::<Vec<_>>().join(",")));
        }
        if order != people::Order::default() {
            parts.push(format!("order={}", order.name()));
        }
        if skip != 0 {
            parts.push(format!("skip={skip}"));
        }
        if let Some(q) = &q {
            parts.push(format!("q={}", encode(q)));
        }
        if limit != default_limit {
            parts.push(format!("limit={limit}"));
        }
        let canonical_query = parts.join("&");
        let canonical = query == canonical_query;
        Ok(Request { items, traits, order, skip, limit, q, canonical, canonical_query })
    }

    /// The canonical query, with its `?`, or nothing.
    pub fn query(&self) -> String {
        if self.canonical_query.is_empty() {
            String::new()
        } else {
            format!("?{}", self.canonical_query)
        }
    }
}

/// A kind's selected ids as an answer names them in `selected` and `excluded`: every value of its positive
/// items and of its excluded ones, a group's each, sorted and each once.
fn selected_ids<'i>(items: &[&'i Item]) -> (Vec<&'i str>, Vec<&'i str>) {
    let ids = |exclude: bool| {
        let mut ids: Vec<&str> = items
            .iter()
            .filter(|i| i.exclude == exclude)
            .flat_map(|i| i.ids.iter().map(String::as_str))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    (ids(false), ids(true))
}

/// Percent-encoded as JavaScript's `encodeURIComponent` does, so a browser client can build the canonical
/// URL with it.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A kind's values, each as the titles carrying it, and the titles the kind is known for.
struct Valued {
    values: BTreeMap<String, Bits>,
    known: Bits,
}

/// An entity section inverted: entity → the rows crediting it, ascending.
struct Postings {
    starts: Vec<u32>,
    rows: Vec<u32>,
}

impl Postings {
    fn of(&self, entity: u32) -> &[u32] {
        match (self.starts.get(entity as usize), self.starts.get(entity as usize + 1)) {
            (Some(&from), Some(&to)) => &self.rows[from as usize..to as usize],
            _ => &[],
        }
    }
}

struct EntityKind {
    /// One per section, in `EntitySpec::sections` order; a section two kinds read is inverted once.
    postings: Vec<Arc<Postings>>,
    /// Rows crediting anyone in the kind.
    known: Bits,
}

impl EntityKind {
    /// Titles crediting an entity, across the kind's sections: listed only from `min_titles`.
    fn titles(&self, entity: u32) -> usize {
        self.postings.iter().map(|p| p.of(entity).len()).max().unwrap_or(0)
    }
}

/// What depends on the rating provider and on TMDB's daily export: rebuilt when either swaps.
struct Derived {
    ratings: Option<Arc<RatingsIndex>>,
    export: Option<Arc<TitleIndex>>,
    rating: Option<Valued>,
    /// Each scope's rows with a card, most voted first then by TMDB id — the order `titles.json` walks:
    /// [movie, series, all], `all` the two merged by rank within their type (`interleave`).
    order: [Vec<u32>; 3],
    /// Per store row, its position in its own type's order (`order[0]` or `order[1]`); `u32::MAX` for a row
    /// with no card. The rank `interleave` merges the types by, read per title (`Context::within_type`).
    rank: Vec<u32>,
    /// Whether each type's order is by popularity at all: [movie, series]. False when no title of the type has
    /// a vote count or an export popularity, and the order is then by TMDB id alone.
    popular: [bool; 2],
    /// A fingerprint of each order, so a client paging while it changes can tell.
    order_id: [String; 3],
    /// Each scope's counts for the empty selection, worked out once.
    empty: [OnceLock<(Value, Counted)>; 3],
}

/// The titles a selection matches. `sure` carries every plot axis it names confidently; `any` also counts the
/// tentative tier, so `sure ⊆ any`, and `any \ sure` are its likely matches. The two are equal for a store
/// without the tier, or a selection naming no plot axis.
struct Matched {
    sure: Bits,
    any: Bits,
}

/// How many titles a selection or a value holds, and how many of those only through the tentative tier.
#[derive(Clone, Copy)]
struct Counted {
    total: usize,
    likely: usize,
}

/// A narrative location a query named (`FilterIndex::place_named`).
pub struct Place {
    pub qid: u32,
    pub name: String,
    pub titles: Vec<Key>,
}

/// Every kind's values over the store's rows (`FilterIndex::build`).
pub struct FilterIndex {
    keys: Vec<Key>,
    /// Each scope's rows with a card, as counts, totals and the grid all count them: [movie, series, all].
    types: [Bits; 3],
    bits: BTreeMap<&'static str, Valued>,
    /// The plot axes' tentative tier (`PlotFacets::tentative_values`): axis → value → the titles carrying it
    /// only tentatively. Never overlaps the value's bits in `bits`. Empty for a store without the tier.
    likely: BTreeMap<&'static str, BTreeMap<String, Bits>>,
    /// Per `ENTITY_KINDS` row; `None` when the store does not carry its sections, and then not offered.
    entities: Vec<Option<EntityKind>>,
    /// Kinds this atlas should answer and cannot, through a failure at load: the facts or the facet rows did
    /// not read. A kind this dataset version cannot answer — a section the store does not carry, a score
    /// table no title reaches the floor of — is not here: it is not offered, as a kind the store has no data
    /// for, and it turns up by itself with the store that has.
    unavailable: Vec<&'static str>,
    derived: Mutex<Option<Arc<Derived>>>,
    names: OnceLock<NameIndex>,
    build_bytes: usize,
}

fn set(bits: &mut [u64], row: usize) {
    bits[row / 64] |= 1 << (row % 64);
}

fn has(bits: &[u64], row: usize) -> bool {
    bits.get(row / 64).is_some_and(|w| w & (1 << (row % 64)) != 0)
}

fn and_count(a: &[u64], b: &[u64]) -> usize {
    a.iter().zip(b).map(|(x, y)| (x & y).count_ones() as usize).sum()
}

fn popcount(bits: &[u64]) -> usize {
    bits.iter().map(|w| w.count_ones() as usize).sum()
}

fn ones(bits: &[u64]) -> impl Iterator<Item = usize> + '_ {
    bits.iter().enumerate().flat_map(|(w, &word)| {
        let mut rest = word;
        std::iter::from_fn(move || {
            (rest != 0).then(|| {
                let bit = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                w * 64 + bit
            })
        })
    })
}

fn type_index(media_type: MediaType) -> usize {
    usize::from(media_type == MediaType::Tv)
}

/// The `all` order: the two types' own orders merged by each title's rank within its type as a share of that
/// type's size (i/films against j/series), films first on a tie. Series then run at their share of the corpus,
/// spread evenly, and each type's most popular titles sit beside the other's — where raw votes, which run far
/// higher for films, would push series pages down. A selection under `all` walks this order filtered, which
/// is the two types' filtered orders merged by the same keys. The shares are compared as exact fractions
/// (i·m against j·n), so the order is deterministic.
fn interleave(movies: &[u32], series: &[u32]) -> Vec<u32> {
    let (n, m) = (movies.len() as u64, series.len() as u64);
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(movies.len() + series.len());
    while i < movies.len() || j < series.len() {
        let film_first = j == series.len() || (i < movies.len() && i as u64 * m <= j as u64 * n);
        if film_first {
            out.push(movies[i]);
            i += 1;
        } else {
            out.push(series[j]);
            j += 1;
        }
    }
    out
}

impl FilterIndex {
    /// Every kind's values over the store's rows.
    ///
    /// Genre, country and language come from the facts, and are unavailable without them: the labels alone name
    /// one genre a title, and counting from them would hide options the facts would show. Decade reads the
    /// card's year (a series' first air date), else the facts' release. Mood, subgenre, primary genre and
    /// animation are the labels, at the display floor `/index/row` uses; the plot axes the facet rows, the
    /// merged rows included; the score tables their `DENSE_KINDS` floors.
    pub fn build(indexes: &Indexes) -> FilterIndex {
        let view = indexes.store.view();
        let packed = view.per_row::<u64>("keys").unwrap_or(&[]);
        let rows = packed.len();
        let words = rows.div_ceil(64);
        let zeros = || vec![0u64; words];
        let keys: Vec<Key> = packed
            .iter()
            .map(|&p| (if (p >> 32) == 1 { MediaType::Tv } else { MediaType::Movie }, p as u32))
            .collect();
        let row_of: HashMap<Key, usize> = keys.iter().enumerate().map(|(row, &key)| (key, row)).collect();

        let mut types = [zeros(), zeros(), zeros()];
        for (row, key) in keys.iter().enumerate() {
            if indexes.cards.as_ref().is_none_or(|cards| cards.contains_key(key)) {
                set(&mut types[type_index(key.0)], row);
                set(&mut types[Scope::All.index()], row);
            }
        }

        let mut bits: BTreeMap<&'static str, Valued> = BTreeMap::new();
        let mut unavailable: Vec<&'static str> = Vec::new();
        let add =
            |bits: &mut BTreeMap<&'static str, Valued>, kind: &'static str, value: String, row: usize| {
                let valued =
                    bits.entry(kind).or_insert_with(|| Valued { values: BTreeMap::new(), known: zeros() });
                set(valued.values.entry(value).or_insert_with(zeros), row);
                set(&mut valued.known, row);
            };
        let open = |bits: &mut BTreeMap<&'static str, Valued>, kind: &'static str| {
            bits.entry(kind).or_insert_with(|| Valued { values: BTreeMap::new(), known: zeros() });
        };

        let facts = indexes.facts.as_ref();
        if facts.is_some() {
            for kind in ["genre", "language", "country", "region", "source"] {
                open(&mut bits, kind);
            }
        } else {
            unavailable.extend(["genre", "language", "country", "region", "source"]);
        }
        if facts.is_some() || indexes.cards.is_some() {
            open(&mut bits, "decade");
        } else {
            unavailable.push("decade");
        }
        for kind in ["mood", "subgenre", "primary", "animated"] {
            open(&mut bits, kind);
        }
        let runtime = view.per_row::<u16>("runtime").ok();
        if runtime.is_some() {
            open(&mut bits, "runtime");
        }

        let floor = den_index::DISPLAY_CONFIDENCE_FLOOR;
        for (row, &key) in keys.iter().enumerate() {
            let (media_type, id) = key;
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
                    add(&mut bits, "genre", genre.to_string(), row);
                }
            }
            if let Some(record) = record {
                for code in &record.countries {
                    add(&mut bits, "country", String::from_utf8_lossy(code).to_ascii_uppercase(), row);
                }
                for code in &record.languages {
                    add(&mut bits, "language", String::from_utf8_lossy(code).to_ascii_lowercase(), row);
                }
                for kind in crate::facts::SourceKinds::names(record.source_kinds.raw()) {
                    add(&mut bits, "source", kind.to_owned(), row);
                }
            }
            let year = indexes
                .cards
                .as_ref()
                .and_then(|cards| cards.get(&key))
                .and_then(|card| card.year)
                .or_else(|| record.and_then(|r| r.released).map(|r| r.year_of()));
            if let Some(year) = year.filter(|&y| y >= FIRST_YEAR) {
                add(&mut bits, "decade", (year.div_euclid(10) * 10).to_string(), row);
            }
            if let Some(labels) = indexes.plot.labels(id, media_type) {
                for (kind, pairs) in [("mood", &labels.moods), ("subgenre", &labels.subgenres)] {
                    for &(name, confidence) in pairs {
                        if confidence >= floor {
                            add(&mut bits, kind, name.to_owned(), row);
                        }
                    }
                }
                if !labels.primary_genre.is_empty() {
                    add(&mut bits, "primary", labels.primary_genre.to_owned(), row);
                }
                add(&mut bits, "animated", if labels.animated { "yes" } else { "no" }.to_owned(), row);
            }
            if let Some(minutes) = runtime.and_then(|r| r.get(row)).map(|&m| u32::from(m)).filter(|&m| m > 0)
            {
                if let Some(&(bucket, _, _)) =
                    RUNTIME_BUCKETS.iter().find(|&&(_, from, to)| (from..to).contains(&minutes))
                {
                    add(&mut bits, "runtime", bucket.to_owned(), row);
                }
            }
        }

        // Regions: each the union of its countries' titles, known wherever a country is. A region none of
        // whose countries a title carries is not offered.
        if let Some(country) = bits.get("country") {
            let mut values = BTreeMap::new();
            for region in den_index::REGIONS {
                let mut union = zeros();
                for member in region.countries.iter().filter_map(|c| country.values.get(*c)) {
                    union.iter_mut().zip(member).for_each(|(u, m)| *u |= m);
                }
                if union.iter().any(|&w| w != 0) {
                    values.insert(region.slug.to_owned(), union);
                }
            }
            let known = country.known.clone();
            bits.insert("region", Valued { values, known });
        }

        // The score tables. A title the labelling pass described is known for all of them — an unanswered
        // axis is stored 0, so "no score" is "not depicted" there — and a title it never reached is known
        // only where it scores something.
        let strings = view.strings().ok();
        for (kind, table, names, floor, _) in DENSE_KINDS {
            let read = (|| {
                let cells = view.column::<u8>(table).ok()?;
                let names = view.column::<u32>(names).ok()?;
                let strings = strings.as_ref()?;
                (cells.len() == rows * names.len()).then_some((cells, names, strings))
            })();
            // A table the store does not carry, or one no title reaches the floor of, is not offered: it can
            // answer nothing, and an exclusion over it would call every title clean.
            let Some((cells, names, strings)) = read else { continue };
            if !names.is_empty() && !cells.iter().any(|&score| score >= floor) {
                continue;
            }
            open(&mut bits, kind);
            let width = names.len();
            let axes: Vec<Option<String>> =
                names.iter().map(|&id| strings.get(id).map(|n| n.to_ascii_lowercase())).collect();
            for (row, &(media_type, id)) in keys.iter().enumerate() {
                let scores = &cells[row * width..(row + 1) * width];
                let labelled = indexes.plot.labels(id, media_type).is_some();
                if labelled || scores.iter().any(|&s| s > 0) {
                    if let Some(valued) = bits.get_mut(kind) {
                        set(&mut valued.known, row);
                    }
                }
                for (score, axis) in scores.iter().zip(&axes) {
                    if let (true, Some(axis)) = (*score >= floor, axis) {
                        add(&mut bits, kind, axis.clone(), row);
                    }
                }
            }
        }

        let mut likely: BTreeMap<&'static str, BTreeMap<String, Bits>> = BTreeMap::new();
        if let Some(plot_facets) = &indexes.plot_facets {
            for &axis in den_store::FACET_AXES.iter() {
                open(&mut bits, axis);
            }
            for (axis, value, titles) in plot_facets.values() {
                let Some(axis) = den_store::FACET_AXES.iter().copied().find(|a| *a == axis) else { continue };
                for (key, _) in titles {
                    if let Some(&row) = row_of.get(key) {
                        add(&mut bits, axis, value.to_owned(), row);
                    }
                }
            }
            // The tentative tier. It does not make a title KNOWN for the axis, so an exclusion never keeps a
            // title on a tentative value; and a value only the tier holds is still a value of the axis, listed
            // and selectable, with no confident titles.
            for (axis, value, titles) in plot_facets.tentative_values() {
                let Some(axis) = den_store::FACET_AXES.iter().copied().find(|a| *a == axis) else { continue };
                let tier = likely.entry(axis).or_default().entry(value.to_owned()).or_insert_with(zeros);
                for (key, _) in titles {
                    if let Some(&row) = row_of.get(key) {
                        set(tier, row);
                    }
                }
                if let Some(valued) = bits.get_mut(axis) {
                    valued.values.entry(value.to_owned()).or_insert_with(zeros);
                }
            }
            // The merged display rows `/index/row` answers (`ending:unhappy`), as the union of their members,
            // in each tier.
            for merged in crate::plotrows::MERGED_ROWS {
                let union = |values: &BTreeMap<String, Bits>| {
                    let mut union = zeros();
                    for member in merged.members.iter().filter_map(|m| values.get(*m)) {
                        union.iter_mut().zip(member).for_each(|(u, m)| *u |= m);
                    }
                    union
                };
                if let Some(valued) = bits.get_mut(merged.axis) {
                    let sure = union(&valued.values);
                    valued.values.insert(merged.value.to_owned(), sure);
                }
                if let Some(values) = likely.get_mut(merged.axis) {
                    let guessed = union(values);
                    values.insert(merged.value.to_owned(), guessed);
                }
            }
        } else {
            unavailable.extend(den_store::FACET_AXES.iter().copied());
        }

        // The iconic studios (`studios.rs`): each the union of the items it is credited as, known wherever a
        // company is. A store without the studio sections does not offer the kind.
        if let (false, Ok(companies)) =
            (indexes.studios.is_empty(), view.list::<u32>("companies_v", "companies_o"))
        {
            open(&mut bits, "studio");
            for row in 0..rows {
                let credited = companies.get(den_store::Row(row));
                if credited.is_empty() {
                    continue;
                }
                if let Some(valued) = bits.get_mut("studio") {
                    set(&mut valued.known, row);
                }
                for studio in indexes.studios.credited(credited) {
                    add(&mut bits, "studio", studio.id(), row);
                }
            }
        }

        // Entity kinds: each section inverted once, shared by the kinds that read it.
        let entity_count = view.column::<u32>("ent_qid").map_or(0, <[u32]>::len);
        let mut inverted: HashMap<&'static str, Option<(Arc<Postings>, Bits)>> = HashMap::new();
        let mut entities = Vec::with_capacity(ENTITY_KINDS.len());
        let mut postings_bytes = 0;
        for entity in ENTITY_KINDS {
            let mut postings = Vec::new();
            let mut known = zeros();
            for &(values, offsets) in entity.sections {
                let built = inverted.entry(values).or_insert_with(|| {
                    let built = invert(&view, values, offsets, rows, entity_count, words);
                    if let Some((p, _)) = &built {
                        postings_bytes += (p.starts.len() + p.rows.len()) * 4;
                    }
                    built.map(|(p, k)| (Arc::new(p), k))
                });
                match built {
                    Some((section, section_known)) => {
                        known.iter_mut().zip(section_known.iter()).for_each(|(k, s)| *k |= s);
                        postings.push(Arc::clone(section));
                    }
                    None => break,
                }
            }
            entities
                .push((postings.len() == entity.sections.len()).then_some(EntityKind { postings, known }));
        }

        let bitset = |b: &Bits| b.len() * 8;
        let build_bytes = bits
            .values()
            .map(|v| v.values.values().map(bitset).sum::<usize>() + bitset(&v.known))
            .sum::<usize>()
            + entities.iter().flatten().map(|e| bitset(&e.known)).sum::<usize>()
            + likely.values().flat_map(|v| v.values()).map(bitset).sum::<usize>()
            + postings_bytes
            + keys.len() * std::mem::size_of::<Key>()
            + 3 * bitset(&types[0]);
        unavailable.sort_unstable();
        unavailable.dedup();
        FilterIndex {
            keys,
            types,
            bits,
            likely,
            entities,
            unavailable,
            derived: Mutex::new(None),
            names: OnceLock::new(),
            build_bytes,
        }
    }

    /// Resident bytes of what `build` made, for the load line and the measurement.
    pub fn bytes(&self) -> usize {
        self.build_bytes
    }

    /// Values across the bitset kinds, for the measurement.
    pub fn value_count(&self) -> usize {
        self.bits.values().map(|v| v.values.len()).sum()
    }

    /// Titles with a card carrying one value of a bitset kind, [films, series]; none for a kind not offered.
    pub fn typed_count(&self, kind: &str, value: &str) -> [usize; 2] {
        let Some(bits) = self.bits.get(kind).and_then(|v| v.values.get(value)) else { return [0, 0] };
        [0, 1].map(|t| and_count(bits, &self.types[t]))
    }

    /// Titles with a card an entity kind has anything on record for, of one type or both.
    pub fn entity_known(&self, kind: &str, media_type: Option<MediaType>) -> usize {
        let at = ENTITY_KINDS.iter().position(|e| e.name == kind);
        let Some(entity) = at.and_then(|at| self.entities[at].as_ref()) else { return 0 };
        and_count(&entity.known, &self.types[media_type.map_or(Scope::All, Scope::Type).index()])
    }

    /// Whether an entity kind has anything on record for a title: a title with no narrative location is not
    /// known to be set anywhere else.
    pub fn entity_on_record(&self, indexes: &Indexes, kind: &str, (media_type, id): Key) -> bool {
        let at = ENTITY_KINDS.iter().position(|e| e.name == kind);
        let Some(entity) = at.and_then(|at| self.entities[at].as_ref()) else { return false };
        let row = indexes.store.view().row_of(u8::from(media_type == MediaType::Tv), id).ok().flatten();
        row.is_some_and(|row| has(&entity.known, row.0))
    }

    /// Every title carrying one value of a bitset kind (`warning:graphic_violence`); `None` when the kind is
    /// not offered or holds no such value, which is not the same as no title carrying it.
    pub fn titles_with(&self, kind: &str, value: &str) -> Option<Vec<Key>> {
        let bits = self.bits.get(kind)?.values.get(value)?;
        Some(ones(bits).map(|row| self.keys[row]).collect())
    }

    /// The narrative location (P840) a folded name (`facts::name_key`) is exactly the name or an alias of, and
    /// the titles set there; the one set in the most titles when several go by it (Paris, not Paris, Texas).
    /// `None` when no place the `place` kind lists goes by it.
    pub fn place_named(&self, indexes: &Indexes, key: &str) -> Option<Place> {
        let at = ENTITY_KINDS.iter().position(|e| e.name == "place")?;
        let kind = self.entities[at].as_ref()?;
        let entity = self
            .names(indexes)
            .matching(key)
            .into_iter()
            .filter(|&(e, tier)| tier == 0 && kind.titles(e) >= ENTITY_KINDS[at].min_titles)
            .max_by_key(|&(e, _)| (kind.titles(e), std::cmp::Reverse(e)))?
            .0;
        let view = indexes.store.view();
        let qid = *view.column::<u32>("ent_qid").ok()?.get(entity as usize)?;
        let name = view.column::<u32>("ent_name").ok()?.get(entity as usize).copied();
        let name = name.and_then(|n| view.strings().ok()?.get(n)).unwrap_or_default().to_owned();
        let mut rows: Vec<u32> = kind.postings.iter().flat_map(|p| p.of(entity).iter().copied()).collect();
        rows.sort_unstable();
        rows.dedup();
        Some(Place { qid, name, titles: rows.into_iter().map(|row| self.keys[row as usize]).collect() })
    }

    /// The provider- and export-dependent part, rebuilt when either has swapped since it was last built.
    fn derived(&self, indexes: &Indexes, export: Option<Arc<TitleIndex>>) -> Arc<Derived> {
        let ratings = indexes.ratings.as_ref().and_then(|r| r.index());
        let same = |a: &Option<Arc<RatingsIndex>>, b: &Option<Arc<RatingsIndex>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let same_export = |a: &Option<Arc<TitleIndex>>, b: &Option<Arc<TitleIndex>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let mut slot = crate::util::lock(&self.derived);
        if let Some(derived) =
            slot.as_ref().filter(|d| same(&d.ratings, &ratings) && same_export(&d.export, &export))
        {
            return Arc::clone(derived);
        }
        let derived = Arc::new(self.derive(indexes, ratings, export));
        *slot = Some(Arc::clone(&derived));
        derived
    }

    fn derive(
        &self,
        indexes: &Indexes,
        ratings: Option<Arc<RatingsIndex>>,
        export: Option<Arc<TitleIndex>>,
    ) -> Derived {
        let words = self.types[0].len();
        let rating = ratings.as_ref().map(|index| {
            let mut valued = Valued {
                values: RATING_THRESHOLDS.iter().map(|t| (t.to_string(), vec![0u64; words])).collect(),
                known: vec![0u64; words],
            };
            for row in 0..self.keys.len() {
                let Some((votes, score)) = index.of(row) else { continue };
                if votes < MIN_VOTES {
                    continue;
                }
                set(&mut valued.known, row);
                for t in RATING_THRESHOLDS {
                    if score >= t as f32 {
                        if let Some(bits) = valued.values.get_mut(t.to_string().as_str()) {
                            set(bits, row);
                        }
                    }
                }
            }
            valued
        });
        let [(movies, films_popular), (series, series_popular)] = [0, 1].map(|t| {
            let mut ranked: Vec<(u32, f64, u32)> = ones(&self.types[t])
                .map(|row| {
                    let key = self.keys[row];
                    (row as u32, crate::plotrows::popularity(indexes, export.as_deref(), key), key.1)
                })
                .collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
            let popular = ranked.first().is_some_and(|r| r.1 > 0.0);
            (ranked.into_iter().map(|(row, _, _)| row).collect::<Vec<u32>>(), popular)
        });
        let mut rank = vec![u32::MAX; self.keys.len()];
        for order in [&movies, &series] {
            for (i, &row) in order.iter().enumerate() {
                rank[row as usize] = i as u32;
            }
        }
        let all = interleave(&movies, &series);
        let order = [movies, series, all];
        let order_id = [0, 1, 2].map(|t| {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for &row in &order[t] {
                let (media_type, id) = self.keys[row as usize];
                // Under `all` the type is part of what a position holds.
                let typed = (t == Scope::All.index()).then_some(type_index(media_type) as u8);
                for byte in id.to_le_bytes().into_iter().chain(typed) {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            format!("{hash:016x}")
        });
        Derived {
            ratings,
            export,
            rating,
            order,
            rank,
            popular: [films_popular, series_popular],
            order_id,
            empty: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    /// Entity labels and aliases, folded, for `values/<kind>.json?q=`. Built on the first search.
    fn names(&self, indexes: &Indexes) -> &NameIndex {
        self.names.get_or_init(|| NameIndex::build(&indexes.store.view()))
    }
}

/// An entity section inverted, and the rows crediting anyone; `None` when it does not read.
fn invert(
    view: &den_store::Store<'_>,
    values: &'static str,
    offsets: &'static str,
    rows: usize,
    entities: usize,
    words: usize,
) -> Option<(Postings, Bits)> {
    let list = view.list::<u32>(values, offsets).ok()?;
    let mut counts = vec![0u32; entities + 1];
    let mut known = vec![0u64; words];
    for row in 0..rows {
        let credited = list.get(den_store::Row(row));
        if !credited.is_empty() {
            set(&mut known, row);
        }
        for &e in credited {
            if let Some(c) = counts.get_mut(e as usize + 1) {
                *c += 1;
            }
        }
    }
    let mut starts = counts;
    for i in 1..starts.len() {
        starts[i] += starts[i - 1];
    }
    let mut fill = starts.clone();
    let mut out = vec![0u32; *starts.last().unwrap_or(&0) as usize];
    for row in 0..rows {
        for &e in list.get(den_store::Row(row)) {
            if let Some(at) = fill.get_mut(e as usize) {
                out[*at as usize] = row as u32;
                *at += 1;
            }
        }
    }
    Some((Postings { starts, rows: out }, known))
}

/// Every entity's name and aliases, folded (`facts::name_key`), in one arena, scanned per search: a scan of a
/// few MB answers in about a millisecond, where a sorted index of every word would hold several times that.
struct NameIndex {
    text: String,
    /// (entity, start, end) into `text`.
    names: Vec<(u32, u32, u32)>,
}

impl NameIndex {
    fn build(view: &den_store::Store<'_>) -> NameIndex {
        let mut out = NameIndex { text: String::new(), names: Vec::new() };
        let (Ok(names), Ok(strings)) = (view.column::<u32>("ent_name"), view.strings()) else { return out };
        let aliases = view.list_of::<u32>("ent_alias_v", "ent_alias_o", names.len()).ok();
        for (entity, &name) in names.iter().enumerate() {
            let alias_ids = aliases.as_ref().map_or(&[][..], |a| a.get(den_store::Row(entity)));
            for id in std::iter::once(name).chain(alias_ids.iter().copied()) {
                let Some(text) = strings.get(id) else { continue };
                let key = crate::facts::name_key(text);
                if key.is_empty() {
                    continue;
                }
                let start = out.text.len() as u32;
                out.text.push_str(&key);
                out.names.push((entity as u32, start, out.text.len() as u32));
            }
        }
        out
    }

    /// Entities with a name or alias in which some word starts `prefix`, each once, with the best `match_tier`
    /// any of its names reaches.
    fn matching(&self, prefix: &str) -> Vec<(u32, u8)> {
        let mut out: Vec<(u32, u8)> = self
            .names
            .iter()
            .filter_map(|&(entity, from, to)| {
                match_tier(&self.text[from as usize..to as usize], prefix).map(|tier| (entity, tier))
            })
            .collect();
        out.sort_unstable();
        out.dedup_by_key(|&mut (entity, _)| entity);
        out
    }
}

/// How well a folded name (`facts::name_key`) answers a values search, best first: 0 when it is the query,
/// 1 when it holds the query as whole words ("walt disney pictures" for `disney`), 2 when a word only starts
/// with it (`disneynature`); `None` when no word starts with it. Ordering by title count alone let an alias
/// outrank the name itself: San Francisco, which Wikidata also calls "Paris of the West", came before Paris.
/// Whole words rather than the start of the name are the middle tier because a typeahead is searched by
/// surname: `nolan` is Christopher Nolan before Nolan Gerard Funk.
fn match_tier(name: &str, q: &str) -> Option<u8> {
    if name == q {
        return Some(0);
    }
    std::iter::once(0)
        .chain(name.match_indices(' ').map(|(i, _)| i + 1))
        .filter(|&at| name[at..].starts_with(q))
        .map(|at| {
            let after = &name[at + q.len()..];
            if after.is_empty() || after.starts_with(' ') {
                1
            } else {
                2
            }
        })
        .min()
}

/// Whether a kind answers now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Ready,
    /// This atlas should answer it and its source did not load (a store section, a join not yet landed).
    Unavailable,
    /// Not a kind here: switched off, or not one of this type's (a network for films).
    NotOffered,
}

/// One request's view of the index: the live provider- and export-dependent parts, and the store.
pub struct Context<'a> {
    filter: &'a FilterIndex,
    indexes: &'a Indexes,
    derived: Arc<Derived>,
    characters: Option<Arc<CharacterIndex>>,
    scope: Scope,
    view: den_store::Store<'a>,
}

impl<'a> Context<'a> {
    pub fn new(
        indexes: &'a Indexes,
        scope: impl Into<Scope>,
        export: Option<Arc<TitleIndex>>,
    ) -> Context<'a> {
        let filter = indexes.filter();
        Context {
            filter,
            indexes,
            derived: filter.derived(indexes, export),
            characters: indexes.characters.as_ref().and_then(|c| c.index()),
            scope: scope.into(),
            view: indexes.store.view(),
        }
    }

    fn t(&self) -> usize {
        self.scope.index()
    }

    /// How high a title stands in its own type's popularity order, 1 for the top down towards 0: one minus its
    /// rank there over the type's size — the share `interleave` merges the two types by, so a series and a
    /// film at the same share weigh the same. 0 for a row with no card.
    fn within_type(&self, row: usize) -> f64 {
        let rank = self.derived.rank.get(row).copied().unwrap_or(u32::MAX);
        let size = self.derived.order[type_index(self.filter.keys[row].0)].len();
        if rank == u32::MAX || size == 0 {
            return 0.0;
        }
        1.0 - f64::from(rank) / size as f64
    }

    /// Whether every type of the scope has a popularity order, rather than one by TMDB id alone.
    fn popular(&self) -> bool {
        match self.scope {
            Scope::Type(media_type) => self.derived.popular[type_index(media_type)],
            Scope::All => self.derived.popular.iter().all(|&p| p),
        }
    }

    /// The title a `like` id names: under one type a TMDB id of it, under `all` a typed one.
    fn like_key(&self, id: &str) -> Option<Key> {
        match self.scope {
            Scope::Type(media_type) => Some((media_type, id.parse().ok()?)),
            Scope::All => typed_title(id),
        }
    }

    /// The rows of a `like`'s similar set, in its order: the seed's type alone, or under `all` the row that
    /// mixes films and series (`/index/similar`'s `mixed`).
    fn like_rows(&self, id: &str) -> Vec<u32> {
        let Some((media_type, tmdb_id)) = self.like_key(id) else { return Vec::new() };
        let keys: Vec<Key> = match self.scope {
            Scope::Type(_) => {
                self.indexes.more_like_this(tmdb_id, media_type).iter().map(|&id| (media_type, id)).collect()
            }
            Scope::All => self.indexes.more_like_this_mixed(tmdb_id, media_type).to_vec(),
        };
        keys.into_iter()
            .filter_map(|(media_type, id)| {
                self.view.row_of(u8::from(media_type == MediaType::Tv), id).ok().flatten()
            })
            .map(|row| row.0 as u32)
            .collect()
    }

    fn status(&self, spec: &Spec) -> Status {
        match spec.data {
            Data::Bits if self.filter.bits.contains_key(spec.name) => Status::Ready,
            Data::Bits if self.filter.unavailable.contains(&spec.name) => Status::Unavailable,
            Data::Bits => Status::NotOffered,
            Data::Rating if self.indexes.ratings.is_none() => Status::NotOffered,
            Data::Rating if self.derived.rating.is_some() => Status::Ready,
            Data::Rating => Status::Unavailable,
            // Under `all` a series-only kind is offered, and matches only series.
            Data::Entity(i) if ENTITY_KINDS[i].series_only && self.scope == Scope::Type(MediaType::Movie) => {
                Status::NotOffered
            }
            Data::Entity(i) if self.filter.entities[i].is_some() => Status::Ready,
            Data::Entity(_) => Status::NotOffered,
            // Character links come from whichever provider feeds `Characters`, and until it has, the kind
            // is simply absent: it is search-only, so nothing a client lists goes missing.
            Data::Character if self.characters.is_some() => Status::Ready,
            Data::Character => Status::NotOffered,
            Data::Like => Status::Ready,
        }
    }

    /// The kinds this atlas should answer and cannot right now, by name.
    pub fn unavailable(&self) -> Vec<&'static str> {
        SPECS.iter().filter(|s| self.status(s) == Status::Unavailable).map(|s| s.name).collect()
    }

    /// The selection split: the items applied, and the kinds named but not applied (unknown, switched off,
    /// not the route type's, or unavailable).
    fn split<'r>(&self, items: &'r [Item]) -> (Vec<(&'static Spec, &'r Item)>, Vec<String>) {
        let mut applied = Vec::new();
        let mut ignored: Vec<String> = Vec::new();
        for item in items {
            match spec(&item.kind).filter(|s| self.status(s) == Status::Ready) {
                Some(spec) => applied.push((spec, item)),
                None if !ignored.contains(&item.kind) => ignored.push(item.kind.clone()),
                None => {}
            }
        }
        (applied, ignored)
    }

    fn words(&self) -> usize {
        self.filter.types[0].len()
    }

    /// The titles carrying a value, and the titles its kind is known for.
    fn value_bits(&self, spec: &Spec, id: &str) -> (Bits, Bits) {
        let words = self.words();
        let from_rows = |rows: &mut dyn Iterator<Item = usize>| {
            let mut bits = vec![0u64; words];
            for row in rows {
                if row / 64 < words {
                    set(&mut bits, row);
                }
            }
            bits
        };
        match spec.data {
            Data::Bits | Data::Rating => {
                let valued = if spec.data == Data::Rating {
                    self.derived.rating.as_ref()
                } else {
                    self.filter.bits.get(spec.name)
                };
                let Some(valued) = valued else { return (vec![0; words], vec![0; words]) };
                (valued.values.get(id).cloned().unwrap_or_else(|| vec![0; words]), valued.known.clone())
            }
            Data::Entity(i) => {
                let Some(kind) = self.filter.entities[i].as_ref() else {
                    return (vec![0; words], vec![0; words]);
                };
                let bits = match self.entity_of(id) {
                    Some(e) => {
                        from_rows(&mut kind.postings.iter().flat_map(|p| p.of(e).iter().map(|&r| r as usize)))
                    }
                    None => vec![0; words],
                };
                (bits, kind.known.clone())
            }
            Data::Character => {
                let Some(characters) = self.characters.as_ref() else {
                    return (vec![0; words], vec![0; words]);
                };
                let named = characters.named();
                let rows = named.rows(&id.replace('-', " "));
                let mut known = named.known().to_vec();
                known.resize(words, 0);
                (from_rows(&mut rows.iter().map(|&r| r as usize)), known)
            }
            Data::Like => {
                let known = self.filter.types[self.t()].clone();
                (from_rows(&mut self.like_rows(id).into_iter().map(|row| row as usize)), known)
            }
        }
    }

    /// A Q-id's entity index; `None` for one the store does not name.
    fn entity_of(&self, qid: &str) -> Option<u32> {
        let number: u32 = qid.strip_prefix('Q')?.parse().ok()?;
        let ids = self.view.column::<u32>("ent_qid").ok()?;
        ids.binary_search(&number).ok().map(|i| i as u32)
    }

    fn qid(&self, entity: u32) -> String {
        let id = self.view.column::<u32>("ent_qid").ok().and_then(|ids| ids.get(entity as usize).copied());
        format!("Q{}", id.unwrap_or(0))
    }

    fn label(&self, entity: u32) -> Option<&'a str> {
        let names = self.view.column::<u32>("ent_name").ok()?;
        self.view.strings().ok()?.get(*names.get(entity as usize)?)
    }

    fn tmdb(&self, entity: u32) -> Option<u32> {
        let ids = self.view.column::<u32>("ent_tmdb").ok()?;
        ids.get(entity as usize).copied().filter(|&id| id != den_store::NONE_U32)
    }

    /// Whether the store carries a tentative tier. The split fields are answered only then, which keeps an
    /// answer from a store without one byte-identical to what it was before the tier existed.
    fn tier(&self) -> bool {
        !self.filter.likely.is_empty()
    }

    /// The titles carrying a value only tentatively: a plot axis's tier, `None` for any other kind.
    fn likely_bits(&self, spec: &Spec, id: &str) -> Option<&Bits> {
        match spec.data {
            Data::Bits => self.filter.likely.get(spec.name)?.get(id),
            _ => None,
        }
    }

    /// The titles carrying any value of an item, and the titles its kind is known for.
    fn item_bits(&self, spec: &Spec, item: &Item) -> (Bits, Bits) {
        let mut ids = item.ids.iter();
        let (mut value, known) = self.value_bits(spec, ids.next().map_or("", String::as_str));
        for id in ids {
            let (more, _) = self.value_bits(spec, id);
            value.iter_mut().zip(&more).for_each(|(v, m)| *v |= m);
        }
        (value, known)
    }

    /// The titles carrying any value of an item only tentatively; `None` when none of its values has a tier.
    fn item_likely(&self, spec: &Spec, item: &Item) -> Option<Bits> {
        let mut union: Option<Bits> = None;
        for likely in item.ids.iter().filter_map(|id| self.likely_bits(spec, id)) {
            match union.as_mut() {
                Some(union) => union.iter_mut().zip(likely).for_each(|(u, l)| *u |= l),
                None => union = Some(likely.clone()),
            }
        }
        union
    }

    /// A value's titles within `base` — its confident bits, and its tentative ones — and how many of them
    /// count only through the tentative tier.
    fn counted(&self, base: &Matched, bits: &[u64], likely: Option<&Bits>) -> Counted {
        let total = and_count(&base.any, bits) + likely.map_or(0, |l| and_count(&base.any, l));
        Counted { total, likely: total - and_count(&base.sure, bits) }
    }

    /// The titles of the route type carrying every applied item except those of `skip`, confidently and with
    /// the tentative tier (`Matched`). `-kind:id` keeps the titles known for the kind and not carrying the
    /// value, and on a plot axis "known" is the confident tier alone: an exclusion never keeps a title for a
    /// tentative value, and a title whose axis is only tentative is dropped as unknown, whichever way it leans.
    /// A group matches a title carrying any of its values, in either tier; excluded, one known to carry none.
    fn matched(&self, applied: &[(&'static Spec, &Item)], skip: Option<&str>) -> Matched {
        self.matched_without(applied, &|spec, _| Some(spec.name) == skip)
    }

    /// `matched`, leaving out the items `drop` names.
    fn matched_without(
        &self,
        applied: &[(&'static Spec, &Item)],
        drop: &dyn Fn(&Spec, &Item) -> bool,
    ) -> Matched {
        let mut sure = self.filter.types[self.t()].clone();
        let mut any = sure.clone();
        for (spec, item) in applied {
            if drop(spec, item) {
                continue;
            }
            let (value, known) = self.item_bits(spec, item);
            if item.exclude {
                for matched in [&mut sure, &mut any] {
                    matched.iter_mut().zip(value.iter().zip(&known)).for_each(|(m, (v, k))| *m &= k & !v);
                }
                continue;
            }
            sure.iter_mut().zip(&value).for_each(|(m, v)| *m &= v);
            match self.item_likely(spec, item) {
                Some(likely) => {
                    any.iter_mut().zip(value.iter().zip(&likely)).for_each(|(m, (v, l))| *m &= v | l)
                }
                None => any.iter_mut().zip(&value).for_each(|(m, v)| *m &= v),
            }
        }
        Matched { sure, any }
    }

    /// What a kind's own values are counted under (`counts.json`, `values/<kind>.json`) when it differs from
    /// the whole selection: the selection without the kind's items that are alternatives to its values
    /// (`counted_apart`), so each value reads as what picking it, or adding it to the group, would give.
    /// `None` when the kind has no such item, and its values count under the whole selection.
    fn base_for(&self, spec: &Spec, applied: &[(&'static Spec, &Item)]) -> Option<Matched> {
        let apart = |s: &Spec, i: &Item| s.name == spec.name && counted_apart(s.mode, i);
        applied.iter().any(|(s, i)| apart(s, i)).then(|| self.matched_without(applied, &apart))
    }

    /// The titles of the route's type (of both, under `all`): what `total` and every coverage is out of.
    fn population(&self) -> usize {
        popcount(&self.filter.types[self.t()])
    }

    /// How much of the route type each applied kind is known for.
    fn coverage(&self, applied: &[(&'static Spec, &Item)]) -> Value {
        let population = self.population();
        let mut out = Map::new();
        for (spec, item) in applied {
            if out.contains_key(spec.name) {
                continue;
            }
            let (_, known) = self.value_bits(spec, &item.ids[0]);
            let count = and_count(&known, &self.filter.types[self.t()]);
            out.insert(spec.name.to_owned(), json!({ "count": count, "denominator": population }));
        }
        Value::Object(out)
    }

    /// Entity counts under `base`: every value credited, and how many titles each.
    fn entity_counts(&self, i: usize, base: &[u64]) -> Vec<(u32, u32)> {
        let entity = &ENTITY_KINDS[i];
        let Some(kind) = self.filter.entities[i].as_ref() else { return Vec::new() };
        let lists: Vec<_> =
            entity.sections.iter().filter_map(|&(v, o)| self.view.list::<u32>(v, o).ok()).collect();
        let size = self.view.column::<u32>("ent_qid").map_or(0, <[u32]>::len);
        let mut counts = vec![0u32; size];
        let mut touched: Vec<u32> = Vec::new();
        let mut seen: Vec<u32> = Vec::new();
        // A person both making and in a title is one title, so only a kind reading two lists needs to check.
        let merge = lists.len() > 1;
        for row in ones(base) {
            seen.clear();
            for list in &lists {
                for &e in list.get(den_store::Row(row)) {
                    if merge {
                        if seen.contains(&e) {
                            continue;
                        }
                        seen.push(e);
                    }
                    let Some(count) = counts.get_mut(e as usize) else { continue };
                    if *count == 0 {
                        touched.push(e);
                    }
                    *count += 1;
                }
            }
        }
        touched.into_iter().filter(|&e| self.listable(i, kind, e)).map(|e| (e, counts[e as usize])).collect()
    }

    /// Whether an entity kind lists and searches a value: enough titles, and in its `only` list if it has one.
    fn listable(&self, i: usize, kind: &EntityKind, entity: u32) -> bool {
        let spec = &ENTITY_KINDS[i];
        kind.titles(entity) >= spec.min_titles
            && (spec.only.is_empty()
                || self
                    .qid(entity)
                    .strip_prefix('Q')
                    .and_then(|q| q.parse().ok())
                    .is_some_and(|q| spec.only.contains(&q)))
    }

    /// Entity counts under `base`, each with how many of its titles are likely matches: the counts over `any`,
    /// less those over `sure` where the two differ.
    fn entity_tallies(&self, i: usize, base: &Matched) -> Vec<(u32, Counted)> {
        let counted = self.entity_counts(i, &base.any);
        // Compared once: the two are whole-corpus bitsets (~6 KB), and asked per entity this was most of
        // `counts.json` on the musl build, whose memcmp is a byte loop — ~400 ms a request against ~40 ms.
        let split = base.sure != base.any;
        let sure: HashMap<u32, u32> =
            if split { self.entity_counts(i, &base.sure).into_iter().collect() } else { HashMap::new() };
        counted
            .into_iter()
            .map(|(e, n)| {
                let likely = if split { n - sure.get(&e).copied().unwrap_or(0) } else { 0 };
                (e, Counted { total: n as usize, likely: likely as usize })
            })
            .collect()
    }

    /// One kind's object in `counts.json`. `values` counts both tiers; `likely` says how many of each count
    /// are likely matches, for a value that has any.
    fn kind_answer(&self, spec: &Spec, base: &Matched, selected: &[&Item]) -> Value {
        let mut values = Map::new();
        let mut likely = Map::new();
        let mut labels = Map::new();
        let mut complete = true;
        let mut put = |values: &mut Map<String, Value>, id: String, tally: Counted| {
            if tally.likely > 0 {
                likely.insert(id.clone(), tally.likely.into());
            }
            values.insert(id, tally.total.into());
        };
        match spec.data {
            Data::Bits | Data::Rating => {
                let valued = if spec.data == Data::Rating {
                    self.derived.rating.as_ref()
                } else {
                    self.filter.bits.get(spec.name)
                };
                if let Some(valued) = valued {
                    for (value, bits) in &valued.values {
                        let tally = self.counted(base, bits, self.likely_bits(spec, value));
                        if tally.total > 0 {
                            put(&mut values, value.clone(), tally);
                        }
                    }
                }
            }
            Data::Entity(i) => {
                let mut counted = self.entity_tallies(i, base);
                counted.sort_unstable_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(&b.0)));
                complete = counted.len() <= TOP_K;
                for &(e, tally) in counted.iter().take(TOP_K) {
                    let id = self.qid(e);
                    if let Some(label) = self.label(e) {
                        labels.insert(id.clone(), label.into());
                    }
                    put(&mut values, id, tally);
                }
            }
            Data::Character | Data::Like => complete = false,
        }
        // Every selected value, a group's each, with its label, even at 0.
        for id in selected.iter().flat_map(|item| &item.ids) {
            if !values.contains_key(id) {
                let (bits, _) = self.value_bits(spec, id);
                let tally = self.counted(base, &bits, self.likely_bits(spec, id));
                put(&mut values, id.clone(), tally);
            }
            match spec.data {
                Data::Entity(_) => {
                    if let Some(label) = self.entity_of(id).and_then(|e| self.label(e)) {
                        labels.insert(id.clone(), label.into());
                    }
                }
                Data::Character => {
                    labels.insert(id.clone(), id.replace('-', " ").into());
                }
                _ => {}
            }
        }
        if spec.name == "region" {
            for id in values.keys() {
                if let Some(region) = den_index::region(id) {
                    labels.insert(id.clone(), region.label.into());
                }
            }
        }
        if spec.name == "studio" {
            for id in values.keys() {
                if let Some(studio) = self.indexes.studios.get(id) {
                    labels.insert(id.clone(), studio.name.clone().into());
                }
            }
        }
        // What the values are counted out of: the selection, or for a one-pick kind with its pick made or an and
        // kind with an OR group, the selection without them — so `tone.values.comic` may exceed `total`.
        let mut answer = json!({
            "mode": spec.mode.name(), "complete": complete, "values": values, "denominator": popcount(&base.any),
        });
        if !likely.is_empty() {
            answer["likely"] = Value::Object(likely);
        }
        if !labels.is_empty() {
            answer["labels"] = Value::Object(labels);
        }
        let (positive, excluded) = selected_ids(selected);
        if !positive.is_empty() {
            answer["selected"] = json!(positive);
        }
        if !excluded.is_empty() {
            answer["excluded"] = json!(excluded);
        }
        answer
    }

    /// The kinds object and total for a selection; the empty selection's is worked out once per type.
    fn kinds(&self, applied: &[(&'static Spec, &Item)]) -> (Value, Counted) {
        if applied.is_empty() {
            return self.derived.empty[self.t()].get_or_init(|| self.work_kinds(&[])).clone();
        }
        self.work_kinds(applied)
    }

    fn work_kinds(&self, applied: &[(&'static Spec, &Item)]) -> (Value, Counted) {
        let matched = self.matched(applied, None);
        let total = popcount(&matched.any);
        let tally = Counted { total, likely: total - popcount(&matched.sure) };
        let mut kinds = Map::new();
        for spec in SPECS.iter().filter(|s| self.status(s) == Status::Ready) {
            let selected: Vec<&Item> =
                applied.iter().filter(|(s, _)| s.name == spec.name).map(|(_, i)| *i).collect();
            if !spec.listed() && selected.is_empty() {
                continue;
            }
            // A one-pick kind counts each value as the alternative pick, and an and kind with an OR group each
            // value as one the group could add: without those items applied.
            let answer = match self.base_for(spec, applied) {
                Some(base) => self.kind_answer(spec, &base, &selected),
                None => self.kind_answer(spec, &matched, &selected),
            };
            kinds.insert(spec.name.to_owned(), answer);
        }
        (Value::Object(kinds), tally)
    }

    /// The overall split, beside `total`, for a store with the tier: `confident` titles match every plot axis
    /// selected with a published value, `likely` ones need the tentative tier for at least one.
    fn split_totals(&self, answer: &mut Value, tally: Counted) {
        if self.tier() {
            answer["confident"] = json!(tally.total - tally.likely);
            answer["likely"] = json!(tally.likely);
        }
    }

    /// Whether a value is one its kind holds at all, so a typo (`tone:blaek`, `mood:tense` for `Tense`) is told
    /// apart from a real zero.
    fn known_value(&self, spec: &Spec, id: &str) -> bool {
        match spec.data {
            // A region is known by the table even when no title here carries it.
            Data::Bits if spec.name == "region" => den_index::region(id).is_some(),
            Data::Bits => self.filter.bits.get(spec.name).is_some_and(|v| v.values.contains_key(id)),
            Data::Rating => self.derived.rating.as_ref().is_some_and(|v| v.values.contains_key(id)),
            Data::Entity(_) => self.entity_of(id).is_some(),
            Data::Character => {
                self.characters.as_ref().is_some_and(|c| !c.named().rows(&id.replace('-', " ")).is_empty())
            }
            Data::Like => self.like_key(id).is_some_and(|(media_type, tmdb_id)| {
                self.view.row_of(u8::from(media_type == MediaType::Tv), tmdb_id).ok().flatten().is_some()
            }),
        }
    }

    /// What every answer carries: the kinds it did not apply, the values it did not know, and the kinds this
    /// atlas should answer and cannot. The flag says the answer is degraded: a failure at runtime (the facts
    /// or facet rows did not load, a ratings join has not landed), so it should be cached briefly.
    fn envelope(&self, answer: &mut Value, applied: &[(&'static Spec, &Item)], ignored: Vec<String>) -> bool {
        answer["ignored"] = json!(ignored);
        // Each value by itself: a group keeps its known values, and names the others here.
        let unknown: Vec<String> = applied
            .iter()
            .flat_map(|(s, i)| {
                i.ids.iter().filter(|id| !self.known_value(s, id)).map(|id| i.spelled_value(id))
            })
            .collect();
        if !unknown.is_empty() {
            answer["unknownValues"] = json!(unknown);
        }
        let unavailable = self.unavailable();
        if !unavailable.is_empty() {
            answer["kindsUnavailable"] = json!(unavailable);
        }
        !unavailable.is_empty()
    }

    /// `counts.json`. The flag says a kind this atlas should answer is unavailable.
    pub fn counts(&self, request: &Request) -> (Value, bool) {
        let (applied, ignored) = self.split(&request.items);
        let (kinds, tally) = self.kinds(&applied);
        let mut answer = json!({
            "total": tally.total, "denominator": self.population(), "kinds": kinds,
            "coverage": self.coverage(&applied),
        });
        self.split_totals(&mut answer, tally);
        let degraded = self.envelope(&mut answer, &applied, ignored);
        (answer, degraded)
    }

    /// A likely match's rank key: the lowest probability among the plot axes it carries only tentatively.
    /// `plot` is each selected plot item that has a tier: its axis, and its values' confident titles.
    fn lowest_tentative(tier: &den_store::TentativeFacets<'_>, plot: &[(usize, Bits)], row: usize) -> u8 {
        plot.iter()
            .filter(|(_, sure)| !has(sure, row))
            .filter_map(|&(axis, _)| tier.get(den_store::Row(row), axis).map(|t| t.probability))
            .min()
            .unwrap_or(0)
    }

    /// `titles.json`: the titles carrying the selection, most voted first — or, with a `like` selected, in
    /// its similarity order — as `/index/row`'s cards. Every confident match comes first; the likely ones
    /// follow, by their lowest tentative probability and then in the same order, each card marked `likely`.
    pub fn titles(&self, request: &Request) -> (Value, bool) {
        let (applied, ignored) = self.split(&request.items);
        let matched = self.matched(&applied, None);
        let total = popcount(&matched.any);
        let confident = popcount(&matched.sure);
        let like = applied
            .iter()
            .find(|(s, i)| s.data == Data::Like && !i.exclude && self.like_key(&i.ids[0]).is_some())
            .map(|(_, i)| i.ids[0].as_str());
        let (order, order_id): (Vec<u32>, String) = match like {
            Some(id) => (self.like_rows(id), format!("like:{id}")),
            None => (self.derived.order[self.t()].clone(), self.derived.order_id[self.t()].clone()),
        };
        let sure = order.iter().copied().filter(|&row| has(&matched.sure, row as usize));
        // Ranked only when the page reaches past the confident matches.
        let mut likely: Vec<(u8, usize, u32)> = Vec::new();
        if request.skip + request.limit > confident && total > confident {
            let tier = self.view.tentative_facets().unwrap_or_default();
            let plot: Vec<(usize, Bits)> = applied
                .iter()
                .filter(|(spec, item)| !item.exclude && self.item_likely(spec, item).is_some())
                .filter_map(|(spec, item)| {
                    let axis = den_store::FACET_AXES.iter().position(|a| *a == spec.name)?;
                    Some((axis, self.item_bits(spec, item).0))
                })
                .collect();
            likely = order
                .iter()
                .enumerate()
                .filter(|&(_, &row)| has(&matched.any, row as usize) && !has(&matched.sure, row as usize))
                .map(|(at, &row)| (Self::lowest_tentative(&tier, &plot, row as usize), at, row))
                .collect();
            likely.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        let titles: Vec<Value> = match self.indexes.cards.as_ref() {
            Some(cards) => sure
                .map(|row| (row, false))
                .chain(likely.iter().map(|&(_, _, row)| (row, true)))
                .skip(request.skip)
                .take(request.limit)
                .filter_map(|(row, tentative)| {
                    let key = self.filter.keys[row as usize];
                    let mut title = crate::plotrows::title_json(self.indexes, key, cards.get(&key)?);
                    if tentative {
                        title["likely"] = json!(true);
                    }
                    Some(title)
                })
                .collect(),
            None => Vec::new(),
        };
        let mut answer = json!({
            "titles": titles, "total": total, "denominator": self.population(), "order": order_id,
            "coverage": self.coverage(&applied),
        });
        self.split_totals(&mut answer, Counted { total, likely: total - confident });
        let degraded = self.envelope(&mut answer, &applied, ignored);
        (answer, degraded)
    }

    /// `values/<kind>.json`: the kind's values under the selection, labelled, most titles first — those whose
    /// name has a word starting `q` when one is given.
    pub fn values(&self, spec: &'static Spec, request: &Request) -> (Value, bool) {
        let (applied, ignored) = self.split(&request.items);
        let base = self.base_for(spec, &applied).unwrap_or_else(|| self.matched(&applied, None));
        let q = request.q.as_deref();
        // Every value matches a search that names none, all alike.
        let tier = |name: &str| match q {
            Some(q) => match_tier(&crate::facts::name_key(name), q),
            None => Some(0),
        };
        // How many of `rows` the selection holds, and how many of those only through the tentative tier.
        let rows_tally = |rows: &[u32]| {
            let total = rows.iter().filter(|&&r| has(&base.any, r as usize)).count();
            Counted { total, likely: total - rows.iter().filter(|&&r| has(&base.sure, r as usize)).count() }
        };
        // (match tier, id, name, count, tiebreak, tmdb)
        let mut found: Vec<(u8, String, String, Counted, usize, Option<u32>)> = Vec::new();
        // Values counted but left unnamed past the page, so `complete` still counts them.
        let mut beyond = 0;
        if self.status(spec) == Status::Ready {
            match spec.data {
                Data::Bits | Data::Rating => {
                    let valued = if spec.data == Data::Rating {
                        self.derived.rating.as_ref()
                    } else {
                        self.filter.bits.get(spec.name)
                    };
                    for (value, bits) in valued.map(|v| &v.values).into_iter().flatten() {
                        // A region is named by its label and found by its label, slug or aliases; a studio
                        // by its name.
                        let region = den_index::region(value).filter(|_| spec.name == "region");
                        let studio = self.indexes.studios.get(value).filter(|_| spec.name == "studio");
                        let name = region
                            .map_or_else(|| studio.map_or(value.as_str(), |s| s.name.as_str()), |r| r.label);
                        let matched = match region {
                            Some(r) => {
                                [r.label, r.slug].iter().chain(r.aliases).filter_map(|n| tier(n)).min()
                            }
                            None => tier(name),
                        };
                        let tally = self.counted(&base, bits, self.likely_bits(spec, value));
                        if let (true, Some(matched)) = (tally.total > 0, matched) {
                            let all = and_count(&self.filter.types[self.t()], bits);
                            found.push((matched, value.clone(), name.to_owned(), tally, all, None));
                        }
                    }
                }
                Data::Entity(i) => {
                    if let Some(kind) = self.filter.entities[i].as_ref() {
                        let candidates: Vec<(u32, u8, Counted)> = match q {
                            Some(q) => self
                                .filter
                                .names(self.indexes)
                                .matching(q)
                                .into_iter()
                                .filter(|&(e, _)| self.listable(i, kind, e))
                                .filter_map(|(e, tier)| {
                                    let mut rows: Vec<u32> =
                                        kind.postings.iter().flat_map(|p| p.of(e).iter().copied()).collect();
                                    rows.sort_unstable();
                                    rows.dedup();
                                    let tally = rows_tally(&rows);
                                    (tally.total > 0).then_some((e, tier, tally))
                                })
                                .collect(),
                            None => self
                                .entity_tallies(i, &base)
                                .into_iter()
                                .map(|(e, tally)| (e, 0, tally))
                                .collect(),
                        };
                        // A person kind can count hundreds of thousands of values, so only those that can
                        // reach the page are named: the top `limit` by match, count and titles, and any tied
                        // with the last of them (the name decides among those, below).
                        let mut ranked: Vec<(u32, u8, Counted, usize)> =
                            candidates.into_iter().map(|(e, tier, n)| (e, tier, n, kind.titles(e))).collect();
                        beyond += ranked.len().saturating_sub(request.limit);
                        let rank = |&(_, tier, n, titles): &(u32, u8, Counted, usize)| {
                            (tier, std::cmp::Reverse(n.total), std::cmp::Reverse(titles))
                        };
                        ranked.sort_unstable_by_key(rank);
                        if let Some(last) = ranked.get(request.limit.saturating_sub(1)).map(rank) {
                            ranked.retain(|value| rank(value) <= last);
                        }
                        for (e, tier, n, titles) in ranked {
                            let name = self.label(e).unwrap_or_default().to_owned();
                            found.push((tier, self.qid(e), name, n, titles, self.tmdb(e)));
                        }
                    }
                }
                Data::Character => {
                    if let (Some(characters), Some(q)) = (self.characters.as_ref(), q) {
                        for (name, rows) in characters.named().with_prefix(q) {
                            let tally = rows_tally(rows);
                            if let (true, Some(tier)) = (tally.total > 0, match_tier(name, q)) {
                                found.push((
                                    tier,
                                    name.replace(' ', "-"),
                                    name.to_owned(),
                                    tally,
                                    rows.len(),
                                    None,
                                ));
                            }
                        }
                    }
                }
                Data::Like => {}
            }
        }
        found.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(b.3.total.cmp(&a.3.total))
                .then(b.4.cmp(&a.4))
                .then(a.2.cmp(&b.2))
                .then(a.1.cmp(&b.1))
        });
        let complete = found.len() <= request.limit && beyond == 0;
        let values: Vec<Value> = found
            .into_iter()
            .take(request.limit)
            .map(|(_, id, name, tally, _, tmdb)| {
                let mut value = json!({ "id": id, "name": name, "count": tally.total });
                if tally.likely > 0 {
                    value["likely"] = json!(tally.likely);
                }
                if let Some(tmdb) = tmdb {
                    value["tmdbId"] = json!(tmdb);
                }
                value
            })
            .collect();
        let mut answer = json!({
            "kind": spec.name, "mode": spec.mode.name(), "values": values, "complete": complete,
            "denominator": popcount(&base.any),
        });
        let degraded = self.envelope(&mut answer, &applied, ignored);
        (answer, degraded)
    }
}

/// The kinds, as `/index/schema.json` describes them: how each combines, how its ids are written, whether
/// counts list its values whole, the top `TOP_K`, or none (search-only), and the axis aliases a client
/// canonicalises through.
pub fn schema() -> Value {
    let kinds: Map<String, Value> = SPECS
        .iter()
        .map(|spec| {
            let listing = match spec.data {
                Data::Entity(_) => "top",
                Data::Character => "search",
                Data::Like => "selected",
                _ => "full",
            };
            let mut about = json!({ "mode": spec.mode.name(), "id": spec.id.format(), "listing": listing, "about": spec.about });
            if let Data::Entity(i) = spec.data {
                about["top"] = json!(TOP_K);
                about["minTitles"] = json!(ENTITY_KINDS[i].min_titles);
                if ENTITY_KINDS[i].series_only {
                    about["appliesTo"] = json!("series");
                }
            }
            if let Some(&(_, _, _, floor, _)) = DENSE_KINDS.iter().find(|d| d.0 == spec.name) {
                about["minScore"] = json!(f64::from(floor) / 100.0);
            }
            if spec.data == Data::Character {
                about["minPrefix"] = json!(CHARACTER_MIN_PREFIX);
                about["maxResults"] = json!(CHARACTER_LIMIT);
            }
            if spec.tmdbs() {
                about["source"] = json!("tmdb");
            }
            (spec.name.to_owned(), about)
        })
        .collect();
    let merged: Map<String, Value> = crate::plotrows::MERGED_ROWS
        .iter()
        .map(|m| (format!("{}:{}", m.axis, m.value), json!(m.members)))
        .collect();
    json!({
        "types": ["movie", "series", "all"],
        "all": "films and series together: counts and values over both; titles are the two types' own orders \
                merged by rank within type (a title's position in its type's order over that type's size, \
                ascending, films first on a tie), so series run at their share, spread evenly, and each card \
                names its type. Every kind answers for both; a kind or value only one type has \
                (network, a series-only genre, a composite genre) matches only that type's titles, and a film \
                genre id matches the series filed under it too. like names its title's type: \
                like:movie-550, like:series-1396; a bare id is a 400 there",
        "kinds": kinds,
        "aliases": {
            "structure": {
                "single-day": "timespan", "anthology": "continuity", "*": "chronology",
                "about": "structure:<value> is the axis named here for that value, chronology for any other",
            },
        },
        "merged": merged,
        "regions": den_index::REGIONS
            .iter()
            .map(|r| json!({ "slug": r.slug, "label": r.label, "aliases": r.aliases, "countries": r.countries }))
            .collect::<Vec<_>>(),
        "runtimeBuckets": RUNTIME_BUCKETS.iter().map(|b| b.0).collect::<Vec<_>>(),
        "ratingThresholds": RATING_THRESHOLDS,
        "ratingMinVotes": MIN_VOTES,
        "exclude": "-<kind>:<id>: the titles known for the kind and not carrying the value. Stricter than a \
                    search's negation (/index/query.json, \"not british\"), which drops the titles on record as \
                    carrying the value and keeps the unknown ones: the two can answer different sets. On a plot \
                    axis only a confident value makes a title known: a title whose axis is only likely is \
                    dropped by an exclusion on that axis, whichever value it leans to, and an exclusion adds \
                    no likely matches",
        "likely": "plot axes carry two tiers. A confident value passed the dataset's publication gates; a likely \
                   one is the model's best guess where it was not sure (probability 0.50 to 0.70, about 8 in \
                   10 right against 9 to 10). A plot-axis selection matches both, and a title that needs a \
                   likely value for any selected axis is a likely match. Where the store carries likely values: \
                   counts.json and titles.json add confident and likely beside total (total = confident + \
                   likely); each kind's likely map says how many of values[id] are likely matches, listing \
                   only non-zero ones (confident = values - likely); a values/<kind>.json entry carries \
                   likely the same way; titles.json lists every confident match first, then the likely ones, \
                   highest lowest-probability first and then in the route's order, each card marked \
                   likely: true; people.json counts people on both. A store without likely values answers \
                   none of these fields, and every count is confident",
        "unknownValues": "selected values the kind does not hold (a typo, a label's wrong case), each as \
                          [-]<kind>:<id>, a group's one by one: they match nothing, and are named so a client can \
                          tell them from a real zero",
        "canonical": "sel items [-]<kind>:<id>, ids normalised per kind, sorted by kind, then positive before \
                      excluded, then id as strings, each once, joined by ','; ids encoded as encodeURIComponent \
                      does, ':' ',' '-' '|' literal. An OR group's ids are sorted as strings and each kept once, \
                      joined by '|', and the group sorts among the items by that joined id; a group left with one \
                      id is written without '|'. Then traits (people), written the same way, and order (people, \
                      lowercased); then skip and limit (titles, people) or q and limit (values, people values), \
                      each only when not its default. Any other \
                      spelling answers privately with Content-Location naming this one.",
        "or": "<kind>:<id>|<id>… is one item matching any of its values: country:FR|IT is French or Italian, \
               decade:1980|1990 the 1980s or 1990s. Separate items still AND, within a kind and across kinds: \
               country:FR,country:IT is both, country:FR|IT,decade:1990 either of the 1990s. -<kind>:<a>|<b> is \
               neither: the titles known for the kind carrying none of the values. Every kind takes a group but \
               like (one title an item); a group's values must resolve to one kind (structure:single-day|nonlinear \
               is refused). A group counts toward maxSelection by its values. '|' is literal in the canonical \
               URL, and %7C reads the same. A value the kind does not hold is named in unknownValues as \
               [-]<kind>:<id> and the group's other values apply; a group of none matches nothing. On a plot \
               axis a group matches either tier as a lone value does, and its exclusion reads the confident \
               tier alone. In counts.json and values/<kind>.json a kind with a positive group is counted \
               without its groups (a one-pick kind without any of its items), so each value is what \
               adding it to the group would give; every other kind is counted under the group. selected and \
               excluded list every id of the kind's items, a group's each",
        "traits": people::schema(),
        "maxSelection": MAX_SELECTION,
        "maxQuery": MAX_QUERY,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::IndexQueries;
    use den_index::MediaType::{Movie, Tv};

    fn dataset(name: &str) -> crate::dataset::Dataset {
        let dir = std::env::temp_dir().join(format!("den-atlas-filter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::queries::write_fixture(&dir)
    }

    fn fixture(name: &str) -> Indexes {
        crate::queries::load_for_tools(&dataset(name)).expect("the fixture loads")
    }

    /// A request to a one-type route: movie and series parse alike.
    fn request(route: Route, query: &str) -> Request {
        Request::parse(route, Scope::Type(Movie), query).expect("a well-formed request")
    }

    fn request_all(route: Route, query: &str) -> Request {
        Request::parse(route, Scope::All, query).expect("a well-formed request")
    }

    fn counts(indexes: &Indexes, scope: impl Into<Scope>, query: &str) -> Value {
        let scope = scope.into();
        Context::new(indexes, scope, None).counts(&Request::parse(Route::Counts, scope, query).unwrap()).0
    }

    fn ids(answer: &Value) -> Vec<u64> {
        answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
    }

    /// Movies 1–3 of the fixture: 1 is Korean and Danish from 1985, genres 80 and 18, Tense and a Heist,
    /// violent, 95 minutes; 2 is Korean from 1995, a Heist, 130 minutes; 3 is Spanish from 1985, a Heist
    /// and Campy/Cult, 160 minutes.
    #[test]
    fn counts_carry_the_selection_and_each_value() {
        let indexes = fixture("and");
        let all = counts(&indexes, Movie, "");
        assert_eq!(all["total"], 3);
        let kinds = &all["kinds"];
        assert_eq!(kinds["country"]["values"], json!({ "DK": 1, "ES": 1, "KR": 2 }), "every country listed");
        assert_eq!(kinds["country"]["complete"], true);
        assert_eq!(kinds["decade"]["values"], json!({ "1980": 2, "1990": 1 }), "the card's year");
        assert_eq!(kinds["subgenre"]["values"], json!({ "Campy/Cult": 1, "Heist": 3 }));
        assert_eq!(kinds["ending"]["values"], json!({ "bittersweet": 3, "unhappy": 3 }), "a merged row");
        assert_eq!(kinds["runtime"]["values"], json!({ "90-120": 1, "120-150": 1, "over-150": 1 }));
        assert_eq!(kinds["primary"]["values"], json!({ "Comedy": 1, "Drama": 2 }));
        assert_eq!(kinds["warning"]["values"], json!({ "violence": 1 }), "at the floor, not under it");
        assert_eq!(kinds["source"]["values"], json!({ "book": 1, "play": 1 }));
        assert!(kinds.get("network").is_none(), "a series-only kind is no film's");
        assert!(kinds.get("character").is_none() && kinds.get("like").is_none(), "never listed");

        let korean = counts(&indexes, Movie, "sel=country:KR");
        assert_eq!(korean["total"], 2);
        assert_eq!(korean["kinds"]["subgenre"]["values"], json!({ "Heist": 2 }));
        assert_eq!(korean["kinds"]["country"]["selected"], json!(["KR"]));

        let stacked = counts(&indexes, Movie, "sel=country:KR,tone:bleak,mood:Tense");
        assert_eq!(stacked["total"], 1);
        assert_eq!(
            counts(&indexes, Tv, "sel=country:KR")["kinds"]["subgenre"]["values"],
            json!({ "Heist": 1 })
        );
    }

    /// A one-pick kind counts its values as the alternative pick: the other decade under the rest of the
    /// selection, not under this decade too (which would be 0 by construction).
    #[test]
    fn a_single_kind_counts_each_value_without_its_own_pick() {
        let indexes = fixture("single");
        let picked = counts(&indexes, Movie, "sel=country:KR,decade:1980");
        assert_eq!(picked["total"], 1);
        assert_eq!(picked["kinds"]["decade"]["mode"], "single");
        assert_eq!(picked["kinds"]["decade"]["values"], json!({ "1980": 1, "1990": 1 }));
        assert_eq!(picked["kinds"]["decade"]["selected"], json!(["1980"]));
        // An AND kind counts under the whole selection.
        assert_eq!(picked["kinds"]["country"]["values"], json!({ "DK": 1, "KR": 1 }));
        // Each count is out of the titles it was counted among, which the answer names: the decade's out of
        // the two Korean films, everything else out of the one Korean film of the 1980s, and total out of the
        // three films.
        assert_eq!(picked["kinds"]["decade"]["denominator"], 2);
        assert_eq!(picked["kinds"]["country"]["denominator"], 1);
        assert_eq!(picked["denominator"], 3);
    }

    /// Every filter answer names what its counts are out of, the empty selection's too, where no coverage would.
    #[test]
    fn every_filter_answer_names_its_denominator() {
        let indexes = fixture("denominator");
        let context = Context::new(&indexes, Scope::Type(Movie), None);
        let empty = counts(&indexes, Movie, "");
        assert_eq!((&empty["denominator"], &empty["coverage"]), (&3.into(), &json!({})));
        assert_eq!(empty["kinds"]["country"]["denominator"], 3);
        let titles = context.titles(&request(Route::Titles, "sel=country:KR")).0;
        assert_eq!((&titles["total"], &titles["denominator"]), (&2.into(), &3.into()));
        let spec = spec("country").unwrap();
        let values = context.values(spec, &request(Route::Values(spec), "sel=decade:1980")).0;
        assert_eq!(values["denominator"], 2, "the two films of the 1980s");
        assert_eq!(counts(&indexes, Tv, "")["denominator"], 1, "the one series with a card");
    }

    /// An exclusion keeps the titles KNOWN not to carry the value, and every answer says how much of the
    /// type the kinds it applied are known for.
    #[test]
    fn an_exclusion_keeps_what_is_known_not_to_carry_it() {
        let indexes = fixture("exclude");
        let calm = counts(&indexes, Movie, "sel=-warning:violence");
        assert_eq!(calm["total"], 2, "movie 1 depicts it; 2 scores under the floor; 3 was described");
        assert_eq!(calm["kinds"]["warning"]["excluded"], json!(["violence"]));
        assert_eq!(calm["kinds"]["warning"]["values"]["violence"], 0, "an excluded id is listed, at 0");
        assert_eq!(calm["coverage"]["warning"], json!({ "count": 3, "denominator": 3 }));
        // A title nothing is known about for a kind is not assumed clean: only movie 1 credits a company.
        let known = counts(&indexes, Movie, "sel=-company:Q60");
        assert_eq!(known["total"], 0, "only movie 1 credits a company, and it is that one");
        assert_eq!(known["coverage"]["company"], json!({ "count": 1, "denominator": 3 }));
    }

    /// Entity kinds list their top values, labelled, and every selected id with its label even when too thin
    /// to be listed — a studio needs five titles to be offered, but may always be asked for.
    #[test]
    fn entity_kinds_list_labelled_top_values_and_every_selected_one() {
        let indexes = fixture("entities");
        let all = counts(&indexes, Movie, "");
        let person = &all["kinds"]["person"];
        assert_eq!(person["mode"], "and");
        assert_eq!(person["values"]["Q2"], 1);
        assert_eq!(person["labels"]["Q2"], "Lead Actor");
        assert_eq!(person["complete"], true);
        assert_eq!(
            all["kinds"]["company"]["values"],
            json!({}),
            "one title is under the five a studio needs"
        );

        let studio = counts(&indexes, Movie, "sel=company:Q60");
        assert_eq!(studio["total"], 1);
        assert_eq!(studio["kinds"]["company"]["values"], json!({ "Q60": 1 }));
        assert_eq!(studio["kinds"]["company"]["labels"], json!({ "Q60": "A Studio" }));
        assert_eq!(studio["kinds"]["company"]["selected"], json!(["Q60"]));
        let nobody = counts(&indexes, Movie, "sel=person:Q999999");
        assert_eq!(
            (&nobody["total"], &nobody["kinds"]["person"]["values"]["Q999999"]),
            (&0.into(), &0.into())
        );
        let feature = counts(&indexes, Movie, "sel=format:Q90");
        assert_eq!(feature["total"], 2);
        assert_eq!(feature["kinds"]["format"]["labels"], json!({ "Q90": "feature film" }));
    }

    /// A kind whose source did not load is reported unavailable, and a selection naming it is answered around
    /// it — ignored, never read as zero.
    #[test]
    fn an_unloaded_kind_is_unavailable_and_ignored() {
        let mut indexes = fixture("unloaded");
        indexes.facts = None;
        indexes.plot_facets = None;
        let filter = FilterIndex::build(&indexes);
        for kind in ["genre", "country", "language", "tone", "ending"] {
            assert!(filter.unavailable.contains(&kind), "{kind}");
            assert!(!filter.bits.contains_key(kind), "{kind}");
        }
        assert!(filter.bits.contains_key("decade"), "the cards still date them");
    }

    #[tokio::test]
    async fn rating_reads_tmdb_with_a_vote_floor() {
        let ds = dataset("rating");
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        let index = crate::ratings::build(&mapped.view(), &HashMap::from([((0, 1), (8.4, 9000))])).unwrap();
        let ratings = Arc::new(crate::ratings::Ratings::with_index(index));
        let queries = IndexQueries::new(&ds).with_ratings(Some(ratings));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let all = counts(&indexes, Movie, "");
        assert_eq!(all["kinds"]["rating"]["values"], json!({ "6": 1, "7": 1, "8": 1 }));
        assert_eq!(all["kinds"]["rating"]["mode"], "single");
        let good = counts(&indexes, Movie, "sel=rating:8");
        assert_eq!(good["total"], 1);
        assert_eq!(good["coverage"]["rating"], json!({ "count": 1, "denominator": 3 }));
        // No score beyond the thresholds reaches an answer.
        assert!(!all.to_string().contains("8.4"));
        // Movie 1's 9,000 votes put it first.
        let titles = Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "")).0;
        assert_eq!(ids(&titles), vec![1, 2, 3]);
    }

    /// Whether `warning` answers is read off the loaded store, not declared: a store whose `depicts` scores
    /// reach the floor offers it (the route fixture's movie 1 scores 0.85), and one whose scores all stay under
    /// it — as the title-only scores of store 5b1c3213b6a1 did — does not offer it at all: not listed, not
    /// unavailable, the answer not degraded. That is the dataset version, not a failure.
    #[test]
    fn warning_is_offered_exactly_when_the_store_s_depicts_reach_the_floor() {
        let reaching = fixture("warning-reaching");
        let context = Context::new(&reaching, Movie, None);
        assert_eq!(context.status(spec("warning").unwrap()), Status::Ready);
        assert_eq!(
            context.counts(&request(Route::Counts, "")).0["kinds"]["warning"]["values"],
            json!({ "violence": 1 })
        );

        let dir = std::env::temp_dir().join(format!("den-atlas-filter-warning-low-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let title = |tmdb_id, violence| crate::store::fixture::Title {
            media: 0,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(2000))),
            depicts: vec![("graphic_violence", violence)],
            ..crate::store::fixture::Title::default()
        };
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &[title(1, 17), title(2, 28)], &[]);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the low-scoring store loads");
        let low = crate::queries::load_for_tools(&ds).expect("its indexes load");
        let context = Context::new(&low, Movie, None);
        assert_eq!(context.status(spec("warning").unwrap()), Status::NotOffered);
        let (answer, degraded) = context.counts(&request(Route::Counts, "sel=-warning:graphic_violence"));
        assert!(answer.get("kindsUnavailable").is_none(), "{answer}");
        assert!(answer["kinds"].get("warning").is_none(), "nothing to offer");
        assert_eq!(answer["ignored"], json!(["warning"]), "not answered with every title clean");
        assert!(!degraded, "a property of the dataset, not an outage");
    }

    /// Six films in both tiers, most voted first: 3 (1000 votes), 2 (500), 1 (100), 4 (50), 5 (10), 6 (5).
    /// Confident: 1 ending=bittersweet, 4 ending=happy, 5 and 6 tone=bleak. Tentative: 2 bittersweet at 0.65,
    /// 3 bittersweet at 0.55, 5 happy at 0.60, 6 open at 0.52. Films 1 and 2 are French, 3 American.
    fn tentative_fixture(name: &str) -> Indexes {
        use crate::store::fixture::Title;
        let dir = std::env::temp_dir().join(format!("den-atlas-filter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let film = |tmdb_id, votes, country| Title {
            media: 0,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A film", None, Some(2000))),
            votes,
            countries: vec![country],
            ..Title::default()
        };
        let titles = [
            Title { facets: vec![("ending", "bittersweet", 90)], ..film(1, 100, "FR") },
            Title { tentative: vec![("ending", "bittersweet", 65)], ..film(2, 500, "FR") },
            Title { tentative: vec![("ending", "bittersweet", 55)], ..film(3, 1000, "US") },
            Title { facets: vec![("ending", "happy", 90)], ..film(4, 50, "US") },
            Title {
                facets: vec![("tone", "bleak", 90)],
                tentative: vec![("ending", "happy", 60)],
                ..film(5, 10, "US")
            },
            Title {
                facets: vec![("tone", "bleak", 90)],
                tentative: vec![("ending", "open", 52)],
                ..film(6, 5, "US")
            },
        ];
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &titles, &[]);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the fixture loads");
        crate::queries::load_for_tools(&ds).expect("its indexes load")
    }

    /// A plot-axis selection matches both tiers: `total` counts every title, `confident` and `likely` split
    /// it, and each kind's `likely` map says how much of each value's count is likely.
    #[test]
    fn a_plot_axis_counts_its_likely_titles_and_says_so() {
        let indexes = tentative_fixture("likely-counts");
        let bittersweet = counts(&indexes, Movie, "sel=ending:bittersweet");
        assert_eq!(
            (&bittersweet["total"], &bittersweet["confident"], &bittersweet["likely"]),
            (&3.into(), &1.into(), &2.into())
        );
        // A one-pick kind counts its values without its own pick, so over all six films. The merged rows
        // count too: `unhappy` holds bittersweet, `unresolved` holds open.
        let ending = &bittersweet["kinds"]["ending"];
        assert_eq!(
            ending["values"],
            json!({ "bittersweet": 3, "happy": 2, "open": 1, "unhappy": 3, "unresolved": 1 })
        );
        assert_eq!(
            ending["likely"],
            json!({ "bittersweet": 2, "happy": 1, "open": 1, "unhappy": 2, "unresolved": 1 })
        );
        // An AND kind counts under the whole selection, likely titles included.
        assert_eq!(bittersweet["kinds"]["country"]["values"], json!({ "FR": 2, "US": 1 }));
        assert_eq!(bittersweet["kinds"]["country"]["likely"], json!({ "FR": 1, "US": 1 }));

        let french = counts(&indexes, Movie, "sel=country:FR,ending:bittersweet");
        assert_eq!((&french["total"], &french["likely"]), (&2.into(), &1.into()));
        // Stacked: film 5 is bleak confidently and happy only tentatively, so it is a likely match.
        let stacked = counts(&indexes, Movie, "sel=ending:happy,tone:bleak");
        assert_eq!(
            (&stacked["total"], &stacked["confident"], &stacked["likely"]),
            (&1.into(), &0.into(), &1.into())
        );
        // A value only the tentative tier holds is still a value, not an unknown one.
        let open = counts(&indexes, Movie, "sel=ending:open");
        assert_eq!((&open["total"], &open["likely"]), (&1.into(), &1.into()));
        assert!(open.get("unknownValues").is_none(), "{open}");
        // No plot axis selected: nothing is likely, and a kind lists no likely map.
        let us = counts(&indexes, Movie, "sel=country:US");
        assert_eq!((&us["total"], &us["likely"]), (&4.into(), &0.into()));
        assert!(us["kinds"]["country"].get("likely").is_none());
    }

    /// An exclusion reads the confident tier alone: it keeps the titles confidently carrying another value,
    /// and drops a title whose axis is only tentative, whichever way it leans.
    #[test]
    fn an_exclusion_on_a_plot_axis_reads_the_confident_tier_alone() {
        let indexes = tentative_fixture("likely-exclude");
        let not_happy = counts(&indexes, Movie, "sel=-ending:happy");
        assert_eq!(
            (&not_happy["total"], &not_happy["confident"], &not_happy["likely"]),
            (&1.into(), &1.into(), &0.into()),
            "film 1 alone: 2, 3 and 6 lean elsewhere and 5 leans happy, all only tentatively"
        );
        let titles =
            Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "sel=-ending:happy")).0;
        assert_eq!(ids(&titles), vec![1]);
    }

    /// Every confident match comes first, however popular a likely one is; the likely ones follow by their
    /// probability, not their votes, each card marked.
    #[test]
    fn titles_list_confident_matches_first_then_likely_by_probability() {
        let indexes = tentative_fixture("likely-titles");
        let context = Context::new(&indexes, Movie, None);
        let titles = context.titles(&request(Route::Titles, "sel=ending:bittersweet")).0;
        assert_eq!(ids(&titles), vec![1, 2, 3], "3 has the most votes, and the lowest probability");
        let likely: Vec<bool> =
            titles["titles"].as_array().unwrap().iter().map(|t| t.get("likely").is_some()).collect();
        assert_eq!(likely, vec![false, true, true]);
        assert_eq!(
            (&titles["total"], &titles["confident"], &titles["likely"]),
            (&3.into(), &1.into(), &2.into())
        );
        // A page past the confident ones starts inside the likely tier, in the same order.
        let second = context.titles(&request(Route::Titles, "sel=ending:bittersweet&skip=1&limit=1")).0;
        assert_eq!(ids(&second), vec![2]);
        // The merged row ranks its likely titles the same way.
        let unhappy = context.titles(&request(Route::Titles, "sel=ending:unhappy")).0;
        assert_eq!(ids(&unhappy), vec![1, 2, 3]);
        // No plot axis selected: the route's own order, untouched.
        assert_eq!(ids(&context.titles(&request(Route::Titles, "sel=country:US")).0), vec![3, 4, 5, 6]);
    }

    /// `values/<kind>.json` counts both tiers too, with each entry's likely share.
    #[test]
    fn values_count_likely_titles_and_say_so() {
        let indexes = tentative_fixture("likely-values");
        let spec = spec("ending").unwrap();
        let answer = Context::new(&indexes, Movie, None)
            .values(spec, &request(Route::Values(spec), "sel=country:FR"))
            .0;
        let bittersweet = answer["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "bittersweet")
            .expect("listed")
            .clone();
        assert_eq!((&bittersweet["count"], &bittersweet["likely"]), (&2.into(), &1.into()));
    }

    /// A browse row lists its confident titles first, then its likely ones by probability, and counts them.
    #[test]
    fn a_row_lists_likely_titles_after_confident_ones() {
        let indexes = tentative_fixture("likely-row");
        let row = |constraints: &[(&str, &str)]| {
            let owned: Vec<(String, String)> =
                constraints.iter().map(|(a, v)| ((*a).to_owned(), (*v).to_owned())).collect();
            crate::plotrows::row(&indexes, None, Movie, &owned, None, 0, 20)
        };
        let bittersweet = row(&[("ending", "bittersweet")]);
        assert_eq!(ids(&bittersweet), vec![1, 2, 3]);
        assert_eq!((&bittersweet["total"], &bittersweet["likely"]), (&3.into(), &2.into()));
        assert_eq!(bittersweet["titles"][1]["likely"], true);
        assert!(bittersweet["titles"][0].get("likely").is_none());
        let stacked = row(&[("ending", "happy"), ("tone", "bleak")]);
        assert_eq!(ids(&stacked), vec![5]);
        assert_eq!(stacked["likely"], 1);
    }

    /// A store without the tier answers exactly as before it: no split fields, no likely maps, no marks.
    #[test]
    fn a_store_without_the_tier_answers_no_split() {
        let indexes = fixture("no-tier");
        let answer = counts(&indexes, Movie, "sel=ending:bittersweet");
        for field in ["confident", "likely"] {
            assert!(answer.get(field).is_none(), "{field}: {answer}");
        }
        assert!(!answer.to_string().contains("\"likely\""), "{answer}");
        let titles =
            Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "sel=ending:bittersweet")).0;
        assert!(!titles.to_string().contains("likely"), "{titles}");
        let row = crate::plotrows::row(
            &indexes,
            None,
            Movie,
            &[("ending".to_owned(), "bittersweet".to_owned())],
            None,
            0,
            20,
        );
        assert!(row.get("likely").is_none(), "{row}");
    }

    /// Before any TMDB numbers are kept, `rating` is unavailable — said so, and a selection naming it is
    /// answered around it — and when a build lands the rating kind and the vote order are rebuilt from it.
    #[tokio::test]
    async fn the_rating_kinds_follow_the_ratings_build() {
        let ds = dataset("rating-swap");
        let ratings = Arc::new(crate::ratings::Ratings::default());
        let queries = IndexQueries::new(&ds).with_ratings(Some(Arc::clone(&ratings)));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let before = counts(&indexes, Movie, "sel=rating:8");
        assert_eq!(before["kindsUnavailable"], json!(["rating"]));
        assert_eq!((&before["ignored"], &before["total"]), (&json!(["rating"]), &3.into()));
        let order = Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "")).0;
        assert_eq!(ids(&order), vec![2, 1, 3], "the store's own votes");

        let mapped = crate::store::MappedStore::open(&ds.store).unwrap();
        ratings.set(Some(
            crate::ratings::build(&mapped.view(), &HashMap::from([((0, 1), (8.4, 9000))])).unwrap(),
        ));
        let after = counts(&indexes, Movie, "sel=rating:8");
        assert!(after.get("kindsUnavailable").is_none(), "{after}");
        assert_eq!(after["total"], 1);
        let reordered = Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "")).0;
        assert_eq!(ids(&reordered), vec![1, 2, 3], "the provider's 9,000 votes");
        assert_ne!(reordered["order"], order["order"], "a new order says so");
    }

    /// Characters are search-only: never listed, found by a normalised prefix of three characters or more,
    /// at most five, and selectable once found.
    #[tokio::test]
    async fn characters_are_found_by_prefix_and_selectable() {
        let ds = dataset("characters");
        let role =
            |order, person, character: &str| crate::tmdb::Role { order, person, character: character.into() };
        let credits = |roles| crate::tmdb::Credits { fetched: 0, roles };
        let kept = HashMap::from([
            ((0, 1), credits(vec![role(0, 100, "Walter White"), role(1, 200, "Jesse Pinkman")])),
            ((0, 2), credits(vec![role(0, 100, "Walter White (voice)")])),
        ]);
        let mapped = crate::store::MappedStore::open(&ds.store).unwrap();
        let list = crate::characters::build(&mapped.view(), &kept).unwrap();
        let characters = Arc::new(crate::characters::Characters::with_index(list));
        let queries = IndexQueries::new(&ds).with_characters(Some(characters));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let spec = spec("character").unwrap();
        let context = Context::new(&indexes, Movie, None);
        let found = context.values(spec, &request(Route::Values(spec), "q=Walt")).0;
        assert_eq!(found["values"], json!([{ "id": "walter-white", "name": "walter white", "count": 2 }]));
        let none = context.values(spec, &request(Route::Values(spec), "q=jes")).0;
        assert_eq!(none["values"], json!([]), "a name played in one title is not one to filter by");
        assert!(Request::parse(Route::Values(spec), Scope::Type(Movie), "q=wa").is_err());
        assert!(Request::parse(Route::Values(spec), Scope::Type(Movie), "").is_err());

        let all = context.counts(&request(Route::Counts, "")).0;
        assert!(all["kinds"].get("character").is_none(), "never listed");
        let walter = context.counts(&request(Route::Counts, "sel=character:walter-white")).0;
        assert_eq!(walter["total"], 2);
        assert_eq!(walter["kinds"]["character"]["labels"], json!({ "walter-white": "walter white" }));
        let titles = context.titles(&request(Route::Titles, "sel=character:walter-white")).0;
        assert_eq!(ids(&titles), vec![2, 1]);
    }

    /// More like a title, as a kind: its similar set, filtered with everything else, in its own order.
    #[test]
    fn like_is_the_similar_set_in_its_order() {
        let indexes = fixture("like");
        let context = Context::new(&indexes, Movie, None);
        let similar: Vec<u64> = indexes.more_like_this(1, Movie).iter().map(|&id| u64::from(id)).collect();
        let like = context.titles(&request(Route::Titles, "sel=like:1")).0;
        assert_eq!(ids(&like), similar);
        let counted = context.counts(&request(Route::Counts, "sel=like:1")).0;
        assert_eq!(counted["total"], similar.len());
        assert_eq!(counted["kinds"]["like"]["mode"], "single");
    }

    fn typed(answer: &Value) -> Vec<(String, u64)> {
        let titles = answer["titles"].as_array().unwrap();
        titles.iter().map(|t| (t["type"].as_str().unwrap().to_owned(), t["id"].as_u64().unwrap())).collect()
    }

    fn pair(media: &str, id: u64) -> (String, u64) {
        (media.to_owned(), id)
    }

    /// `all` merges the types' own orders by rank within type: films 2, 1, 3 sit at 0, 1/3 and 2/3, series 4
    /// (the only one) at 0, after the film it ties with. Pages of it are slices of that one order, and a
    /// selection merges the filtered lists by the same keys.
    #[test]
    fn all_merges_the_types_by_rank_within_type() {
        let indexes = fixture("all-order");
        let context = Context::new(&indexes, Scope::All, None);
        let whole = context.titles(&request_all(Route::Titles, "")).0;
        let order = vec![pair("movie", 2), pair("series", 4), pair("movie", 1), pair("movie", 3)];
        assert_eq!(typed(&whole), order);
        assert_eq!(whole["total"], 4);
        let movies = Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "")).0;
        assert_eq!(ids(&movies), vec![2, 1, 3], "the films' own order, which all interleaves");
        assert_ne!(whole["order"], movies["order"]);
        assert_eq!(whole["order"].as_str().map(str::len), Some(16));

        let mut paged = Vec::new();
        for skip in [0, 1, 2, 3, 4] {
            let page = context.titles(&request_all(Route::Titles, &format!("skip={skip}&limit=1"))).0;
            assert_eq!(page["order"], whole["order"], "every page is a slice of the same order");
            paged.extend(typed(&page));
        }
        assert_eq!(paged, order, "paging one at a time walks the whole order once");
        let second = context.titles(&request_all(Route::Titles, "skip=2&limit=2")).0;
        assert_eq!(typed(&second), order[2..]);
        let korean = context.titles(&request_all(Route::Titles, "sel=country:KR")).0;
        assert_eq!(typed(&korean), vec![pair("movie", 2), pair("series", 4), pair("movie", 1)]);
    }

    /// `all` counts over both types' rows: the union, each title once, and `values` alike.
    #[test]
    fn all_counts_the_union_of_both_types() {
        let indexes = fixture("all-counts");
        let all = counts(&indexes, Scope::All, "");
        assert_eq!(all["total"], 4, "three films and the one series with a card");
        assert_eq!(all["kinds"]["country"]["values"], json!({ "DK": 1, "ES": 1, "KR": 3 }));
        assert_eq!(all["kinds"]["subgenre"]["values"]["Heist"], 4);
        let korean = counts(&indexes, Scope::All, "sel=country:KR");
        assert_eq!(korean["total"], 3);
        assert_eq!(korean["coverage"]["country"]["denominator"], 4, "out of both types");
        let (movie, series) =
            (counts(&indexes, Movie, "sel=country:KR"), counts(&indexes, Tv, "sel=country:KR"));
        assert_eq!(movie["total"].as_u64().unwrap() + series["total"].as_u64().unwrap(), 3);

        let spec = spec("decade").unwrap();
        let context = Context::new(&indexes, Scope::All, None);
        let decades = context.values(spec, &request_all(Route::Values(spec), "sel=country:KR")).0;
        let ids: Vec<&str> =
            decades["values"].as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 3, "the films' 1980 and 1990 and the series' 2010: {decades}");
        assert!(ids.contains(&"2010"));
    }

    /// The indexes of a store holding these titles.
    fn store_of(
        name: &str,
        titles: &[crate::store::fixture::Title],
        entities: &[crate::store::fixture::Entity],
    ) -> Indexes {
        let dir = std::env::temp_dir().join(format!("den-atlas-filter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, titles, entities);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the store loads");
        crate::queries::load_for_tools(&ds).expect("its indexes load")
    }

    /// Series run at their share, spread evenly — here 1 in 6 — even when every series outvotes every film,
    /// where raw votes would put all five first.
    #[test]
    fn all_spreads_series_at_their_share() {
        let title = |media, tmdb_id, votes| crate::store::fixture::Title {
            media,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(2000))),
            votes,
            ..crate::store::fixture::Title::default()
        };
        let mut titles: Vec<_> = (1..=25).map(|id| title(0, id, 1000 - id)).collect();
        titles.extend((101..=105).map(|id| title(1, id, 5000 - id)));
        let indexes = store_of("all-spread", &titles, &[]);
        let context = Context::new(&indexes, Scope::All, None);
        let first = typed(&context.titles(&request_all(Route::Titles, "limit=30")).0);
        let positions: Vec<usize> =
            first.iter().enumerate().filter(|(_, (m, _))| m == "series").map(|(i, _)| i + 1).collect();
        assert_eq!(positions, vec![2, 8, 14, 20, 26], "{first:?}");
        let series: Vec<u64> = first.iter().filter(|(m, _)| m == "series").map(|&(_, id)| id).collect();
        assert_eq!(series, vec![101, 102, 103, 104, 105], "each type keeps its own order");
        assert_eq!(first[0], pair("movie", 1), "the most popular film, then the most popular series");
    }

    /// The tiers every values search orders by, the person traits' included.
    #[test]
    fn a_name_matches_exactly_then_by_whole_words_then_by_a_word_start() {
        assert_eq!(match_tier("paris", "paris"), Some(0));
        assert_eq!(match_tier("paris of the west", "paris"), Some(1));
        assert_eq!(match_tier("walt disney pictures", "disney"), Some(1));
        assert_eq!(match_tier("new york city", "new york"), Some(1));
        assert_eq!(match_tier("disneynature", "disney"), Some(2));
        assert_eq!(match_tier("disneytoon disney", "disney"), Some(1), "the best of its words");
        assert_eq!(match_tier("san francisco", "paris"), None);
    }

    /// A values search puts the value it names first, then those holding it as whole words, then those with
    /// a word merely starting with it, each by titles. San Francisco, which Wikidata also calls "Paris of
    /// the West", used to lead `q=paris` wherever it had more titles than Paris.
    #[test]
    fn a_values_search_leads_with_the_value_it_names() {
        use crate::store::fixture::{Entity, Title};
        let title = |tmdb_id, locations: Vec<u32>, cast: Vec<u32>| Title {
            media: 0,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(2000))),
            votes: 100,
            locations,
            cast,
            ..Title::default()
        };
        // Paris is set in 5 titles, San Francisco in 8, Parisot in 12; Nolan North is in 3, Christopher
        // Nolan in 2, Nolanne in 4.
        let titles: Vec<Title> = (1..=12)
            .map(|id| {
                let mut places = vec![3];
                places.extend((id <= 5).then_some(1));
                places.extend((id <= 8).then_some(2));
                let cast = [(id <= 3, 10), (id <= 2, 11), (id <= 4, 12)];
                title(id, places, cast.into_iter().filter(|c| c.0).map(|c| c.1).collect())
            })
            .collect();
        let entities = [
            Entity { qid: 1, name: "Paris", ..Entity::default() },
            Entity { qid: 2, name: "San Francisco", aliases: vec!["Paris of the West"], ..Entity::default() },
            Entity { qid: 3, name: "Parisot", ..Entity::default() },
            Entity { qid: 10, name: "Nolan North", ..Entity::default() },
            Entity { qid: 11, name: "Christopher Nolan", ..Entity::default() },
            Entity { qid: 12, name: "Nolanne Smith", ..Entity::default() },
        ];
        let indexes = store_of("values-order", &titles, &entities);
        let context = Context::new(&indexes, Movie, None);
        let ids = |kind: &str, q: &str| {
            let spec = spec(kind).unwrap();
            let answer = context.values(spec, &request(Route::Values(spec), &format!("q={q}"))).0;
            let ids: Vec<String> = answer["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["id"].as_str().unwrap().into())
                .collect();
            ids
        };
        assert_eq!(ids("place", "paris"), ["Q1", "Q2", "Q3"], "the name, the alias's whole word, a prefix");
        assert_eq!(ids("place", "pari"), ["Q3", "Q2", "Q1"], "no whole word: by titles");
        assert_eq!(ids("person", "nolan"), ["Q10", "Q11", "Q12"], "a surname is a whole word");
        assert_eq!(ids("person", "christopher nolan"), ["Q11"]);
        let limited = spec("place").unwrap();
        let first = context.values(limited, &request(Route::Values(limited), "q=paris&limit=1")).0;
        assert_eq!(first["values"][0]["id"], "Q1", "the page is cut after ordering: {first}");
    }

    /// Films 1 (action) and 4, series 2 (Action & Adventure, and kids) and 3 (kids), both on network Q70.
    fn mixed_genres(name: &str) -> Indexes {
        let title =
            |media, tmdb_id, votes, genres: Vec<u32>, broadcasters: Vec<u32>| crate::store::fixture::Title {
                media,
                tmdb_id,
                primary_genre: "Drama",
                plot: vec![100, 0, 0],
                premise: vec![100, 0, 0],
                card: Some(("A title", None, Some(2000))),
                votes,
                genres,
                broadcasters,
                ..crate::store::fixture::Title::default()
            };
        let titles = [
            title(0, 1, 100, vec![28], vec![]),
            title(1, 2, 300, vec![10759, 10762], vec![70]),
            title(1, 3, 50, vec![10762], vec![70]),
            title(0, 4, 200, vec![18], vec![]),
        ];
        let network = crate::store::fixture::Entity { qid: 70, name: "A Network", ..Default::default() };
        store_of(name, &titles, &[network])
    }

    /// A series' genres are read as films', so under `all` a film genre id matches the films and the series
    /// filed under it; a series-only genre, and a composite, only their series.
    #[test]
    fn all_folds_series_genres_into_film_ones() {
        let indexes = mixed_genres("all-genres");
        let genres = &counts(&indexes, Scope::All, "")["kinds"]["genre"]["values"];
        assert_eq!(genres["28"], 2, "action film 1 and Action & Adventure series 2: {genres}");
        assert_eq!(genres["10759"], 1, "the composite: the series alone");
        assert_eq!(genres["10762"], 2, "kids: series 2 and 3");
        let context = Context::new(&indexes, Scope::All, None);
        let action = context.titles(&request_all(Route::Titles, "sel=genre:28")).0;
        assert_eq!(typed(&action), vec![pair("series", 2), pair("movie", 1)]);
        let kids = context.titles(&request_all(Route::Titles, "sel=genre:10762")).0;
        assert_eq!(typed(&kids), vec![pair("series", 2), pair("series", 3)]);
        assert_eq!(counts(&indexes, Movie, "sel=genre:28")["total"], 1);
        assert_eq!(counts(&indexes, Movie, "sel=genre:10762")["total"], 0, "no film is a kids' series");
    }

    /// A kind only one type has keeps working under `all`, matching that type's titles alone.
    #[test]
    fn a_series_only_kind_answers_under_all_with_its_series() {
        let indexes = mixed_genres("all-network");
        let all = counts(&indexes, Scope::All, "");
        assert_eq!(all["kinds"]["network"]["values"], json!({ "Q70": 2 }));
        assert_eq!(all["kinds"]["network"]["labels"], json!({ "Q70": "A Network" }));
        let context = Context::new(&indexes, Scope::All, None);
        let aired = context.titles(&request_all(Route::Titles, "sel=network:Q70")).0;
        assert_eq!(typed(&aired), vec![pair("series", 2), pair("series", 3)]);
        assert_eq!(aired["ignored"], json!([]));
        let action = counts(&indexes, Scope::All, "sel=genre:28,network:Q70");
        assert_eq!(action["total"], 1, "series 2 only");
        let off = context.titles(&request_all(Route::Titles, "sel=-network:Q70")).0;
        assert_eq!(off["total"], 0, "no title is known to be on another network");
        let films = counts(&indexes, Movie, "sel=network:Q70");
        assert_eq!((&films["total"], &films["ignored"]), (&2.into(), &json!(["network"])));
    }

    /// Under `all` a `like` names its title's type, and answers the row `/index/similar` serves as `mixed`,
    /// in its order; a bare id is refused, since both types use the same numbers.
    #[test]
    fn like_under_all_is_the_typed_mixed_row() {
        let indexes = fixture("all-like");
        let context = Context::new(&indexes, Scope::All, None);
        let mixed: Vec<(String, u64)> = indexes
            .more_like_this_mixed(1, Movie)
            .iter()
            .filter(|&&(_, id)| id <= 4)
            .map(|&(m, id)| pair(if m == Tv { "series" } else { "movie" }, u64::from(id)))
            .collect();
        assert!(mixed.iter().any(|(m, _)| m == "series"), "the fixture's row mixes: {mixed:?}");
        let like = context.titles(&request_all(Route::Titles, "sel=like:movie-1")).0;
        assert_eq!(typed(&like), mixed);
        assert_eq!(like["order"], "like:movie-1");
        assert_eq!(context.counts(&request_all(Route::Counts, "sel=like:movie-1")).0["total"], mixed.len());
        assert!(Request::parse(Route::Titles, Scope::All, "sel=like:1").is_err());
        assert!(Request::parse(Route::Titles, Scope::All, "sel=like:anime-1").is_err());
        assert!(Request::parse(Route::Titles, Scope::Type(Movie), "sel=like:movie-1").is_err());
        let nobody = context.counts(&request_all(Route::Counts, "sel=like:series-1")).0;
        assert_eq!(nobody["unknownValues"], json!(["like:series-1"]));
    }

    /// A region is the union of its countries: a title with two members counts once. One pick at a time, its
    /// values counted without it, and it narrows with a country like any other kind.
    #[test]
    fn a_region_is_the_union_of_its_countries() {
        let dir = std::env::temp_dir().join(format!("den-atlas-filter-regions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let title = |tmdb_id, countries: Vec<&'static str>| crate::store::fixture::Title {
            media: 0,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(2000))),
            countries,
            ..crate::store::fixture::Title::default()
        };
        let titles = [
            title(1, vec!["SE"]),
            title(2, vec!["NO", "SE"]),
            title(3, vec!["DK"]),
            title(4, vec!["FI"]),
            title(5, vec!["US"]),
        ];
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &titles, &[]);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the region store loads");
        let indexes = crate::queries::load_for_tools(&ds).expect("its indexes load");

        let all = counts(&indexes, Movie, "");
        let countries = &all["kinds"]["country"]["values"];
        assert_eq!(countries, &json!({ "DK": 1, "FI": 1, "NO": 1, "SE": 2, "US": 1 }));
        let sum: u64 = ["SE", "NO", "DK", "FI", "IS"].iter().filter_map(|c| countries[c].as_u64()).sum();
        let region = &all["kinds"]["region"];
        assert_eq!(region["mode"], "single");
        assert_eq!(region["values"]["nordic"], sum - 1, "title 2 is Swedish and Norwegian, counted once");
        assert_eq!(
            region["values"],
            json!({ "nordic": 4, "north-american": 1, "scandinavian": 3 }),
            "a region no title carries is not offered"
        );
        assert_eq!(region["labels"]["north-american"], "North American");

        let nordic = counts(&indexes, Movie, "sel=region:nordic");
        assert_eq!(nordic["total"], 4);
        let region = &nordic["kinds"]["region"];
        assert_eq!(region["values"], json!({ "nordic": 4, "north-american": 1, "scandinavian": 3 }));
        assert_eq!(region["selected"], json!(["nordic"]));
        assert_eq!(nordic["coverage"]["region"], json!({ "count": 5, "denominator": 5 }));
        let swedish = counts(&indexes, Movie, "sel=country:SE,region:scandinavian");
        assert_eq!(swedish["total"], 2);
        assert_eq!(swedish["kinds"]["region"]["values"], json!({ "nordic": 2, "scandinavian": 2 }));
        let context = Context::new(&indexes, Movie, None);
        let titles = context.titles(&request(Route::Titles, "sel=-region:scandinavian")).0;
        let mut excluded = ids(&titles);
        excluded.sort_unstable();
        assert_eq!(excluded, vec![4, 5]);

        let empty = counts(&indexes, Movie, "sel=region:african");
        assert_eq!(empty["total"], 0);
        assert!(empty.get("unknownValues").is_none(), "a real region, just none here: {empty}");
        let typo = counts(&indexes, Movie, "sel=region:nordik");
        assert_eq!(typo["unknownValues"], json!(["region:nordik"]));

        let spec = spec("region").unwrap();
        let scandi = context.values(spec, &request(Route::Values(spec), "q=scandi")).0;
        assert_eq!(scandi["values"], json!([{ "id": "scandinavian", "name": "Scandinavian", "count": 3 }]));
        let schema = schema();
        let nordic = schema["regions"].as_array().unwrap().iter().find(|r| r["slug"] == "nordic").unwrap();
        assert_eq!(nordic["countries"], json!(["SE", "NO", "DK", "FI", "IS"]));
        assert_eq!(nordic["label"], "Nordic");
    }

    /// Five films: 1 French (1990s), 2 Italian (1990s), 3 French and Italian (1980s), 4 American (1990s), 5 with
    /// no country on record (1980s).
    fn countries_store(name: &str) -> Indexes {
        let dir = std::env::temp_dir().join(format!("den-atlas-filter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let title = |tmdb_id, year, countries: Vec<&'static str>| crate::store::fixture::Title {
            media: 0,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(year))),
            votes: 100 - tmdb_id,
            countries,
            ..crate::store::fixture::Title::default()
        };
        let titles = [
            title(1, 1990, vec!["FR"]),
            title(2, 1995, vec!["IT"]),
            title(3, 1985, vec!["FR", "IT"]),
            title(4, 1999, vec!["US"]),
            title(5, 1980, vec![]),
        ];
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &titles, &[]);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the countries store loads");
        crate::queries::load_for_tools(&ds).expect("its indexes load")
    }

    fn sorted_ids(indexes: &Indexes, query: &str) -> Vec<u64> {
        let mut found = ids(&Context::new(indexes, Movie, None).titles(&request(Route::Titles, query)).0);
        found.sort_unstable();
        found
    }

    /// `|` OR-s values of one kind; separate items still AND, within a kind and across kinds.
    #[test]
    fn a_group_is_the_union_and_separate_items_the_intersection() {
        let indexes = countries_store("or-union");
        assert_eq!(sorted_ids(&indexes, "sel=country:FR|IT"), vec![1, 2, 3], "French or Italian");
        assert_eq!(sorted_ids(&indexes, "sel=country:FR,country:IT"), vec![3], "French and Italian");
        assert_eq!(sorted_ids(&indexes, "sel=country:FR|IT,decade:1990"), vec![1, 2], "either, in the 1990s");
        assert_eq!(
            sorted_ids(&indexes, "sel=decade:1980|1990"),
            vec![1, 2, 3, 4, 5],
            "a one-pick kind OR-ed"
        );
        assert_eq!(counts(&indexes, Movie, "sel=country:FR|IT")["total"], 3);
        // Excluded, a group is none of its values, and a title with no country on record stays out.
        assert_eq!(
            sorted_ids(&indexes, "sel=-country:FR|IT"),
            vec![4],
            "neither; 5 is not known to be neither"
        );
        assert_eq!(sorted_ids(&indexes, "sel=-country:FR"), vec![2, 4]);
    }

    /// A group with a value its kind does not hold keeps the others and names that one, as a lone unknown
    /// value is named; a group of nothing but unknown values matches nothing.
    #[test]
    fn an_unknown_value_in_a_group_is_named_and_the_rest_apply() {
        let indexes = countries_store("or-unknown");
        let partly = counts(&indexes, Movie, "sel=country:FR|XX");
        assert_eq!(partly["total"], 2, "French alone: 1 and 3");
        assert_eq!(partly["unknownValues"], json!(["country:XX"]));
        let excluded = counts(&indexes, Movie, "sel=-country:IT|XX");
        assert_eq!((&excluded["total"], &excluded["unknownValues"]), (&2.into(), &json!(["-country:XX"])));
        let none = counts(&indexes, Movie, "sel=country:XX|YY");
        assert_eq!(none["total"], 0);
        assert_eq!(none["unknownValues"], json!(["country:XX", "country:YY"]));
        assert_eq!(none["kinds"]["country"]["selected"], json!(["XX", "YY"]));
    }

    /// counts.json counts a kind with an OR group selected without the group — what each value would add — and
    /// every other kind under it. A lone value of an and kind still narrows its own kind's counts.
    #[test]
    fn a_kind_with_a_group_counts_its_values_without_the_group() {
        let indexes = countries_store("or-counts");
        let either = counts(&indexes, Movie, "sel=country:FR|IT,decade:1990");
        assert_eq!(either["total"], 2);
        let country = &either["kinds"]["country"];
        assert_eq!(country["values"], json!({ "FR": 1, "IT": 1, "US": 1 }), "the 1990s films, group aside");
        assert_eq!(country["denominator"], 3);
        assert_eq!(country["selected"], json!(["FR", "IT"]));
        // The one-pick decade, counted without its own pick but under the group: 3 is from the 1980s.
        assert_eq!(either["kinds"]["decade"]["values"], json!({ "1980": 1, "1990": 2 }));
        // The AND form counts country under the whole selection.
        let both = counts(&indexes, Movie, "sel=country:FR,country:IT");
        assert_eq!(both["kinds"]["country"]["values"], json!({ "FR": 1, "IT": 1 }));
        // A lone value beside a group: the kind is counted under the lone value, not the group.
        let french = counts(&indexes, Movie, "sel=country:FR,country:IT|US");
        assert_eq!(french["total"], 1, "3 is French and Italian");
        assert_eq!(french["kinds"]["country"]["values"], json!({ "FR": 2, "IT": 1, "US": 0 }));
        assert_eq!(french["kinds"]["country"]["selected"], json!(["FR", "IT", "US"]));
        // values/<kind>.json counts the same way.
        let spec = spec("country").unwrap();
        let values = Context::new(&indexes, Movie, None)
            .values(spec, &request(Route::Values(spec), "sel=country:FR|IT,decade:1990"))
            .0;
        assert_eq!(values["denominator"], 3);
        assert_eq!(values["values"].as_array().unwrap().len(), 3, "{values}");
    }

    /// A group on a plot axis reads both tiers as a lone value does: a title needing a likely value is a likely
    /// match, and an excluded group reads the confident tier alone.
    #[test]
    fn a_plot_axis_group_keeps_the_tiers() {
        let indexes = tentative_fixture("or-likely");
        let either = counts(&indexes, Movie, "sel=ending:bittersweet|happy");
        assert_eq!(
            (&either["total"], &either["confident"], &either["likely"]),
            (&5.into(), &2.into(), &3.into()),
            "1 and 4 confidently; 2, 3 and 5 only tentatively"
        );
        let titles = Context::new(&indexes, Movie, None)
            .titles(&request(Route::Titles, "sel=ending:bittersweet|happy"))
            .0;
        assert_eq!(ids(&titles), vec![1, 4, 2, 5, 3], "confident by votes, then likely by probability");
        let neither = counts(&indexes, Movie, "sel=-ending:happy|open");
        assert_eq!(
            (&neither["total"], &neither["likely"]),
            (&1.into(), &0.into()),
            "film 1 alone: the rest are happy or only tentative"
        );
    }

    /// A group counts toward the cap by its values; `like` and a group across axes are refused.
    #[test]
    fn a_group_is_capped_by_its_values_and_refused_where_it_cannot_apply() {
        let sixteen = (1..=16).map(|g| g.to_string()).collect::<Vec<_>>();
        let (first, rest) = sixteen.split_at(9);
        let query = format!("sel=genre:{},genre:{}", first.join("|"), rest.join("|"));
        assert!(Request::parse(Route::Counts, Scope::Type(Movie), &query).is_ok(), "{query}");
        let seventeen = format!("{query},genre:17");
        assert!(Request::parse(Route::Counts, Scope::Type(Movie), &seventeen).is_err());
        let one_group = format!("sel=genre:{}|17", sixteen.join("|"));
        assert!(Request::parse(Route::Counts, Scope::Type(Movie), &one_group).is_err());
        for refused in
            ["sel=like:550|680", "sel=structure:single-day|nonlinear", "sel=country:FR|", "sel=country:|"]
        {
            assert!(Request::parse(Route::Titles, Scope::Type(Movie), refused).is_err(), "{refused}");
        }
    }

    /// Den Web tests against the same file (tests/fixtures/facets-canonical.json): every url answers as its
    /// canonical one, a canonical url is canonical, and the refused ones are refused.
    #[test]
    fn the_canonical_fixture_holds() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/facets-canonical.json")).unwrap();
        let split = |url: &str| -> (Route, Scope, String, String) {
            let (path, query) = url.split_once('?').unwrap_or((url, ""));
            let parts: Vec<&str> = path.trim_start_matches("/index/filter/").split('/').collect();
            let scope = Scope::parse(parts[0]).unwrap_or_else(|| panic!("{url}"));
            let route = match parts[1..] {
                ["counts.json"] => Route::Counts,
                ["titles.json"] => Route::Titles,
                ["people.json"] => Route::People,
                ["people", "counts.json"] => Route::PeopleCounts,
                ["people", "values", kind] => {
                    Route::PeopleValues(people_values_kind(kind.trim_end_matches(".json")).unwrap())
                }
                ["values", kind] => Route::Values(spec(kind.trim_end_matches(".json")).unwrap()),
                _ => panic!("{url}"),
            };
            (route, scope, path.to_owned(), query.to_owned())
        };
        for case in fixture["cases"].as_array().unwrap() {
            let (url, canonical) = (case["url"].as_str().unwrap(), case["canonical"].as_str().unwrap());
            let (route, scope, path, query) = split(url);
            let parsed = Request::parse(route, scope, &query).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert_eq!(format!("{path}{}", parsed.query()), canonical, "{url}");
            assert_eq!(parsed.canonical, url == canonical, "{url}");
            let (route, scope, _, query) = split(canonical);
            assert!(
                Request::parse(route, scope, &query).unwrap().canonical,
                "{canonical} is its own canonical form"
            );
        }
        for url in fixture["refused"].as_array().unwrap() {
            let url = url.as_str().unwrap();
            let (route, scope, _, query) = split(url);
            assert!(Request::parse(route, scope, &query).is_err(), "{url} should be refused");
        }
    }

    /// The filters over the REAL corpus, and what they cost. Opt-in: `DEN_STORE` names a store whose directory
    /// holds its `dataset.meta.json`; `CACHE_DIR`, a directory of kept TMDB numbers and credits
    /// (`tmdb-votes.tsv`, `tmdb-credits.tsv`), adds the rating and character kinds and the vote order.
    #[test]
    fn real_corpus_filters_and_timing() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to measure this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let tmdb = std::env::var("CACHE_DIR").ok().map(|kept| {
            let tmdb = crate::tmdb::Tmdb::new(ds.mapped.clone(), Some(kept.into()), None, 0).unwrap();
            eprintln!("{}", runtime.block_on(tmdb.load()));
            tmdb
        });
        let (ratings, characters) =
            (tmdb.as_ref().map(|t| t.ratings()), tmdb.as_ref().map(|t| t.characters()));
        let (indexes, _) = runtime
            .block_on(IndexQueries::new(&ds).with_ratings(ratings).with_characters(characters).get(|| ()))
            .expect("the indexes load");
        let started = std::time::Instant::now();
        let filter = FilterIndex::build(&indexes);
        eprintln!(
            "built in {:?}: {} bitset values, {:.1} MB resident; unavailable {:?}",
            started.elapsed(),
            filter.value_count(),
            filter.bytes() as f64 / 1e6,
            filter.unavailable
        );
        let time = |label: &str, work: &dyn Fn() -> String| {
            let first = std::time::Instant::now();
            let body = work();
            let cold = first.elapsed();
            let rounds = 20;
            let warm = std::time::Instant::now();
            for _ in 0..rounds {
                work();
            }
            eprintln!(
                "{label}: first {cold:?}, then {:?} each, {} bytes",
                warm.elapsed() / rounds,
                body.len()
            );
            body
        };
        let context = Context::new(&indexes, Movie, None);
        let derived = std::time::Instant::now();
        drop(Context::new(&indexes, Movie, None));
        eprintln!("derived (rating kind, vote order) already built: {:?}", derived.elapsed());
        let rebuilt = std::time::Instant::now();
        let fresh = filter.derive(&indexes, indexes.ratings.as_ref().and_then(|r| r.index()), None);
        eprintln!(
            "derive from scratch (rating kind, three orders): {:?}; orders {} + {} = all {}",
            rebuilt.elapsed(),
            fresh.order[0].len(),
            fresh.order[1].len(),
            fresh.order[2].len()
        );
        let both = Context::new(&indexes, Scope::All, None);
        for query in [
            "",
            "sel=genre:28",
            "sel=country:US,decade:1990,genre:28",
            "sel=genre:10762",
            "sel=like:movie-550",
        ] {
            let parsed = request_all(Route::Counts, query);
            time(&format!("all counts {query:?}"), &|| both.counts(&parsed).0.to_string());
            let parsed = request_all(
                Route::Titles,
                &format!("{query}{}limit=40", if query.is_empty() { "" } else { "&" }),
            );
            let body =
                time(&format!("all titles {query:?} limit=40"), &|| both.titles(&parsed).0.to_string());
            let answer: Value = serde_json::from_str(&body).unwrap();
            let first: Vec<String> = answer["titles"]
                .as_array()
                .unwrap()
                .iter()
                .take(12)
                .map(|t| format!("{} {}", t["type"].as_str().unwrap(), t["title"]))
                .collect();
            eprintln!("  total {}, first {first:?}", answer["total"]);
        }
        let genres = both.counts(&request_all(Route::Counts, "")).0["kinds"]["genre"]["values"].clone();
        eprintln!("  all genre counts {genres}");
        let top = both.titles(&request_all(Route::Titles, "limit=100")).0;
        let series: Vec<usize> = top["titles"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, t)| t["type"] == "series")
            .map(|(i, _)| i + 1)
            .collect();
        eprintln!("  all: series at positions {series:?} of the first 100");
        for query in [
            "",
            "sel=genre:18",
            "sel=country:US,decade:1990,genre:28",
            "sel=person:Q25191",
            "sel=genre:18,person:Q25191",
            "sel=decade:2000,subgenre:Heist,-warning:violence",
            // OR groups beside their AND forms.
            "sel=country:FR|IT",
            "sel=country:FR,country:IT",
            "sel=country:FR|IT,decade:1990",
            "sel=-country:FR|IT",
            "sel=decade:1980|1990",
            "sel=genre:18|28|35|53|80",
        ] {
            let parsed = request(Route::Counts, query);
            let body = time(&format!("counts {query:?}"), &|| context.counts(&parsed).0.to_string());
            eprintln!("  total {}", serde_json::from_str::<Value>(&body).unwrap()["total"]);
            let parsed = request(
                Route::Titles,
                &format!("{query}{}limit=40", if query.is_empty() { "" } else { "&" }),
            );
            time(&format!("titles {query:?} limit=40"), &|| context.titles(&parsed).0.to_string());
        }
        for (kind, query) in [("person", ""), ("person", "sel=-genre:99999"), ("cast", "sel=genre:18")] {
            let spec = spec(kind).unwrap();
            let parsed = request(Route::Values(spec), query);
            time(&format!("values/{kind} {query:?}"), &|| context.values(spec, &parsed).0.to_string());
        }
        for (kind, q) in [
            ("person", "nolan"),
            ("company", "ghibli"),
            ("subject", "world war"),
            ("genre", ""),
            ("format", ""),
            ("character", "jesse pink"),
            ("character", "sherlock"),
        ] {
            let spec = spec(kind).unwrap();
            if context.status(spec) != Status::Ready {
                continue;
            }
            let parsed = request(
                Route::Values(spec),
                &if q.is_empty() { String::new() } else { format!("q={}", q.replace(' ', "%20")) },
            );
            let body =
                time(&format!("values/{kind} q={q:?}"), &|| context.values(spec, &parsed).0.to_string());
            eprintln!("  {body}");
        }
        // The score tables' distributions, which their floors are chosen from.
        let view = indexes.store.view();
        let strings = view.strings().unwrap();
        for (kind, table, names, floor, _) in DENSE_KINDS {
            let (Ok(cells), Ok(names)) = (view.column::<u8>(table), view.column::<u32>(names)) else {
                continue;
            };
            let width = names.len();
            for (a, &name) in names.iter().enumerate() {
                let column: Vec<u8> = cells.iter().skip(a).step_by(width).copied().collect();
                let at = |t: u8| column.iter().filter(|&&s| s >= t).count();
                eprintln!(
                    "  {kind} {:<28} >0 {:>6}  >=0.2 {:>6}  >=0.3 {:>6}  >=0.4 {:>6}  >=floor {:>6}  max {}",
                    strings.get(name).unwrap_or("?"),
                    at(1),
                    at(20),
                    at(30),
                    at(40),
                    at(floor),
                    column.iter().max().unwrap_or(&0)
                );
            }
        }
        let names = filter.names(&indexes);
        eprintln!(
            "name index: {} names, {:.1} MB",
            names.names.len(),
            (names.text.len() + names.names.len() * 12) as f64 / 1e6
        );
        let all = context.counts(&request(Route::Counts, "")).0;
        for (kind, answer) in all["kinds"].as_object().unwrap() {
            let values = answer["values"].as_object().unwrap();
            let top: Vec<String> = values
                .iter()
                .take(12)
                .map(|(id, n)| {
                    let label = answer["labels"][id].as_str().map(|l| format!(" {l}")).unwrap_or_default();
                    format!("{id}{label}={n}")
                })
                .collect();
            eprintln!(
                "  {kind} ({}{}): {}",
                values.len(),
                if answer["complete"] == true { "" } else { "+" },
                top.join(", ")
            );
        }
        // Every format with twenty titles or more, to see how much of instance_of is noise.
        let format = ENTITY_KINDS.iter().position(|e| e.name == "format").unwrap();
        let mut formats = context.entity_counts(format, &filter.types[0]);
        formats.extend(context.entity_counts(format, &filter.types[1]));
        let mut merged: BTreeMap<u32, u32> = BTreeMap::new();
        for (e, n) in formats {
            *merged.entry(e).or_default() += n;
        }
        let mut formats: Vec<(u32, u32)> = merged.into_iter().filter(|&(_, n)| n >= 20).collect();
        formats.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        for (e, n) in formats {
            eprintln!("  format {} {} = {n}", context.qid(e), context.label(e).unwrap_or("?"));
        }
        assert!(filter.value_count() > 100);
    }
}
