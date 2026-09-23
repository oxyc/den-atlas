//! `/index/filter/<movie|series>/…` — stackable filters over the corpus, for Den Web's Search (oxyc/den#133,
//! oxyc/den#134): a selection of values from many kinds (genre, language, decade, a mood, a plot axis, a
//! person, a studio, a rating, …) AND-ed together, and three questions about it.
//!
//! - `counts.json` — for every value of every kind, how many titles carry the selection AND that value, so an
//!   option that would leave nothing is hidden instead of spending TMDB discover calls to find out.
//! - `titles.json` — the titles carrying the selection, paged, as the cards `/index/row` draws.
//! - `values/<kind>.json` — one kind's values under the selection, labelled, with a prefix search: the
//!   typeahead for the kinds too big to list whole (people, studios, characters).
//!
//! # What a count is
//!
//! Titles of the type, with a card, on record as carrying every selected value and the counted one. A title
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
//! `TOP_K`. The empty selection's counts are worked out once. What depends on IMDb's daily ratings (the
//! `rating` kind and the popularity order `titles.json` walks) is rebuilt when a new ratings join lands.
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

type Bits = Vec<u64>;
type Key = (MediaType, u32);

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
/// A rating counts only on this many IMDb votes.
pub const MIN_VOTES: u32 = 10;
/// The `rating` kind's values: IMDb's average at or above each.
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
/// under the selection WITHOUT this kind's own values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    And,
    Single,
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
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Data {
    /// A bitset per value, built at load.
    Bits,
    /// IMDb's rating thresholds, rebuilt with each ratings join.
    Rating,
    /// Posting lists into `ENTITY_KINDS[i]`.
    Entity(usize),
    /// IMDb's character names (`characters.rs`), rebuilt with each principals join. Search-only.
    Character,
    /// More Like This for a title of the route's type: the set `/index/similar` answers.
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
/// reported unavailable there rather than answering every exclusion with "all clean". Those were title-only
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
        spec("genre", Mode::And, Id::Integer, B, "TMDB genre id; a series also under TMDB's composites"),
        spec("language", Mode::And, Id::Lower, B, "ISO 639-1, every original language"),
        spec("country", Mode::And, Id::Upper, B, "ISO 3166-1, every country of origin"),
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
            "IMDb's average at or above 6, 7 or 8, on 10+ votes",
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
    specs.push(spec("character", Mode::And, Id::Character, Character, "a character; search-only"));
    specs.push(spec(
        "like",
        Mode::Single,
        Id::Integer,
        Like,
        "More Like This for a TMDB id of the route's type",
    ));
    specs
});

/// A kind by name.
pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

/// One `[-]<kind>:<id>` of a selection, normalised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Item {
    pub kind: String,
    /// `-kind:id`: the titles known NOT to carry the value.
    pub exclude: bool,
    pub id: String,
}

impl Item {
    fn spelled(&self) -> String {
        format!("{}{}:{}", if self.exclude { "-" } else { "" }, self.kind, encode(&self.id))
    }
}

/// A kind and an id as their canonical pair. An unknown kind is kept — lowercased, its id as sent — so a
/// client built for a newer atlas still gets an answer, with the kind reported as ignored.
fn normalise(kind: &str, id: &str) -> Result<(String, String), String> {
    let kind = kind.trim().to_ascii_lowercase();
    let id = id.trim();
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
    let number = || id.parse::<u32>().map_err(|_| format!("{kind}: {id:?} is not an integer"));
    let id = match spec.id {
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
    };
    Ok((kind, id))
}

/// Which question a request asks.
#[derive(Clone, Copy, Debug)]
pub enum Route {
    Counts,
    Titles,
    Values(&'static Spec),
}

/// A request's parameters, read and checked before anything loads.
#[derive(Debug, Default)]
pub struct Request {
    /// Sorted by kind, then positive before excluded, then id; each once.
    pub items: Vec<Item>,
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
    /// - `sel`, then (titles) `skip` and `limit`, or (values) `q` and `limit`; each only when it is not its
    ///   default (no selection, 0, the route's page) and nothing else. No query at all when everything is.
    /// - `sel`: the items `[-]<kind>:<id>` joined by `,`, each id normalised as its kind says (`Id`), sorted by
    ///   kind, then positive before excluded, then id (compared as strings), each once. `:`, `,` and `-` are
    ///   literal; an id is percent-encoded as JavaScript's `encodeURIComponent` does.
    /// - `skip` and `limit` plain decimals, `limit` within 1..=100 and `skip` a multiple of it.
    /// - `q` normalised as the kind's names are (folded and lowercased, words joined by single spaces), encoded
    ///   like an id.
    ///
    /// The error is a request that cannot be answered as sent: a malformed item, an id its kind cannot read,
    /// too many values, a query too long, a `skip` that is not a page boundary, a prefix too short.
    pub fn parse(route: Route, query: &str) -> Result<Request, String> {
        if query.len() > MAX_QUERY {
            return Err(format!("a query of at most {MAX_QUERY} bytes"));
        }
        let allowed: &[&str] = match route {
            Route::Counts => &["sel"],
            Route::Titles => &["sel", "skip", "limit"],
            Route::Values(_) => &["sel", "q", "limit"],
        };
        let mut params: HashMap<&str, &str> = HashMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if allowed.contains(&name) {
                params.entry(name).or_insert(value);
            }
        }
        let decode = |v: &str| crate::handler::percent_decode(&v.replace('+', " "));

        let sel = params.get("sel").map(|v| decode(v)).unwrap_or_default();
        let raw: Vec<&str> = sel.split(',').filter(|i| !i.is_empty()).collect();
        if raw.len() > MAX_SELECTION {
            return Err(format!("at most {MAX_SELECTION} selected values"));
        }
        let mut items = Vec::with_capacity(raw.len());
        for item in raw {
            let (exclude, item) = match item.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, item),
            };
            let (kind, id) = item.split_once(':').ok_or_else(|| format!("{item:?} is not <kind>:<id>"))?;
            let (kind, id) = normalise(kind, id)?;
            items.push(Item { kind, exclude, id });
        }
        items.sort();
        items.dedup();

        let count = |name: &str| -> Result<Option<usize>, String> {
            params
                .get(name)
                .map(|v| v.parse::<usize>().map_err(|_| format!("{name}: {v:?} is not a count")))
                .transpose()
        };
        let (default_limit, max_limit) = match route {
            Route::Counts => (0, 0),
            Route::Titles => (PAGE, MAX_PAGE),
            Route::Values(spec) => (spec.values_limit(), spec.values_limit()),
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
            _ => None,
        };

        let mut parts = Vec::new();
        if !items.is_empty() {
            parts.push(format!("sel={}", items.iter().map(Item::spelled).collect::<Vec<_>>().join(",")));
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
        Ok(Request { items, skip, limit, q, canonical, canonical_query })
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

/// What depends on IMDb's daily ratings join and on TMDB's daily export: rebuilt when either swaps.
struct Derived {
    ratings: Option<Arc<RatingsIndex>>,
    export: Option<Arc<TitleIndex>>,
    rating: Option<Valued>,
    /// Each type's rows with a card, most voted first then by TMDB id — the order `titles.json` walks.
    order: [Vec<u32>; 2],
    /// A fingerprint of each order, so a client paging while it changes can tell.
    order_id: [String; 2],
    /// Each type's counts for the empty selection, worked out once.
    empty: [OnceLock<(Value, usize)>; 2],
}

/// Every kind's values over the store's rows (`FilterIndex::build`).
pub struct FilterIndex {
    keys: Vec<Key>,
    /// Each type's rows with a card, as counts, totals and the grid all count them: [movie, series].
    types: [Bits; 2],
    bits: BTreeMap<&'static str, Valued>,
    /// Per `ENTITY_KINDS` row; `None` when its sections did not read.
    entities: Vec<Option<EntityKind>>,
    /// Kinds this store should answer and whose source did not read.
    unavailable: Vec<&'static str>,
    /// Those of `unavailable` that are a property of this dataset version — a section the store does not
    /// carry, a score table no title reaches the floor of — rather than a failure at runtime. Reported, but
    /// no reason to cache an answer briefly: it will not change until the next dataset.
    stable: Vec<&'static str>,
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

        let mut types = [zeros(), zeros()];
        for (row, key) in keys.iter().enumerate() {
            if indexes.cards.as_ref().is_none_or(|cards| cards.contains_key(key)) {
                set(&mut types[type_index(key.0)], row);
            }
        }

        let mut bits: BTreeMap<&'static str, Valued> = BTreeMap::new();
        let mut unavailable: Vec<&'static str> = Vec::new();
        let mut stable: Vec<&'static str> = Vec::new();
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
            for kind in ["genre", "language", "country", "source"] {
                open(&mut bits, kind);
            }
        } else {
            unavailable.extend(["genre", "language", "country", "source"]);
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
        } else {
            unavailable.push("runtime");
            stable.push("runtime");
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
            let Some((cells, names, strings)) = read else {
                unavailable.push(kind);
                stable.push(kind);
                continue;
            };
            // A vocabulary no title reaches the floor of can answer nothing, and an exclusion over it would
            // call every title clean.
            if !names.is_empty() && !cells.iter().any(|&score| score >= floor) {
                unavailable.push(kind);
                stable.push(kind);
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
            // The merged display rows `/index/row` answers (`ending:unhappy`), as the union of their members.
            for merged in crate::plotrows::MERGED_ROWS {
                let Some(valued) = bits.get_mut(merged.axis) else { continue };
                let mut union = zeros();
                for member in merged.members.iter().filter_map(|m| valued.values.get(*m)) {
                    union.iter_mut().zip(member).for_each(|(u, m)| *u |= m);
                }
                valued.values.insert(merged.value.to_owned(), union);
            }
        } else {
            unavailable.extend(den_store::FACET_AXES.iter().copied());
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
            if postings.len() == entity.sections.len() {
                entities.push(Some(EntityKind { postings, known }));
            } else {
                unavailable.push(entity.name);
                stable.push(entity.name);
                entities.push(None);
            }
        }

        let bitset = |b: &Bits| b.len() * 8;
        let build_bytes = bits
            .values()
            .map(|v| v.values.values().map(bitset).sum::<usize>() + bitset(&v.known))
            .sum::<usize>()
            + entities.iter().flatten().map(|e| bitset(&e.known)).sum::<usize>()
            + postings_bytes
            + keys.len() * std::mem::size_of::<Key>()
            + 2 * bitset(&types[0]);
        unavailable.sort_unstable();
        unavailable.dedup();
        FilterIndex {
            keys,
            types,
            bits,
            entities,
            unavailable,
            stable,
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

    /// The IMDb- and export-dependent part, rebuilt when either has swapped since it was last built.
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
        let order = [0, 1].map(|t| {
            let mut ranked: Vec<(u32, f64, u32)> = ones(&self.types[t])
                .map(|row| {
                    let key = self.keys[row];
                    (row as u32, crate::plotrows::popularity(indexes, export.as_deref(), key), key.1)
                })
                .collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
            ranked.into_iter().map(|(row, _, _)| row).collect::<Vec<u32>>()
        });
        let order_id = [0, 1].map(|t| {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for &row in &order[t] {
                for byte in self.keys[row as usize].1.to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            format!("{hash:016x}")
        });
        Derived { ratings, export, rating, order, order_id, empty: [OnceLock::new(), OnceLock::new()] }
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

    /// Entities with a name or alias in which some word starts `prefix`, each once.
    fn matching(&self, prefix: &str) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .names
            .iter()
            .filter(|&&(_, from, to)| {
                let name = &self.text[from as usize..to as usize];
                name.starts_with(prefix)
                    || name.match_indices(' ').any(|(i, _)| name[i + 1..].starts_with(prefix))
            })
            .map(|&(entity, _, _)| entity)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
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

/// One request's view of the index: the live IMDb- and export-dependent parts, and the store.
pub struct Context<'a> {
    filter: &'a FilterIndex,
    indexes: &'a Indexes,
    derived: Arc<Derived>,
    characters: Option<Arc<CharacterIndex>>,
    media_type: MediaType,
    view: den_store::Store<'a>,
}

impl<'a> Context<'a> {
    pub fn new(indexes: &'a Indexes, media_type: MediaType, export: Option<Arc<TitleIndex>>) -> Context<'a> {
        let filter = indexes.filter();
        Context {
            filter,
            indexes,
            derived: filter.derived(indexes, export),
            characters: indexes.characters.as_ref().and_then(|c| c.index()),
            media_type,
            view: indexes.store.view(),
        }
    }

    fn t(&self) -> usize {
        type_index(self.media_type)
    }

    fn status(&self, spec: &Spec) -> Status {
        match spec.data {
            Data::Bits if self.filter.bits.contains_key(spec.name) => Status::Ready,
            Data::Bits if self.filter.unavailable.contains(&spec.name) => Status::Unavailable,
            Data::Bits => Status::NotOffered,
            Data::Rating if self.indexes.ratings.is_none() => Status::NotOffered,
            Data::Rating if self.derived.rating.is_some() => Status::Ready,
            Data::Rating => Status::Unavailable,
            Data::Entity(i) if ENTITY_KINDS[i].series_only && self.media_type == MediaType::Movie => {
                Status::NotOffered
            }
            Data::Entity(i) if self.filter.entities[i].is_some() => Status::Ready,
            Data::Entity(_) => Status::Unavailable,
            Data::Character if self.indexes.characters.is_none() => Status::NotOffered,
            Data::Character if self.characters.is_some() => Status::Ready,
            Data::Character => Status::Unavailable,
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
                let Ok(tmdb_id) = id.parse::<u32>() else { return (vec![0; words], known) };
                let similar = self.indexes.more_like_this(tmdb_id, self.media_type);
                let rows: Vec<usize> = similar
                    .iter()
                    .filter_map(|&id| {
                        self.view.row_of(u8::from(self.media_type == MediaType::Tv), id).ok().flatten()
                    })
                    .map(|row| row.0)
                    .collect();
                (from_rows(&mut rows.into_iter()), known)
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

    /// The titles of the route type carrying every applied item except those of `skip`: `-kind:id` keeps the
    /// titles known for the kind and not carrying the value.
    fn matched(&self, applied: &[(&'static Spec, &Item)], skip: Option<&str>) -> Bits {
        let mut matched = self.filter.types[self.t()].clone();
        for (spec, item) in applied {
            if Some(spec.name) == skip {
                continue;
            }
            let (value, known) = self.value_bits(spec, &item.id);
            if item.exclude {
                matched.iter_mut().zip(value.iter().zip(&known)).for_each(|(m, (v, k))| *m &= k & !v);
            } else {
                matched.iter_mut().zip(&value).for_each(|(m, v)| *m &= v);
            }
        }
        matched
    }

    /// How much of the route type each applied kind is known for.
    fn coverage(&self, applied: &[(&'static Spec, &Item)]) -> Value {
        let population = self.filter.types[self.t()].iter().map(|w| w.count_ones() as usize).sum::<usize>();
        let mut out = Map::new();
        for (spec, item) in applied {
            if out.contains_key(spec.name) {
                continue;
            }
            let (_, known) = self.value_bits(spec, &item.id);
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

    /// One kind's object in `counts.json`.
    fn kind_answer(&self, spec: &Spec, base: &[u64], selected: &[&Item]) -> Value {
        let mut values = Map::new();
        let mut labels = Map::new();
        let mut complete = true;
        match spec.data {
            Data::Bits | Data::Rating => {
                let valued = if spec.data == Data::Rating {
                    self.derived.rating.as_ref()
                } else {
                    self.filter.bits.get(spec.name)
                };
                if let Some(valued) = valued {
                    for (value, bits) in &valued.values {
                        let n = and_count(base, bits);
                        if n > 0 {
                            values.insert(value.clone(), n.into());
                        }
                    }
                }
            }
            Data::Entity(i) => {
                let mut counted = self.entity_counts(i, base);
                counted.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                complete = counted.len() <= TOP_K;
                for &(e, n) in counted.iter().take(TOP_K) {
                    let id = self.qid(e);
                    if let Some(label) = self.label(e) {
                        labels.insert(id.clone(), label.into());
                    }
                    values.insert(id, n.into());
                }
            }
            Data::Character | Data::Like => complete = false,
        }
        // Every selected value, with its label, even at 0.
        for item in selected {
            if !values.contains_key(&item.id) {
                let (bits, _) = self.value_bits(spec, &item.id);
                values.insert(item.id.clone(), and_count(base, &bits).into());
            }
            match spec.data {
                Data::Entity(_) => {
                    if let Some(label) = self.entity_of(&item.id).and_then(|e| self.label(e)) {
                        labels.insert(item.id.clone(), label.into());
                    }
                }
                Data::Character => {
                    labels.insert(item.id.clone(), item.id.replace('-', " ").into());
                }
                _ => {}
            }
        }
        let mut answer = json!({ "mode": spec.mode.name(), "complete": complete, "values": values });
        if !labels.is_empty() {
            answer["labels"] = Value::Object(labels);
        }
        let ids = |exclude: bool| -> Vec<&str> {
            selected.iter().filter(|i| i.exclude == exclude).map(|i| i.id.as_str()).collect()
        };
        if !ids(false).is_empty() {
            answer["selected"] = json!(ids(false));
        }
        if !ids(true).is_empty() {
            answer["excluded"] = json!(ids(true));
        }
        answer
    }

    /// The kinds object and total for a selection; the empty selection's is worked out once per type.
    fn kinds(&self, applied: &[(&'static Spec, &Item)]) -> (Value, usize) {
        if applied.is_empty() {
            return self.derived.empty[self.t()].get_or_init(|| self.work_kinds(&[])).clone();
        }
        self.work_kinds(applied)
    }

    fn work_kinds(&self, applied: &[(&'static Spec, &Item)]) -> (Value, usize) {
        let matched = self.matched(applied, None);
        let total = matched.iter().map(|w| w.count_ones() as usize).sum();
        let mut kinds = Map::new();
        for spec in SPECS.iter().filter(|s| self.status(s) == Status::Ready) {
            let selected: Vec<&Item> =
                applied.iter().filter(|(s, _)| s.name == spec.name).map(|(_, i)| *i).collect();
            if !spec.listed() && selected.is_empty() {
                continue;
            }
            // A one-pick kind counts each value as the alternative pick: without its own values applied.
            let answer = if spec.mode == Mode::Single && !selected.is_empty() {
                self.kind_answer(spec, &self.matched(applied, Some(spec.name)), &selected)
            } else {
                self.kind_answer(spec, &matched, &selected)
            };
            kinds.insert(spec.name.to_owned(), answer);
        }
        (Value::Object(kinds), total)
    }

    fn envelope(&self, answer: &mut Value, ignored: Vec<String>) -> bool {
        answer["ignored"] = json!(ignored);
        let unavailable = self.unavailable();
        if !unavailable.is_empty() {
            answer["kindsUnavailable"] = json!(unavailable);
        }
        // Degraded — a short cache and the header — only for a failure at runtime (the facts or facet rows
        // did not load, a ratings or principals join has not landed). A kind this dataset version cannot
        // answer is reported, but answers about it will not change until the next dataset does.
        unavailable.iter().any(|kind| !self.filter.stable.contains(kind))
    }

    /// `counts.json`. The flag says a kind this atlas should answer is unavailable.
    pub fn counts(&self, request: &Request) -> (Value, bool) {
        let (applied, ignored) = self.split(&request.items);
        let (kinds, total) = self.kinds(&applied);
        let mut answer = json!({ "total": total, "kinds": kinds, "coverage": self.coverage(&applied) });
        let degraded = self.envelope(&mut answer, ignored);
        (answer, degraded)
    }

    /// `titles.json`: the titles carrying the selection, most voted first — or, with a `like` selected, in
    /// its similarity order — as `/index/row`'s cards.
    pub fn titles(&self, request: &Request) -> (Value, bool) {
        let (applied, ignored) = self.split(&request.items);
        let matched = self.matched(&applied, None);
        let total: usize = matched.iter().map(|w| w.count_ones() as usize).sum();
        let like = applied
            .iter()
            .find(|(s, i)| s.data == Data::Like && !i.exclude)
            .and_then(|(_, i)| i.id.parse().ok());
        let (order, order_id): (Vec<u32>, String) = match like {
            Some(tmdb_id) => {
                let similar = self.indexes.more_like_this(tmdb_id, self.media_type);
                let media = u8::from(self.media_type == MediaType::Tv);
                let rows = similar
                    .iter()
                    .filter_map(|&id| self.view.row_of(media, id).ok().flatten())
                    .map(|row| row.0 as u32)
                    .collect();
                (rows, format!("like:{tmdb_id}"))
            }
            None => (self.derived.order[self.t()].clone(), self.derived.order_id[self.t()].clone()),
        };
        let titles: Vec<Value> = match self.indexes.cards.as_ref() {
            Some(cards) => order
                .iter()
                .filter(|&&row| has(&matched, row as usize))
                .skip(request.skip)
                .take(request.limit)
                .filter_map(|&row| {
                    let key = self.filter.keys[row as usize];
                    cards.get(&key).map(|card| crate::plotrows::title_json(self.indexes, key, card))
                })
                .collect(),
            None => Vec::new(),
        };
        let mut answer = json!({
            "titles": titles, "total": total, "order": order_id, "coverage": self.coverage(&applied),
        });
        let degraded = self.envelope(&mut answer, ignored);
        (answer, degraded)
    }

    /// `values/<kind>.json`: the kind's values under the selection, labelled, most titles first — those whose
    /// name has a word starting `q` when one is given.
    pub fn values(&self, spec: &'static Spec, request: &Request) -> (Value, bool) {
        let (applied, ignored) = self.split(&request.items);
        let own = applied.iter().any(|(s, _)| s.name == spec.name);
        let base = if spec.mode == Mode::Single && own {
            self.matched(&applied, Some(spec.name))
        } else {
            self.matched(&applied, None)
        };
        let q = request.q.as_deref();
        let words_match = |name: &str| {
            let key = crate::facts::name_key(name);
            q.is_none_or(|q| {
                key.starts_with(q) || key.match_indices(' ').any(|(i, _)| key[i + 1..].starts_with(q))
            })
        };
        // (id, name, count, tiebreak, tmdb)
        let mut found: Vec<(String, String, usize, usize, Option<u32>)> = Vec::new();
        if self.status(spec) == Status::Ready {
            match spec.data {
                Data::Bits | Data::Rating => {
                    let valued = if spec.data == Data::Rating {
                        self.derived.rating.as_ref()
                    } else {
                        self.filter.bits.get(spec.name)
                    };
                    for (value, bits) in valued.map(|v| &v.values).into_iter().flatten() {
                        let n = and_count(&base, bits);
                        if n > 0 && words_match(value) {
                            let all = and_count(&self.filter.types[self.t()], bits);
                            found.push((value.clone(), value.clone(), n, all, None));
                        }
                    }
                }
                Data::Entity(i) => {
                    if let Some(kind) = self.filter.entities[i].as_ref() {
                        let candidates: Vec<(u32, usize)> = match q {
                            Some(q) => self
                                .filter
                                .names(self.indexes)
                                .matching(q)
                                .into_iter()
                                .filter(|&e| self.listable(i, kind, e))
                                .filter_map(|e| {
                                    let mut rows: Vec<u32> =
                                        kind.postings.iter().flat_map(|p| p.of(e).iter().copied()).collect();
                                    rows.sort_unstable();
                                    rows.dedup();
                                    let n = rows.iter().filter(|&&r| has(&base, r as usize)).count();
                                    (n > 0).then_some((e, n))
                                })
                                .collect(),
                            None => self
                                .entity_counts(i, &base)
                                .into_iter()
                                .map(|(e, n)| (e, n as usize))
                                .collect(),
                        };
                        for (e, n) in candidates {
                            let name = self.label(e).unwrap_or_default().to_owned();
                            found.push((self.qid(e), name, n, kind.titles(e), self.tmdb(e)));
                        }
                    }
                }
                Data::Character => {
                    if let (Some(characters), Some(q)) = (self.characters.as_ref(), q) {
                        for (name, rows) in characters.named().with_prefix(q) {
                            let n = rows.iter().filter(|&&r| has(&base, r as usize)).count();
                            if n > 0 {
                                found.push((name.replace(' ', "-"), name.to_owned(), n, rows.len(), None));
                            }
                        }
                    }
                }
                Data::Like => {}
            }
        }
        found.sort_by(|a, b| b.2.cmp(&a.2).then(b.3.cmp(&a.3)).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)));
        let complete = found.len() <= request.limit;
        let values: Vec<Value> = found
            .into_iter()
            .take(request.limit)
            .map(|(id, name, count, _, tmdb)| {
                let mut value = json!({ "id": id, "name": name, "count": count });
                if let Some(tmdb) = tmdb {
                    value["tmdbId"] = json!(tmdb);
                }
                value
            })
            .collect();
        let mut answer =
            json!({ "kind": spec.name, "mode": spec.mode.name(), "values": values, "complete": complete });
        let degraded = self.envelope(&mut answer, ignored);
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
            (spec.name.to_owned(), about)
        })
        .collect();
    let merged: Map<String, Value> = crate::plotrows::MERGED_ROWS
        .iter()
        .map(|m| (format!("{}:{}", m.axis, m.value), json!(m.members)))
        .collect();
    json!({
        "kinds": kinds,
        "aliases": {
            "structure": {
                "single-day": "timespan", "anthology": "continuity", "*": "chronology",
                "about": "structure:<value> is the axis named here for that value, chronology for any other",
            },
        },
        "merged": merged,
        "runtimeBuckets": RUNTIME_BUCKETS.iter().map(|b| b.0).collect::<Vec<_>>(),
        "ratingThresholds": RATING_THRESHOLDS,
        "ratingMinVotes": MIN_VOTES,
        "exclude": "-<kind>:<id>: the titles known for the kind and not carrying the value",
        "canonical": "sel items [-]<kind>:<id>, ids normalised per kind, sorted by kind, then positive before \
                      excluded, then id as strings, each once, joined by ','; ids encoded as encodeURIComponent \
                      does, ':' ',' '-' literal. Then skip and limit (titles) or q and limit (values), each only \
                      when not its default. Any other spelling answers privately with Content-Location naming \
                      this one.",
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

    fn request(route: Route, query: &str) -> Request {
        Request::parse(route, query).expect("a well-formed request")
    }

    fn counts(indexes: &Indexes, media_type: MediaType, query: &str) -> Value {
        Context::new(indexes, media_type, None).counts(&request(Route::Counts, query)).0
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
    async fn rating_reads_imdb_with_a_vote_floor() {
        let ds = dataset("rating");
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        let tsv = "tconst\taverageRating\tnumVotes\ntt0000001\t8.4\t9000\n";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, tsv.as_bytes()).unwrap();
        let index = crate::ratings::build(&mapped.view(), &gz.finish().unwrap()).unwrap();
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
    /// it — as the title-only scores of store 5b1c3213b6a1 did — reports it unavailable, without calling the
    /// answer degraded: that is the dataset version, not a failure, and it lasts until the next one.
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
        assert_eq!(context.status(spec("warning").unwrap()), Status::Unavailable);
        let (answer, degraded) = context.counts(&request(Route::Counts, "sel=-warning:graphic_violence"));
        assert_eq!(answer["kindsUnavailable"], json!(["warning"]));
        assert_eq!(answer["ignored"], json!(["warning"]), "not answered with every title clean");
        assert!(!degraded, "a property of the dataset, not an outage");
    }

    /// Before the first ratings join lands, `rating` is unavailable — said so, and a selection naming it is
    /// answered around it — and when a join lands the rating kind and the vote order are rebuilt from it.
    #[tokio::test]
    async fn the_imdb_kinds_follow_the_ratings_join() {
        let ds = dataset("rating-swap");
        let ratings =
            Arc::new(crate::ratings::Ratings::new(ds.store.clone(), "http://127.0.0.1:9/").unwrap());
        let queries = IndexQueries::new(&ds).with_ratings(Some(Arc::clone(&ratings)));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let before = counts(&indexes, Movie, "sel=rating:8");
        assert_eq!(before["kindsUnavailable"], json!(["rating"]));
        assert_eq!((&before["ignored"], &before["total"]), (&json!(["rating"]), &3.into()));
        let order = Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "")).0;
        assert_eq!(ids(&order), vec![2, 1, 3], "the store's own votes");

        let mapped = crate::store::MappedStore::open(&ds.store).unwrap();
        let tsv = "tconst\taverageRating\tnumVotes\ntt0000001\t8.4\t9000\n";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, tsv.as_bytes()).unwrap();
        ratings.swap(crate::ratings::build(&mapped.view(), &gz.finish().unwrap()).unwrap());
        let after = counts(&indexes, Movie, "sel=rating:8");
        assert!(after.get("kindsUnavailable").is_none(), "{after}");
        assert_eq!(after["total"], 1);
        let reordered = Context::new(&indexes, Movie, None).titles(&request(Route::Titles, "")).0;
        assert_eq!(ids(&reordered), vec![1, 2, 3], "IMDb's 9,000 votes");
        assert_ne!(reordered["order"], order["order"], "a new order says so");
    }

    /// Characters are search-only: never listed, found by a normalised prefix of three characters or more,
    /// at most five, and selectable once found.
    #[tokio::test]
    async fn characters_are_found_by_prefix_and_selectable() {
        let ds = dataset("characters");
        let tsv = "tconst\tordering\tnconst\tcategory\tjob\tcharacters\n\
                   tt0000001\t1\tnm0000100\tactor\t\\N\t[\"Walter White\"]\n\
                   tt0000001\t2\tnm0000200\tactor\t\\N\t[\"Jesse Pinkman\"]\n\
                   tt0000002\t1\tnm0000100\tactor\t\\N\t[\"Walter White (voice)\"]\n";
        let rows = HashMap::from([(1, 0), (2, 1)]);
        let list = crate::characters::build(&rows, 12, std::io::Cursor::new(tsv)).unwrap();
        let characters = Arc::new(crate::characters::Characters::with_index(list));
        let queries = IndexQueries::new(&ds).with_characters(Some(characters));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let spec = spec("character").unwrap();
        let context = Context::new(&indexes, Movie, None);
        let found = context.values(spec, &request(Route::Values(spec), "q=Walt")).0;
        assert_eq!(found["values"], json!([{ "id": "walter-white", "name": "walter white", "count": 2 }]));
        let none = context.values(spec, &request(Route::Values(spec), "q=jes")).0;
        assert_eq!(none["values"], json!([]), "a name played in one title is not one to filter by");
        assert!(Request::parse(Route::Values(spec), "q=wa").is_err());
        assert!(Request::parse(Route::Values(spec), "").is_err());

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

    /// Den Web tests against the same file (tests/fixtures/facets-canonical.json): every url answers as its
    /// canonical one, a canonical url is canonical, and the refused ones are refused.
    #[test]
    fn the_canonical_fixture_holds() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/facets-canonical.json")).unwrap();
        let split = |url: &str| -> (Route, String, String) {
            let (path, query) = url.split_once('?').unwrap_or((url, ""));
            let parts: Vec<&str> = path.trim_start_matches("/index/filter/").split('/').collect();
            let route = match parts[1..] {
                ["counts.json"] => Route::Counts,
                ["titles.json"] => Route::Titles,
                ["values", kind] => Route::Values(spec(kind.trim_end_matches(".json")).unwrap()),
                _ => panic!("{url}"),
            };
            (route, path.to_owned(), query.to_owned())
        };
        for case in fixture["cases"].as_array().unwrap() {
            let (url, canonical) = (case["url"].as_str().unwrap(), case["canonical"].as_str().unwrap());
            let (route, path, query) = split(url);
            let parsed = Request::parse(route, &query).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert_eq!(format!("{path}{}", parsed.query()), canonical, "{url}");
            assert_eq!(parsed.canonical, url == canonical, "{url}");
            let (route, _, query) = split(canonical);
            assert!(
                Request::parse(route, &query).unwrap().canonical,
                "{canonical} is its own canonical form"
            );
        }
        for url in fixture["refused"].as_array().unwrap() {
            let url = url.as_str().unwrap();
            let (route, _, query) = split(url);
            assert!(Request::parse(route, &query).is_err(), "{url} should be refused");
        }
    }

    /// The filters over the REAL corpus, and what they cost. Opt-in: `DEN_STORE` names a store whose directory
    /// holds its `dataset.meta.json`; `DEN_IMDB_RATINGS`, a `title.ratings.tsv.gz`, adds the rating kind and
    /// the vote order.
    #[test]
    fn real_corpus_filters_and_timing() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to measure this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let ratings = std::env::var("DEN_IMDB_RATINGS").ok().map(|path| {
            let mapped = crate::store::MappedStore::open(&ds.store).unwrap();
            let gz = std::fs::read(path).expect("the ratings dump");
            Arc::new(crate::ratings::Ratings::with_index(crate::ratings::build(&mapped.view(), &gz).unwrap()))
        });
        // `DEN_IMDB_PRINCIPALS`, IMDb's whole `title.principals.tsv.gz`, adds the character kind.
        let characters = std::env::var("DEN_IMDB_PRINCIPALS").ok().map(|path| {
            let mapped = crate::store::MappedStore::open(&ds.store).unwrap();
            let (rows, row_count) = crate::ratings::imdb_rows(&mapped.view()).unwrap();
            let started = std::time::Instant::now();
            let reader =
                std::io::BufReader::new(flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()));
            let index = crate::characters::build(&rows, row_count, reader).unwrap();
            eprintln!(
                "characters: {} filterable names, {:.1} MB resident with the links ({} links), built in {:?}",
                index.named().len(),
                index.bytes() as f64 / 1e6,
                index.links(),
                started.elapsed()
            );
            Arc::new(crate::characters::Characters::with_index(index))
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
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
        for query in [
            "",
            "sel=genre:18",
            "sel=country:US,decade:1990,genre:28",
            "sel=person:Q25191",
            "sel=genre:18,person:Q25191",
            "sel=decade:2000,subgenre:Heist,-warning:violence",
        ] {
            let parsed = request(Route::Counts, query);
            time(&format!("counts {query:?}"), &|| context.counts(&parsed).0.to_string());
            let parsed = request(
                Route::Titles,
                &format!("{query}{}limit=40", if query.is_empty() { "" } else { "&" }),
            );
            time(&format!("titles {query:?} limit=40"), &|| context.titles(&parsed).0.to_string());
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
