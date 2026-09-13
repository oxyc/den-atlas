//! `POST /recommend` — the titles a featured surface leads with (the web's billboard, and later the TV's
//! heroes), ranked here so that no client ranks.
//!
//! A port of the web app's `billboard.ts` and of the pipeline in `Library.svelte` that fed it. Where the web
//! asked TMDB what a title is, this reads what atlas already holds: the dataset's labels (primary genre,
//! animation, subgenres, moods), `facets.bin` (country, original language, year) and, once the dataset
//! publishes them, the Wikidata facts (dates with their precision, genres, makers, cast, franchise). What a
//! client got from TMDB lists it already fetched for its own rows — release date, genres, popularity,
//! rating — arrives as a per-candidate hint and fills only what atlas doesn't know.
//!
//! What a billboard shows is not the leading row. A "Because you watched X" row is the nearest neighbours of
//! a title already watched, and after enough history that neighbourhood IS the history. A billboard is the
//! top of a page opened to find something to watch, so this ranks what is new — new in the world and new to
//! this library — weighted towards the library's taste and away from what it has already worn out.
//!
//! Pure and deterministic given `now`, so a recorded request replays to the same answer.

use crate::catalog::{new_catalog_id, Provider, TRENDING_ID};
use crate::config::Config;
use crate::facts::{Facts, Released};
use crate::queries::Indexes;
use crate::AppState;
use den_index::MediaType;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

/// Names the scoring rules, so a kept answer can say which rules chose it.
pub const SCORER: &str = "billboard-port-1";

type Key = (MediaType, u32);

/// The same 120 days that New Releases calls new, so the billboard and that row can't disagree.
const FRESH_DAYS: f64 = 120.0;
/// How fast anticipation builds on the way to release — shorter than the decay after it, because "out on
/// Friday" is worth saying and "out next December" is not.
const ANTICIPATION_DAYS: f64 = 45.0;
/// Votes at which a rating stands on its own; below it the score is pulled towards `RATING_PRIOR`.
const RATING_PRIOR_VOTES: f64 = 200.0;
/// What a title is worth before anyone has said: a little above TMDB's middle, where most rated titles land.
const RATING_PRIOR: f64 = 6.6;
/// How the terms trade off. Freshness leads but not alone; attention is weighted to match it, so among new
/// things the ones people are actually watching win.
const WEIGHT_FRESH: f64 = 0.22;
const WEIGHT_ATTENTION: f64 = 0.4;
const WEIGHT_QUALITY: f64 = 0.15;
/// What a title keeps when it matches nothing this library watches. Taste multiplies the rest rather than
/// adding to it, so it decides between contenders instead of being outvoted by them.
const TASTE_FLOOR: f64 = 0.35;
/// How much a second kind of attention adds once the first is counted.
const CORROBORATION: f64 = 0.3;
/// How the facets of a taste trade off; they sum to one. Language is the smallest: in a library three-quarters
/// in English it would otherwise hand the decision to every Hollywood release.
const FACET_GENRES: f64 = 0.56;
const FACET_PEOPLE: f64 = 0.22;
const FACET_COUNTRIES: f64 = 0.1;
const FACET_LANGUAGES: f64 = 0.06;
const FACET_DECADES: f64 = 0.06;
/// How much atlas's labels add on top of the facets, where both the title and the library carry them.
///
/// In the web app labels only chose which sixty candidates TMDB was asked about, which is how they came to
/// decide the field. With every candidate judged here there is no such cut, so without a term of their own
/// the labels would stop counting at all — and a primary genre alone cannot tell Nordic noir from a slasher.
const LABEL_WEIGHT: f64 = 0.3;
/// How the two kinds of label trade off: a subgenre says what a thing is, a mood only how it feels.
const LABEL_SUBGENRES: f64 = 0.7;
const LABEL_MOODS: f64 = 0.3;
/// Weight of matching people at which the household counts as following them: one lead is a coincidence.
const PEOPLE_ENOUGH: f64 = 2.0;
/// Cast members that count as much as a maker. Wikidata's cast is unordered and about ten deep where the web
/// read the top three billed, so each cast member counts `3 / cast size`, at most one.
const CAST_BILLED: f64 = 3.0;
/// How far towards a full match a title carries for being the next of something already followed.
const FRANCHISE_LIFT: f64 = 0.5;
/// Weight of dislike that halves a title's affinity.
const DISLIKE_PATIENCE: f64 = 2.0;
/// Keeps a facet's ratio finite when a share is zero.
const FACET_FLOOR: f64 = 0.02;
/// How sharply the summed facets separate.
const FACET_SHARPNESS: f64 = 3.0;
/// What a title keeps at the dead centre of what this household already watches.
const NOVELTY_FLOOR: f64 = 0.5;
/// How much of a match is too little to belong on a personal billboard.
const STRANGER: f64 = 0.15;
/// The titles asked "more like this" about, to tell what the library has worn out.
const SEEDS: usize = 8;
/// Redundancy divides by at least this many seeds, so one answering seed can't call its neighbours wholly redundant.
const MIN_SEEDS: usize = 3;
/// Judged candidates enough to drop the unjudged ones.
const JUDGED_ENOUGH: usize = 20;
/// Slides by default, and at most.
const SLIDES: usize = 40;
const MAX_SLIDES: usize = 100;
/// "New on" lists read, as the web app does.
const ARRIVAL_LISTS: usize = 8;
/// The most any one request may name.
pub const MAX_LIBRARY: usize = 5000;
pub const MAX_OWNED: usize = 10_000;
pub const MAX_CANDIDATES: usize = 2000;
pub const MAX_SERVICES: usize = 64;

// ---------------------------------------------------------------------------------------------------------
// The request.

#[derive(Deserialize)]
pub struct Request {
    /// `home` (both types), `movies` or `series`.
    #[serde(default)]
    pub surface: Option<String>,
    /// RFC 3339 in UTC (`…Z`); the server's clock when absent.
    #[serde(default)]
    pub now: Option<String>,
    /// The services this household has, each in its own country. None: the install's own.
    #[serde(default)]
    pub services: Vec<ServicePick>,
    /// What the library says about taste, as signed weights.
    #[serde(default)]
    pub library: Vec<LibraryEntry>,
    /// Every title the library holds, whatever its status — never shown.
    #[serde(default)]
    pub owned: Vec<Ref>,
    #[serde(default)]
    pub hide: Hide,
    /// Titles from the client's own lists, in list order.
    #[serde(default)]
    pub candidates: Vec<Offered>,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl Request {
    pub fn check(&self) -> Result<(), String> {
        for (len, max, what) in [
            (self.library.len(), MAX_LIBRARY, "library titles"),
            (self.owned.len(), MAX_OWNED, "owned titles"),
            (self.candidates.len(), MAX_CANDIDATES, "candidates"),
            (self.services.len(), MAX_SERVICES, "services"),
        ] {
            if len > max {
                return Err(format!("at most {max} {what}"));
            }
        }
        Ok(())
    }

    fn only(&self) -> Option<MediaType> {
        match self.surface.as_deref() {
            Some("movies") => Some(MediaType::Movie),
            Some("series") => Some(MediaType::Tv),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
pub struct ServicePick {
    /// The provider id atlas's catalogs carry (`denProviderIds`).
    pub id: i64,
    pub country: String,
}

#[derive(Deserialize)]
pub struct Ref {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: u32,
}

#[derive(Deserialize)]
pub struct LibraryEntry {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: u32,
    /// Watched or in progress 1, watchlisted 0.6, like +0.5, love +1; a dislike is −1.5 outright.
    pub weight: f64,
    /// When it was last touched; orders the seeds.
    #[serde(default)]
    pub at: f64,
}

/// The household's hide rules (the TV's `UserPreferences.isHidden`).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Hide {
    #[serde(default)]
    pub min_year: Option<i64>,
    /// TMDB genre ids.
    #[serde(default)]
    pub genres: Vec<u16>,
    /// ISO 639-1.
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub anime: bool,
}

#[derive(Deserialize)]
pub struct Offered {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: u32,
    /// Its place in its list, when that list is a ranking.
    #[serde(default)]
    pub rank: Option<f64>,
    #[serde(default)]
    pub of: Option<f64>,
    #[serde(default)]
    pub hint: Hint,
}

/// What the client's list said about a title. Read only where atlas knows nothing itself.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Hint {
    pub release_date: Option<String>,
    pub genre_ids: Option<Vec<u16>>,
    pub original_language: Option<String>,
    pub popularity: Option<f64>,
    pub rating: Option<f64>,
    pub votes: Option<f64>,
    pub adult: Option<bool>,
    pub imdb_id: Option<String>,
}

fn media_type(name: &str) -> Option<MediaType> {
    match name {
        "movie" => Some(MediaType::Movie),
        "series" | "tv" => Some(MediaType::Tv),
        _ => None,
    }
}

fn type_name(media_type: MediaType) -> &'static str {
    match media_type {
        MediaType::Movie => "movie",
        MediaType::Tv => "series",
    }
}

/// `2026-09-12T20:00:00Z` (fractional seconds allowed) as days since 1970-01-01.
pub fn parse_now(text: &str) -> Option<f64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let day = Released::parse(date, "day")?.first_day;
    let mut clock = time.splitn(3, ':');
    let hours: f64 = clock.next()?.parse().ok()?;
    let minutes: f64 = clock.next()?.parse().ok()?;
    let seconds: f64 = clock.next().unwrap_or("0").parse().ok()?;
    Some(day as f64 + (hours * 3600.0 + minutes * 60.0 + seconds) / 86_400.0)
}

/// Now, as days since 1970-01-01.
pub fn today() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() / 86_400.0)
}

// ---------------------------------------------------------------------------------------------------------
// Genres.

/// A TMDB genre id as a film's genre ids. Series have genres of their own for a few pairs — "Action &
/// Adventure", "Sci-Fi & Fantasy", "War & Politics" — which name nothing a film can share, so a taste read
/// across both types folds them into the films' names. The labels already name every title's genre that way.
pub fn fold_genre(genre: u16) -> &'static [u16] {
    match genre {
        10759 => &[28, 12],
        10765 => &[878, 14],
        10768 => &[10752],
        other => GENRE_IDS.iter().find(|&&g| g == other).map_or(&[], std::slice::from_ref),
    }
}

/// Every TMDB genre id that names the same thing for a film and a series, and the series-only ones that
/// have no film counterpart (kids, news, reality, soap, talk).
const GENRE_IDS: &[u16] = &[
    28, 12, 16, 35, 80, 99, 18, 10751, 14, 36, 27, 10402, 9648, 10749, 878, 10770, 53, 10752, 37, 10762,
    10763, 10764, 10766, 10767,
];

/// The labels' primary genre (TMDB's film genre names) as its id. Never "Animation": that is `animated`.
fn genre_named(name: &str) -> Option<u16> {
    Some(match name {
        "Action" => 28,
        "Adventure" => 12,
        "Animation" => 16,
        "Comedy" => 35,
        "Crime" => 80,
        "Documentary" => 99,
        "Drama" => 18,
        "Family" => 10751,
        "Fantasy" => 14,
        "History" => 36,
        "Horror" => 27,
        "Music" => 10402,
        "Mystery" => 9648,
        "Romance" => 10749,
        "Science Fiction" => 878,
        "TV Movie" => 10770,
        "Thriller" => 53,
        "War" => 10752,
        "Western" => 37,
        _ => return None,
    })
}

const ANIMATION: u16 = 16;

// ---------------------------------------------------------------------------------------------------------
// What is known about a title.

/// A title's labels, as ranking reads them.
#[derive(Clone, Debug, Default)]
pub struct LabelSet<'a> {
    pub subgenres: Vec<(&'a str, f64)>,
    pub moods: Vec<(&'a str, f64)>,
}

/// Everything ranking reads about one title. Every field may be unknown.
#[derive(Clone, Debug, Default)]
pub struct Title<'a> {
    pub released: Option<Released>,
    pub rating: Option<f64>,
    pub votes: Option<f64>,
    /// Whether `votes` is a stand-in for a count nobody gave (JustWatch's IMDb score comes without one).
    pub estimated_votes: bool,
    pub popularity: Option<f64>,
    /// TMDB genre ids, series' own folded into films' (`fold_genre`).
    pub genres: Vec<u16>,
    /// Its original language, which the hide rules read.
    pub original_language: Option<[u8; 2]>,
    pub languages: Vec<[u8; 2]>,
    pub countries: Vec<[u8; 2]>,
    /// Who made it and who is in it, each with how much it counts (see `CAST_BILLED`).
    pub people: Vec<(u32, f64)>,
    pub franchise: Option<u32>,
    pub labels: Option<LabelSet<'a>>,
    pub adult: bool,
    pub imdb_id: Option<String>,
}

/// A title from one of atlas's own JustWatch lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub key: Key,
    pub imdb_id: Option<String>,
    /// JustWatch's IMDb score.
    pub rating: Option<f64>,
    pub year: Option<i64>,
}

/// Atlas's lists for one request: each "new on <service>" list, and Trending Everywhere with both types
/// interleaved.
#[derive(Default)]
pub struct Lists {
    pub arrivals: Vec<Vec<Listed>>,
    pub everywhere: Vec<Listed>,
}

/// A rendered catalog body (`catalog::render_metas`) back as titles. A row without a TMDB id can't be named
/// by a Den client, so it is dropped.
pub fn listed(body: &str, media_type: MediaType) -> Vec<Listed> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    let Some(metas) = value.get("metas").and_then(|m| m.as_array()) else { return Vec::new() };
    metas
        .iter()
        .filter_map(|meta| {
            let id = u32::try_from(meta.get("moviedb_id")?.as_u64()?).ok()?;
            let text = |field: &str| meta.get(field).and_then(|v| v.as_str());
            Some(Listed {
                key: (media_type, id),
                imdb_id: text("imdb_id").map(str::to_owned),
                rating: text("imdbRating").and_then(|r| r.parse().ok()),
                year: text("releaseInfo").and_then(|y| y.get(..4)).and_then(|y| y.parse().ok()),
            })
        })
        .collect()
}

/// What atlas knows, to describe titles from.
pub struct Knowledge<'a> {
    pub indexes: &'a Indexes,
}

impl<'a> Knowledge<'a> {
    fn facts(&self) -> Option<&'a Facts> {
        self.indexes.facts.as_ref()
    }

    /// A title as ranking reads it: the facts first, then the labels and the facet blob, then whatever a list
    /// said about it.
    pub fn title(&self, (media_type, id): Key, hint: Option<&Hint>, listed: Option<&Listed>) -> Title<'a> {
        let labels = self.indexes.plot.labels(id, media_type);
        let facets = self.indexes.facets.as_ref().and_then(|f| f.title(id, media_type));
        let record = self.facts().and_then(|f| f.get(id, media_type));
        let mut title = Title::default();

        let mut genres: Vec<u16> = record.map(|r| r.genres.clone()).unwrap_or_default();
        if let Some(labels) = &labels {
            let primary = genre_named(labels.primary_genre).into_iter();
            for genre in primary.chain(labels.animated.then_some(ANIMATION)) {
                if !genres.contains(&genre) {
                    genres.push(genre);
                }
            }
        }
        if genres.is_empty() {
            // A list names every genre it knows, where the labels name one. Without the facts' fuller
            // genres to match, only a list's first genre is read, so that a title atlas has never seen isn't
            // favoured over the library's own for carrying three genres where they carry one.
            let hinted = hint.and_then(|h| h.genre_ids.as_deref()).unwrap_or_default();
            let take = if self.facts().is_some() { hinted.len() } else { 1 };
            for &genre in hinted.iter().take(take) {
                for &folded in fold_genre(genre) {
                    if !genres.contains(&folded) {
                        genres.push(folded);
                    }
                }
            }
        }
        title.genres = genres;

        let language = |code: &str| -> Option<[u8; 2]> {
            match code.as_bytes() {
                [a, b] if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => {
                    Some([a.to_ascii_lowercase(), b.to_ascii_lowercase()])
                }
                _ => None,
            }
        };
        title.original_language = facets
            .and_then(|f| f.language)
            .or_else(|| hint.and_then(|h| h.original_language.as_deref()).and_then(language))
            .or_else(|| record.and_then(|r| r.languages.first().copied()));
        title.languages = match (facets.and_then(|f| f.language), record) {
            (Some(code), _) => vec![code],
            (None, Some(r)) if !r.languages.is_empty() => r.languages.clone(),
            _ => title.original_language.into_iter().collect(),
        };
        title.countries = match record {
            Some(r) if !r.countries.is_empty() => r.countries.clone(),
            _ => facets.and_then(|f| f.country).into_iter().collect(),
        };
        if let Some(r) = record {
            title.people = r.makers.iter().map(|&id| (id, 1.0)).collect();
            let each = if r.cast.is_empty() { 0.0 } else { (CAST_BILLED / r.cast.len() as f64).min(1.0) };
            title.people.extend(r.cast.iter().filter(|id| !r.makers.contains(*id)).map(|&id| (id, each)));
            title.franchise = r.franchise;
        }
        title.released = hint
            .and_then(|h| h.release_date.as_deref())
            .and_then(|date| Released::parse(date, "day"))
            .or_else(|| record.and_then(|r| r.released))
            .or_else(|| facets.and_then(|f| f.year).map(|y| Released::year(i64::from(y))))
            .or_else(|| listed.and_then(|l| l.year).map(Released::year));
        match (hint.and_then(|h| h.rating), listed.and_then(|l| l.rating)) {
            (Some(rating), _) => {
                title.rating = Some(rating);
                title.votes = hint.and_then(|h| h.votes);
            }
            // JustWatch gives IMDb's score without its vote count: trusted as far as the prior's own weight.
            (None, Some(rating)) => {
                title.rating = Some(rating);
                title.votes = Some(RATING_PRIOR_VOTES);
                title.estimated_votes = true;
            }
            (None, None) => {}
        }
        title.popularity = hint.and_then(|h| h.popularity);
        title.adult = hint.and_then(|h| h.adult).unwrap_or(false);
        title.imdb_id = hint
            .and_then(|h| h.imdb_id.clone())
            .or_else(|| listed.and_then(|l| l.imdb_id.clone()))
            .or_else(|| record.and_then(|r| r.imdb_id.clone()))
            .filter(|id| id.starts_with("tt"));
        title.labels = labels.map(|l| LabelSet { subgenres: l.subgenres, moods: l.moods });
        title
    }
}

// ---------------------------------------------------------------------------------------------------------
// Scoring: `billboard.ts`, term for term.

/// A place in a list: its rank from 0, and how long the list was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placing {
    pub rank: f64,
    pub of: f64,
}

/// A title in the running, with where it came from.
#[derive(Clone, Debug)]
pub struct Candidate<'a> {
    pub key: Key,
    pub title: Title<'a>,
    /// Its place in a list that is itself a ranking (trending).
    pub rank: Option<Placing>,
    /// Its place in a "new on <service>" list.
    pub arrival: Option<Placing>,
    /// The share of the library's seeds whose "more like this" includes it, 0…1.
    pub redundancy: Option<f64>,
}

/// What the library says its viewer likes, facet by facet. Weights are signed: a dislike subtracts.
#[derive(Default)]
pub struct Taste<'a> {
    genres: HashMap<u16, f64>,
    languages: HashMap<[u8; 2], f64>,
    countries: HashMap<[u8; 2], f64>,
    people: HashMap<u32, f64>,
    decades: HashMap<i64, f64>,
    franchises: HashMap<u32, f64>,
    subgenres: HashMap<&'a str, f64>,
    moods: HashMap<&'a str, f64>,
    /// What one of this library's own titles scores on each facet, averaged over the library: a facet is read
    /// as how far a title departs from that, not as a level.
    typical: Typical,
}

#[derive(Default)]
struct Typical {
    genres: f64,
    languages: f64,
    countries: f64,
    decades: f64,
    subgenres: f64,
    moods: f64,
}

fn decades_of(title: &Title<'_>) -> Vec<i64> {
    title.released.map(|r| r.year_of().div_euclid(10) * 10).into_iter().collect()
}

fn franchises_of(title: &Title<'_>) -> Vec<u32> {
    title.franchise.into_iter().collect()
}

fn names<'a>(pairs: &[(&'a str, f64)]) -> Vec<&'a str> {
    pairs.iter().map(|&(name, _)| name).collect()
}

/// How much of a facet's liked weight these keys account for. Only the liked part.
fn share_of<K: Copy + Eq + Hash>(map: &HashMap<K, f64>, keys: &[K]) -> f64 {
    let liked: f64 = map.values().filter(|&&w| w > 0.0).sum();
    if liked <= 0.0 {
        return 0.0;
    }
    let mine: f64 = keys
        .iter()
        .enumerate()
        .filter(|&(at, key)| !keys[..at].contains(key))
        .map(|(_, key)| map.get(key).copied().unwrap_or(0.0).max(0.0))
        .sum();
    (mine / liked).min(1.0)
}

fn add<K: Copy + Eq + Hash>(map: &mut HashMap<K, f64>, keys: impl IntoIterator<Item = (K, f64)>) {
    let mut seen: Vec<K> = Vec::new();
    for (key, weight) in keys {
        if !seen.contains(&key) {
            seen.push(key);
            *map.entry(key).or_insert(0.0) += weight;
        }
    }
}

/// The shape of a library, from its titles and their signed weights.
pub fn taste_of<'a>(entries: &[(Title<'a>, f64)]) -> Taste<'a> {
    let mut taste = Taste::default();
    for (title, weight) in entries {
        let w = *weight;
        if w == 0.0 {
            continue;
        }
        add(&mut taste.genres, title.genres.iter().map(|&g| (g, w)));
        add(&mut taste.languages, title.languages.iter().map(|&l| (l, w)));
        add(&mut taste.countries, title.countries.iter().map(|&c| (c, w)));
        add(&mut taste.people, title.people.iter().map(|&(p, share)| (p, w * share)));
        add(&mut taste.decades, decades_of(title).into_iter().map(|d| (d, w)));
        add(&mut taste.franchises, franchises_of(title).into_iter().map(|f| (f, w)));
        if let Some(labels) = &title.labels {
            add(&mut taste.subgenres, labels.subgenres.iter().map(|&(n, c)| (n, w * c)));
            add(&mut taste.moods, labels.moods.iter().map(|&(n, c)| (n, w * c)));
        }
    }
    let liked: Vec<(&Title<'a>, f64)> =
        entries.iter().filter(|(_, w)| *w > 0.0).map(|(t, w)| (t, *w)).collect();
    let mean = |score: &dyn Fn(&Title<'a>) -> Option<f64>| {
        let (mut sum, mut total) = (0.0, 0.0);
        for &(title, weight) in &liked {
            if let Some(share) = score(title) {
                sum += weight * share;
                total += weight;
            }
        }
        if total > 0.0 {
            sum / total
        } else {
            0.0
        }
    };
    taste.typical = Typical {
        genres: mean(&|t| Some(share_of(&taste.genres, &t.genres))),
        languages: mean(&|t| Some(share_of(&taste.languages, &t.languages))),
        countries: mean(&|t| Some(share_of(&taste.countries, &t.countries))),
        decades: mean(&|t| Some(share_of(&taste.decades, &decades_of(t)))),
        // Over the titles that carry labels at all, as the web's label profile was.
        subgenres: mean(&|t| t.labels.as_ref().map(|l| share_of(&taste.subgenres, &names(&l.subgenres)))),
        moods: mean(&|t| t.labels.as_ref().map(|l| share_of(&taste.moods, &names(&l.moods)))),
    };
    taste
}

/// Whether the household follows the people behind this title, saturating: one shared lead is a coincidence.
fn following(map: &HashMap<u32, f64>, people: &[(u32, f64)]) -> f64 {
    if people.is_empty() || map.is_empty() {
        return 0.0;
    }
    let met: f64 = people
        .iter()
        .enumerate()
        .filter(|&(at, (id, _))| !people[..at].iter().any(|(other, _)| other == id))
        .map(|(_, &(id, share))| map.get(&id).copied().unwrap_or(0.0).max(0.0) * share)
        .sum();
    1.0 - (-met / PEOPLE_ENOUGH).exp()
}

/// How much this library has turned down what the title is made of.
fn distaste(title: &Title<'_>, taste: &Taste<'_>) -> f64 {
    let owed = |weight: Option<&f64>| -weight.copied().unwrap_or(0.0).min(0.0);
    let mut against = 0.0;
    let mut genres: Vec<u16> = Vec::new();
    for &genre in &title.genres {
        if !genres.contains(&genre) {
            genres.push(genre);
            against += owed(taste.genres.get(&genre));
        }
    }
    let mut people: Vec<u32> = Vec::new();
    for &(id, share) in &title.people {
        if !people.contains(&id) {
            people.push(id);
            against += owed(taste.people.get(&id)) * share;
        }
    }
    for franchise in franchises_of(title) {
        against += owed(taste.franchises.get(&franchise));
    }
    if against <= 0.0 {
        0.0
    } else {
        against / (against + DISLIKE_PATIENCE)
    }
}

/// A facet read against the library's typical title: nothing where there is nothing to compare.
fn facet<K: Copy + Eq + Hash>(map: &HashMap<K, f64>, keys: &[K], typical: f64) -> f64 {
    if keys.is_empty() || typical <= 0.0 {
        0.0
    } else {
        ((share_of(map, keys) + FACET_FLOOR) / (typical + FACET_FLOOR)).ln().tanh()
    }
}

/// How much a title looks like the library, read as how far it departs from the library's own average.
pub fn affinity(title: &Title<'_>, taste: Option<&Taste<'_>>) -> f64 {
    let Some(taste) = taste.filter(|t| t.typical.genres > 0.0) else { return 0.0 };
    let t = &taste.typical;
    let mut evidence = FACET_GENRES * facet(&taste.genres, &title.genres, t.genres)
        + FACET_LANGUAGES * facet(&taste.languages, &title.languages, t.languages)
        + FACET_COUNTRIES * facet(&taste.countries, &title.countries, t.countries)
        + FACET_DECADES * facet(&taste.decades, &decades_of(title), t.decades)
        + FACET_PEOPLE * following(&taste.people, &title.people);
    if let Some(labels) = &title.labels {
        evidence += LABEL_WEIGHT
            * (LABEL_SUBGENRES * facet(&taste.subgenres, &names(&labels.subgenres), t.subgenres)
                + LABEL_MOODS * facet(&taste.moods, &names(&labels.moods), t.moods));
    }
    let matched = 1.0 / (1.0 + (-FACET_SHARPNESS * evidence).exp());
    // The next of something already followed is wanted whatever else it is.
    let followed = title.franchise.is_some_and(|f| taste.franchises.get(&f).copied().unwrap_or(0.0) > 0.0);
    let lifted = if followed { matched + (1.0 - matched) * FRANCHISE_LIFT } else { matched };
    lifted * (1.0 - distaste(title, taste))
}

/// How much of this title is not already covered by what the household watches.
pub fn novelty(candidate: &Candidate<'_>) -> f64 {
    1.0 - (1.0 - NOVELTY_FLOOR) * candidate.redundancy.unwrap_or(0.0).clamp(0.0, 1.0)
}

/// Peaks on release day and falls away either side of it; unknown dates score as old. A date known only to
/// its month or year scores the mean over the days it could be.
pub fn freshness(title: &Title<'_>, now: f64) -> f64 {
    let Some(released) = title.released else { return 0.0 };
    let at = |day: i64| {
        let days = now - day as f64;
        if days <= 0.0 {
            (days / ANTICIPATION_DAYS).exp()
        } else {
            (-days / FRESH_DAYS).exp()
        }
    };
    let span = released.span_days.max(1);
    (0..span).map(|i| at(released.first_day + i)).sum::<f64>() / span as f64
}

fn standing(placing: Option<Placing>) -> f64 {
    placing.filter(|p| p.of > 0.0).map_or(0.0, |p| (1.0 - p.rank / p.of).max(0.0))
}

/// Attention from a ranked list, else from popularity read against the busiest title in the pool.
pub fn buzz(candidate: &Candidate<'_>, busiest: f64) -> f64 {
    let ranked = standing(candidate.rank);
    let popular =
        if busiest > 0.0 { candidate.title.popularity.unwrap_or(0.0).ln_1p() / busiest.ln_1p() } else { 0.0 };
    ranked.max(popular.min(1.0))
}

/// Newly watchable on a service this household has.
pub fn arrival(candidate: &Candidate<'_>) -> f64 {
    standing(candidate.arrival)
}

/// The two kinds of attention, counted once rather than twice.
pub fn attention(candidate: &Candidate<'_>, busiest: f64) -> f64 {
    let (a, b) = (buzz(candidate, busiest), arrival(candidate));
    a.max(b) + CORROBORATION * a.min(b)
}

/// Well liked, on enough votes to mean it.
pub fn quality(title: &Title<'_>) -> f64 {
    let votes = if title.rating.is_none() { 0.0 } else { title.votes.unwrap_or(0.0) };
    let rating = title.rating.unwrap_or(RATING_PRIOR);
    let settled = (votes * rating + RATING_PRIOR_VOTES * RATING_PRIOR) / (votes + RATING_PRIOR_VOTES);
    ((settled - 6.0) / 2.0).clamp(0.0, 1.0)
}

/// Why a title scored what it did.
#[derive(Clone, Copy, Debug)]
pub struct Why {
    pub fresh: f64,
    pub attention: f64,
    pub quality: f64,
    pub taste: f64,
    pub novelty: f64,
    pub score: f64,
}

pub fn score(candidate: &Candidate<'_>, now: f64, busiest: f64, taste: Option<&Taste<'_>>) -> Why {
    let fresh = freshness(&candidate.title, now);
    let attention = attention(candidate, busiest);
    let quality = quality(&candidate.title);
    let taste = affinity(&candidate.title, taste);
    let novelty = novelty(candidate);
    let worth = WEIGHT_FRESH * fresh + WEIGHT_ATTENTION * attention + WEIGHT_QUALITY * quality;
    let score = worth * (TASTE_FLOOR + (1.0 - TASTE_FLOOR) * taste) * novelty;
    Why { fresh, attention, quality, taste, novelty, score }
}

/// Whether a title resembles too little of what this library holds. Only asked when there is an answer.
fn stranger_here(title: &Title<'_>, taste: Option<&Taste<'_>>) -> bool {
    match taste {
        Some(taste) if !taste.genres.is_empty() && !title.genres.is_empty() => {
            affinity(title, Some(taste)) < STRANGER
        }
        _ => false,
    }
}

/// The better of two placings.
fn best(a: Option<Placing>, b: Option<Placing>) -> Option<Placing> {
    let at = |p: Option<Placing>| p.filter(|p| p.of > 0.0).map_or(-1.0, |p| 1.0 - p.rank / p.of);
    if at(a) < 0.0 {
        return b;
    }
    if at(b) < 0.0 {
        return a;
    }
    if at(a) >= at(b) {
        a
    } else {
        b
    }
}

/// The same title from two sources is one candidate holding everything both knew about it; the first says
/// what it knows first, except where the second knows it better. Atlas's own lists come first in the pool
/// and name a title by its year and a vote-less score, so a client list's exact date and counted rating
/// must not lose to them.
fn merge<'a>(a: Candidate<'a>, b: Candidate<'a>) -> Candidate<'a> {
    fn either<T>(x: Vec<T>, y: Vec<T>) -> Vec<T> {
        if x.is_empty() {
            y
        } else {
            x
        }
    }
    let (x, y) = (a.title, b.title);
    let released = match (x.released, y.released) {
        (Some(first), Some(second)) if second.span_days < first.span_days => Some(second),
        (first, second) => first.or(second),
    };
    let second_rating = x.rating.is_none() || (x.estimated_votes && y.rating.is_some() && !y.estimated_votes);
    let (rating, votes, estimated_votes) = if second_rating {
        (y.rating, y.votes, y.estimated_votes)
    } else {
        (x.rating, x.votes, x.estimated_votes)
    };
    Candidate {
        key: a.key,
        title: Title {
            released,
            rating,
            votes,
            estimated_votes,
            popularity: x.popularity.or(y.popularity),
            genres: either(x.genres, y.genres),
            original_language: x.original_language.or(y.original_language),
            languages: either(x.languages, y.languages),
            countries: either(x.countries, y.countries),
            people: either(x.people, y.people),
            franchise: x.franchise.or(y.franchise),
            labels: x.labels.or(y.labels),
            adult: x.adult || y.adult,
            imdb_id: x.imdb_id.or(y.imdb_id),
        },
        rank: best(a.rank, b.rank),
        arrival: best(a.arrival, b.arrival),
        redundancy: a.redundancy.or(b.redundancy),
    }
}

/// The slides, best first: deduped, filtered, and only then cut to `slides`.
pub fn pick<'a>(
    candidates: Vec<Candidate<'a>>,
    now: f64,
    slides: usize,
    keep: impl Fn(&Candidate<'a>) -> bool,
    taste: Option<&Taste<'a>>,
) -> Vec<(Candidate<'a>, Why)> {
    let mut order: Vec<Key> = Vec::new();
    let mut by_key: HashMap<Key, Candidate<'a>> = HashMap::new();
    for candidate in candidates {
        if !keep(&candidate) || stranger_here(&candidate.title, taste) {
            continue;
        }
        let merged = match by_key.remove(&candidate.key) {
            Some(already) => merge(already, candidate),
            None => {
                order.push(candidate.key);
                candidate
            }
        };
        by_key.insert(merged.key, merged);
    }
    let running: Vec<Candidate<'a>> = order.into_iter().filter_map(|key| by_key.remove(&key)).collect();
    let busiest = running.iter().filter_map(|c| c.title.popularity).fold(0.0, f64::max);
    let mut scored: Vec<(Candidate<'a>, Why)> = running
        .into_iter()
        .map(|candidate| {
            let why = score(&candidate, now, busiest, taste);
            (candidate, why)
        })
        .collect();
    // Stable, so equal scores keep the order their sources put them in.
    scored.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(slides);
    scored
}

// ---------------------------------------------------------------------------------------------------------
// The pipeline.

/// Whether the household's rules hide this title.
fn hidden(candidate: &Candidate<'_>, hide: &Hide) -> bool {
    let title = &candidate.title;
    if title.adult {
        return true;
    }
    if let (Some(floor), Some(released)) = (hide.min_year, title.released) {
        if released.year_of() < floor {
            return true;
        }
    }
    // A series' genres as TMDB names them for series as well as for films, so a rule written against either
    // name catches it.
    let mut genres = title.genres.clone();
    if candidate.key.0 == MediaType::Tv {
        for (series, films) in [(10759, [28, 12]), (10765, [878, 14]), (10768, [10752, 10752])] {
            if films.iter().any(|g| genres.contains(g)) {
                genres.push(series);
            }
        }
    }
    if genres.iter().any(|g| hide.genres.contains(g)) {
        return true;
    }
    let language = title.original_language;
    if language.is_some_and(|code| hide.languages.iter().any(|l| l.as_bytes().eq_ignore_ascii_case(&code))) {
        return true;
    }
    hide.anime && genres.contains(&ANIMATION) && language == Some(*b"ja")
}

/// How many of the library's own seeds each title is a near neighbour of, and how many seeds answered.
fn neighbourhood(indexes: &Indexes, library: &[(Key, f64, f64)]) -> (usize, HashMap<Key, usize>) {
    let held = |(media_type, id): Key| indexes.plot.labels(id, media_type).is_some();
    let mut seeds: Vec<&(Key, f64, f64)> = library.iter().filter(|(_, weight, _)| *weight >= 1.0).collect();
    // What atlas actually holds first: it answers for nothing it never indexed, and there are only eight to spend.
    seeds.sort_by(|a, b| {
        held(b.0)
            .cmp(&held(a.0))
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut keys: Vec<Key> = Vec::new();
    for seed in seeds {
        if !keys.contains(&seed.0) {
            keys.push(seed.0);
        }
    }
    keys.truncate(SEEDS);
    let mut hits: HashMap<Key, usize> = HashMap::new();
    let mut answered = 0;
    for &(media_type, id) in &keys {
        let near: Vec<Key> =
            den_index::more_like_this(Some(&indexes.plot), indexes.premise.as_ref(), id, media_type)
                .into_iter()
                .map(|n| (media_type, n))
                .filter(|key| !keys.contains(key))
                .collect();
        if !near.is_empty() {
            answered += 1;
        }
        for key in near {
            *hits.entry(key).or_insert(0) += 1;
        }
    }
    (answered, hits)
}

/// The answer to a request, given atlas's lists for it.
pub fn answer(indexes: &Indexes, request: &Request, lists: &Lists, now: f64) -> serde_json::Value {
    let known = Knowledge { indexes };
    let only = request.only();
    let slides = request.limit.unwrap_or(SLIDES).clamp(1, MAX_SLIDES);

    let library: Vec<(Key, f64, f64)> = request
        .library
        .iter()
        .filter(|e| e.weight.is_finite() && e.weight != 0.0)
        .filter_map(|e| Some(((media_type(&e.type_)?, e.id), e.weight, e.at)))
        .collect();
    let entries: Vec<(Title<'_>, f64)> =
        library.iter().map(|&(key, weight, _)| (known.title(key, None, None), weight)).collect();
    let library_unjudged = entries.iter().filter(|(title, _)| title.genres.is_empty()).count();
    let taste = taste_of(&entries);
    let (answered, hits) = neighbourhood(indexes, &library);

    // In the web app's order: what just landed, what is trending everywhere, then the client's own lists.
    let mut pool: Vec<Candidate<'_>> = Vec::new();
    let mut push = |key: Key, title, rank: Option<Placing>, arrival: Option<Placing>| {
        let redundancy = hits.get(&key).copied().unwrap_or(0) as f64 / answered.max(MIN_SEEDS) as f64;
        pool.push(Candidate { key, title, rank, arrival, redundancy: Some(redundancy) });
    };
    for list in &lists.arrivals {
        let of = list.len() as f64;
        for (at, item) in list.iter().enumerate() {
            let placing = Placing { rank: at as f64, of };
            push(item.key, known.title(item.key, None, Some(item)), None, Some(placing));
        }
    }
    let of = lists.everywhere.len() as f64;
    for (at, item) in lists.everywhere.iter().enumerate() {
        let placing = Placing { rank: at as f64, of };
        push(item.key, known.title(item.key, None, Some(item)), Some(placing), None);
    }
    for offered in &request.candidates {
        let Some(media_type) = media_type(&offered.type_) else { continue };
        let key = (media_type, offered.id);
        let rank = match (offered.rank, offered.of) {
            (Some(rank), Some(of)) => Some(Placing { rank, of }),
            _ => None,
        };
        push(key, known.title(key, Some(&offered.hint), None), rank, None);
    }

    // Only titles something is known about, when there are enough of them: an unjudged title can't be matched
    // against this library's taste or dropped for missing it, so it competes on attention alone. Judged per title,
    // not per list entry: a title one list named only by id is judged when another list described it, and keeps the
    // place the first list gave it.
    let judged: HashSet<Key> = pool.iter().filter(|c| !c.title.genres.is_empty()).map(|c| c.key).collect();
    let unjudged: HashSet<Key> = pool.iter().map(|c| c.key).filter(|key| !judged.contains(key)).collect();
    if pool.iter().filter(|c| judged.contains(&c.key)).count() >= JUDGED_ENOUGH {
        pool.retain(|c| judged.contains(&c.key));
    }

    let owned: HashSet<Key> =
        request.owned.iter().filter_map(|r| Some((media_type(&r.type_)?, r.id))).collect();
    let keep = |c: &Candidate<'_>| {
        only.is_none_or(|t| t == c.key.0) && !owned.contains(&c.key) && !hidden(c, &request.hide)
    };
    let picked = pick(pool, now, slides, keep, Some(&taste));

    let round = |x: f64| (x * 1000.0).round() / 1000.0;
    let slides: Vec<serde_json::Value> = picked
        .iter()
        .map(|(c, why)| {
            let mut slide = serde_json::json!({
                "type": type_name(c.key.0),
                "id": c.key.1,
                "why": {
                    "score": round(why.score), "fresh": round(why.fresh), "attention": round(why.attention),
                    "quality": round(why.quality), "taste": round(why.taste), "novelty": round(why.novelty),
                },
            });
            if let Some(imdb) = &c.title.imdb_id {
                slide["imdbId"] = serde_json::json!(imdb);
            }
            slide
        })
        .collect();
    serde_json::json!({
        "version": 1,
        "scorer": SCORER,
        "facts": indexes.facts.is_some(),
        "slides": slides,
        "unjudged": unjudged.len(),
        "libraryUnjudged": library_unjudged,
    })
}

/// Atlas's own lists for a request, as the web app fetched them: at most eight "new on" lists — the
/// household's picked services in their own countries, else every service this install carries — and
/// Trending Everywhere. A list that can't be had is empty; the catalog already degrades and caches.
pub async fn lists(state: &Arc<AppState>, config: &Config, request: &Request) -> Lists {
    /// A catalog to read: its id, Stremio type, country and the providers it is for.
    type Wanted = (String, &'static str, String, Vec<&'static Provider>);
    let country = config.country(None, &state.default_country);
    let mut arrivals: Vec<Wanted> = Vec::new();
    for stremio_type in ["movie", "series"] {
        for &provider in &config.providers {
            if request.services.is_empty() {
                arrivals.push((new_catalog_id(provider), stremio_type, country.clone(), vec![provider]));
                continue;
            }
            for pick in request.services.iter().filter(|s| provider.package_ids.contains(&s.id)) {
                let there = config.country(Some(&pick.country), &state.default_country);
                arrivals.push((new_catalog_id(provider), stremio_type, there, vec![provider]));
            }
        }
    }
    arrivals.truncate(ARRIVAL_LISTS);
    let everywhere: Vec<&'static str> = ["movie", "series"]
        .into_iter()
        .filter(|t| request.only().is_none_or(|only| media_type(t) == Some(only)))
        .collect();

    let mut set = tokio::task::JoinSet::new();
    let asked = arrivals.len();
    let wanted = arrivals.into_iter().chain(
        everywhere
            .into_iter()
            .map(|t| (TRENDING_ID.to_owned(), t, country.clone(), config.providers.clone())),
    );
    for (at, (id, stremio_type, country, providers)) in wanted.enumerate() {
        let state = Arc::clone(state);
        set.spawn(async move {
            let body =
                state.catalog.metas_json(&id, stremio_type, &country, &providers).await.map(|r| r.body);
            (at, stremio_type, body)
        });
    }
    let mut answers: Vec<(usize, &'static str, Vec<Listed>)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((at, stremio_type, Some(body))) = joined {
            let media_type = media_type(stremio_type).expect("a catalog type");
            answers.push((at, stremio_type, listed(&body, media_type)));
        }
    }
    answers.sort_by_key(|&(at, _, _)| at);
    let mut out = Lists::default();
    let (mut movies, mut series) = (Vec::new(), Vec::new());
    for (at, stremio_type, items) in answers {
        if at < asked {
            out.arrivals.push(items);
        } else if stremio_type == "movie" {
            movies = items;
        } else {
            series = items;
        }
    }
    // Both types interleaved, so neither buries the other.
    let (mut movies, mut series) = (movies.into_iter(), series.into_iter());
    loop {
        let (m, s) = (movies.next(), series.next());
        if m.is_none() && s.is_none() {
            break;
        }
        out.everywhere.extend(m.into_iter().chain(s));
    }
    out
}

/// The days-since-epoch of a calendar date, for tests elsewhere.
#[cfg(test)]
pub fn day(year: i64, month: u32, day: u32) -> f64 {
    crate::facts::days_from_civil(year, month, day) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRIME: u16 = 80;
    const HORROR: u16 = 27;
    const DRAMA: u16 = 18;
    const MYSTERY: u16 = 9648;
    const ACTION: u16 = 28;

    fn now() -> f64 {
        day(2026, 9, 12)
    }

    fn on(date: &str) -> Option<Released> {
        Released::parse(date, "day")
    }

    fn lang(code: &str) -> Vec<[u8; 2]> {
        let b = code.as_bytes();
        vec![[b[0], b[1]]]
    }

    fn film(genres: &[u16], language: &str) -> Title<'static> {
        Title {
            genres: genres.to_vec(),
            languages: if language.is_empty() { Vec::new() } else { lang(language) },
            ..Title::default()
        }
    }

    fn cand(id: u32, title: Title<'static>) -> Candidate<'static> {
        Candidate { key: (MediaType::Movie, id), title, rank: None, arrival: None, redundancy: None }
    }

    fn ids(picked: &[(Candidate<'_>, Why)]) -> Vec<u32> {
        picked.iter().map(|(c, _)| c.key.1).collect()
    }

    fn all(_: &Candidate<'_>) -> bool {
        true
    }

    #[test]
    fn freshness_peaks_on_release_day_and_falls_away_faster_before_than_after() {
        let fresh = |date| freshness(&Title { released: on(date), ..Title::default() }, now());
        assert!((fresh("2026-09-12") - 1.0).abs() < 1e-6);
        assert!(fresh("2026-09-20") > 0.8);
        assert!((fresh("2026-09-10") - 0.983).abs() < 0.005);
        assert!((fresh("2026-12-01") - 0.169).abs() < 0.001);
        assert!(fresh("2024-01-01") < 0.01);
    }

    #[test]
    fn a_title_known_only_by_its_year_scores_the_mean_of_that_year_and_an_undated_one_as_old() {
        let year = freshness(&Title { released: Some(Released::year(2026)), ..Title::default() }, now());
        let days = (0..365).map(|i| {
            let d = day(2026, 1, 1) + f64::from(i);
            freshness(
                &Title { released: Some(Released { first_day: d as i64, span_days: 1 }), ..Title::default() },
                now(),
            )
        });
        assert!((year - days.sum::<f64>() / 365.0).abs() < 1e-9);
        assert!(year < freshness(&Title { released: on("2026-07-01"), ..Title::default() }, now()));
        assert_eq!(freshness(&Title::default(), now()), 0.0);
    }

    #[test]
    fn buzz_reads_a_ranked_place_or_popularity_against_the_busiest() {
        let ranked = |rank, of| Candidate { rank: Some(Placing { rank, of }), ..cand(1, Title::default()) };
        assert_eq!(buzz(&ranked(0.0, 10.0), 0.0), 1.0);
        assert_eq!(buzz(&ranked(5.0, 10.0), 0.0), 0.5);
        assert_eq!(buzz(&cand(1, Title::default()), 0.0), 0.0);
        let popular = |p| cand(1, Title { popularity: Some(p), ..Title::default() });
        assert!((buzz(&popular(500.0), 500.0) - 1.0).abs() < 1e-6);
        assert_eq!(buzz(&popular(0.0), 500.0), 0.0);
        assert!(buzz(&popular(50.0), 500.0) > 0.6);
    }

    #[test]
    fn among_new_titles_the_one_people_are_watching_leads() {
        let new = |popularity| Title {
            released: on("2026-09-05"),
            popularity: Some(popularity),
            ..Title::default()
        };
        let picked = pick(vec![cand(1, new(2.0)), cand(2, new(900.0))], now(), 40, all, None);
        assert_eq!(ids(&picked), vec![2, 1]);
    }

    #[test]
    fn a_rating_is_pulled_towards_the_prior_by_how_few_votes_it_rests_on() {
        let rated = |rating, votes| Title { rating: Some(rating), votes: Some(votes), ..Title::default() };
        assert!((quality(&rated(8.5, 12.0)) - 0.354).abs() < 0.001);
        assert!(quality(&rated(8.5, 500.0)) > 0.9);
        assert!(quality(&rated(6.0, 500.0)) < 0.1);
        assert!((quality(&Title::default()) - 0.3).abs() < 1e-6);
    }

    fn nordic_crime_with_one_horror() -> Taste<'static> {
        taste_of(&[
            (film(&[CRIME], "sv"), 1.0),
            (film(&[CRIME], "da"), 1.0),
            (film(&[CRIME], "sv"), 1.0),
            (film(&[HORROR], "en"), 0.6),
        ])
    }

    #[test]
    fn taste_marks_up_the_genres_watched_and_down_the_rest() {
        let taste = nordic_crime_with_one_horror();
        let nordic = affinity(&film(&[CRIME, MYSTERY], "sv"), Some(&taste));
        let horror = affinity(&film(&[HORROR], "en"), Some(&taste));
        assert!(nordic > horror);
        assert!(nordic > 0.5);
        assert!(horror < 0.3);
    }

    #[test]
    fn language_nudges_rather_than_decides() {
        let taste = taste_of(&[(film(&[CRIME], "en"), 9.0), (film(&[CRIME], "sv"), 1.0)]);
        let english = affinity(&film(&[CRIME], "en"), Some(&taste));
        let swedish = affinity(&film(&[CRIME], "sv"), Some(&taste));
        let wrong = affinity(&film(&[HORROR], "en"), Some(&taste));
        assert!(english - swedish <= 0.1);
        assert!(english - wrong > 3.0 * (english - swedish));
    }

    #[test]
    fn no_profile_is_no_taste() {
        assert_eq!(affinity(&film(&[CRIME], ""), None), 0.0);
        assert_eq!(affinity(&film(&[CRIME], ""), Some(&taste_of(&[]))), 0.0);
    }

    #[test]
    fn taste_beats_a_title_that_tops_every_list_since_it_multiplies() {
        let everywhere = Candidate {
            rank: Some(Placing { rank: 0.0, of: 100.0 }),
            arrival: Some(Placing { rank: 0.0, of: 100.0 }),
            ..cand(30, Title { released: on("2026-09-01"), popularity: Some(900.0), ..film(&[ACTION], "en") })
        };
        let on_taste = Candidate {
            rank: Some(Placing { rank: 20.0, of: 100.0 }),
            ..cand(31, Title { released: on("2026-09-01"), ..film(&[CRIME], "sv") })
        };
        let taste = taste_of(&[(film(&[CRIME], "sv"), 1.0), (film(&[ACTION], "en"), 0.2)]);
        assert_eq!(ids(&pick(vec![everywhere, on_taste], now(), 40, all, Some(&taste))), vec![31, 30]);
    }

    #[test]
    fn the_on_taste_title_beats_an_equally_new_louder_one() {
        let on_taste =
            cand(20, Title { released: on("2026-09-05"), popularity: Some(40.0), ..film(&[CRIME], "sv") });
        let loud =
            cand(21, Title { released: on("2026-09-05"), popularity: Some(400.0), ..film(&[HORROR], "en") });
        let taste = nordic_crime_with_one_horror();
        assert_eq!(ids(&pick(vec![loud, on_taste], now(), 40, all, Some(&taste))), vec![20, 21]);
    }

    #[test]
    fn a_real_match_beats_a_title_that_merely_carries_the_commonest_genre() {
        let taste = taste_of(&[
            (film(&[DRAMA, MYSTERY], ""), 1.0),
            (film(&[DRAMA, CRIME], ""), 1.0),
            (film(&[DRAMA, MYSTERY, CRIME], ""), 1.0),
            (film(&[DRAMA], ""), 1.0),
        ]);
        let close = affinity(&film(&[DRAMA, MYSTERY, CRIME], ""), Some(&taste));
        let drama = affinity(&film(&[DRAMA], ""), Some(&taste));
        assert!(close > drama);
        assert!(drama < 0.8);
    }

    #[test]
    fn follows_the_people_behind_what_was_watched_across_genres() {
        let by = |genres: &[u16], people: &[u32]| Title {
            people: people.iter().map(|&p| (p, 1.0)).collect(),
            ..film(genres, "")
        };
        let taste = taste_of(&[(by(&[DRAMA], &[5000, 1, 2]), 1.0), (by(&[DRAMA], &[5000, 3, 4]), 1.0)]);
        let theirs = affinity(&by(&[ACTION], &[5000, 9]), Some(&taste));
        let stranger = affinity(&by(&[ACTION], &[8, 9]), Some(&taste));
        assert!(theirs > stranger);
        assert!(theirs > 0.15);
    }

    #[test]
    fn a_cast_member_counts_for_less_the_longer_the_cast() {
        let taste = taste_of(&[(Title { people: vec![(7, 1.0)], ..film(&[DRAMA], "") }, 1.0)]);
        let lead = affinity(&Title { people: vec![(7, 1.0)], ..film(&[DRAMA], "") }, Some(&taste));
        let one_of_ten = affinity(&Title { people: vec![(7, 0.3)], ..film(&[DRAMA], "") }, Some(&taste));
        assert!(lead > one_of_ten);
    }

    #[test]
    fn reads_where_a_title_was_made() {
        let made = |countries: &[&[u8; 2]], language| Title {
            countries: countries.iter().map(|c| **c).collect(),
            ..film(&[CRIME], language)
        };
        let taste = taste_of(&[(made(&[b"SE"], "sv"), 1.0), (made(&[b"DK"], "da"), 1.0)]);
        let coproduction = affinity(&made(&[b"SE", b"GB"], "en"), Some(&taste));
        let american = affinity(&made(&[b"US"], "en"), Some(&taste));
        assert!(coproduction > american);
    }

    #[test]
    fn prefers_the_decade_this_library_watches() {
        let dated = |date| Title { released: on(date), ..film(&[DRAMA], "") };
        let taste = taste_of(&[(dated("2024-01-01"), 1.0), (dated("2022-06-01"), 1.0)]);
        assert!(affinity(&dated("2026-03-01"), Some(&taste)) > affinity(&dated("1981-03-01"), Some(&taste)));
    }

    #[test]
    fn carries_the_next_of_a_franchise_already_started() {
        let taste = taste_of(&[(Title { franchise: Some(77), ..film(&[ACTION], "") }, 1.0)]);
        let sequel = affinity(&Title { franchise: Some(77), ..film(&[DRAMA], "") }, Some(&taste));
        let unrelated = affinity(&film(&[DRAMA], ""), Some(&taste));
        assert!((sequel - (unrelated + (1.0 - unrelated) * 0.5)).abs() < 1e-6);
        assert!(sequel > 0.5);
    }

    #[test]
    fn marks_down_what_was_disliked_while_a_watched_genre_survives_one_bad_film() {
        let taste = taste_of(&[
            (film(&[DRAMA, HORROR], ""), 1.0),
            (film(&[DRAMA], ""), 1.0),
            (film(&[DRAMA], ""), 1.0),
            (film(&[DRAMA, ACTION], ""), -1.5),
        ]);
        let drama = affinity(&film(&[DRAMA], ""), Some(&taste));
        let also_action = affinity(&film(&[DRAMA, ACTION], ""), Some(&taste));
        let only_action = affinity(&film(&[ACTION], ""), Some(&taste));
        assert!(drama > 0.4);
        assert!(also_action < drama);
        assert!(only_action < also_action);
        assert!(only_action < 0.1);
    }

    #[test]
    fn labels_separate_two_titles_of_the_same_genre() {
        let labelled = |subgenres: Vec<(&'static str, f64)>, moods: Vec<(&'static str, f64)>| Title {
            labels: Some(LabelSet { subgenres, moods }),
            ..film(&[CRIME], "")
        };
        let taste = taste_of(&[
            (labelled(vec![("Police Procedural", 0.9), ("Neo-Noir", 0.8)], vec![("Slow-burn", 0.9)]), 1.0),
            (labelled(vec![("Police Procedural", 0.8)], vec![("Slow-burn", 0.8)]), 1.0),
            (labelled(vec![("Neo-Noir", 0.7)], vec![("Dark & Gritty", 0.6)]), 1.0),
        ]);
        let noir = affinity(
            &labelled(vec![("Neo-Noir", 0.9), ("Police Procedural", 0.7)], vec![("Slow-burn", 0.8)]),
            Some(&taste),
        );
        let slasher = affinity(&labelled(vec![("Slasher", 0.9)], vec![("Tense", 0.8)]), Some(&taste));
        assert!(noir > slasher);
        // A title atlas never labelled is judged on its facets alone, not marked down for the gap.
        let unlabelled = affinity(&film(&[CRIME], ""), Some(&taste));
        assert!(unlabelled > slasher && unlabelled < noir);
    }

    #[test]
    fn novelty_leaves_a_title_alone_until_the_library_reaches_it() {
        let worn = |r| Candidate { redundancy: r, ..cand(1, Title::default()) };
        assert_eq!(novelty(&worn(None)), 1.0);
        assert!((novelty(&worn(Some(0.5))) - 0.75).abs() < 1e-6);
        assert!((novelty(&worn(Some(1.0))) - 0.5).abs() < 1e-6);
        let same = || Title { released: on("2026-09-01"), popularity: Some(100.0), ..Title::default() };
        let picked = pick(
            vec![Candidate { redundancy: Some(1.0), ..cand(1, same()) }, cand(2, same())],
            now(),
            40,
            all,
            None,
        );
        assert_eq!(ids(&picked), vec![2, 1]);
    }

    #[test]
    fn an_arrival_lifts_an_old_title_and_a_pushed_release_counts_once() {
        let landed = Candidate {
            arrival: Some(Placing { rank: 0.0, of: 10.0 }),
            ..cand(1, Title { released: on("1997-06-01"), ..Title::default() })
        };
        let merely_new = cand(2, Title { released: on("2026-08-20"), ..Title::default() });
        assert_eq!(ids(&pick(vec![merely_new, landed], now(), 40, all, None)), vec![1, 2]);
        let pushed = Candidate {
            rank: Some(Placing { rank: 0.0, of: 100.0 }),
            arrival: Some(Placing { rank: 0.0, of: 100.0 }),
            ..cand(1, Title::default())
        };
        assert!((attention(&pushed, 0.0) - 1.3).abs() < 1e-6);
        assert_eq!(
            arrival(&Candidate {
                arrival: Some(Placing { rank: 0.0, of: 0.0 }),
                ..cand(4, Title::default())
            }),
            0.0
        );
    }

    #[test]
    fn leads_with_what_is_new_rather_than_what_is_merely_well_liked() {
        let classic = cand(
            1,
            Title {
                released: on("1994-09-23"),
                rating: Some(9.3),
                votes: Some(20_000.0),
                ..Title::default()
            },
        );
        let new = cand(
            2,
            Title { released: on("2026-09-01"), rating: Some(6.4), votes: Some(200.0), ..Title::default() },
        );
        assert_eq!(ids(&pick(vec![classic, new], now(), 40, all, None)), vec![2, 1]);
    }

    #[test]
    fn merges_what_two_sources_offered_and_takes_the_better_placing() {
        let taste = taste_of(&[(film(&[CRIME], "sv"), 1.0)]);
        let ranked = Candidate { rank: Some(Placing { rank: 0.0, of: 10.0 }), ..cand(5, Title::default()) };
        let described = cand(
            5,
            Title {
                released: on("2026-09-01"),
                rating: Some(8.0),
                votes: Some(120.0),
                ..film(&[CRIME], "sv")
            },
        );
        let alone = cand(
            6,
            Title {
                released: on("2026-09-01"),
                rating: Some(8.0),
                votes: Some(120.0),
                ..film(&[CRIME], "sv")
            },
        );
        assert_eq!(ids(&pick(vec![ranked, described, alone], now(), 40, all, Some(&taste))), vec![5, 6]);

        let taste = taste_of(&[(film(&[CRIME], ""), 1.0)]);
        let arriving = |id, rank| Candidate {
            arrival: Some(Placing { rank, of: 100.0 }),
            ..cand(id, film(&[CRIME], ""))
        };
        let picked =
            pick(vec![arriving(5, 60.0), arriving(5, 0.0), arriving(6, 30.0)], now(), 40, all, Some(&taste));
        assert_eq!(ids(&picked), vec![5, 6]);
    }

    #[test]
    fn a_merge_keeps_the_exact_date_and_the_counted_rating_whichever_came_first() {
        let listed = cand(
            5,
            Title {
                released: Some(Released::year(2026)),
                rating: Some(9.0),
                votes: Some(RATING_PRIOR_VOTES),
                estimated_votes: true,
                ..Title::default()
            },
        );
        let described = cand(
            5,
            Title { released: on("2026-09-10"), rating: Some(6.1), votes: Some(40.0), ..Title::default() },
        );
        let (merged, _) = pick(vec![listed, described], now(), 40, all, None).remove(0);
        assert_eq!(merged.title.released, on("2026-09-10"));
        assert_eq!((merged.title.rating, merged.title.votes), (Some(6.1), Some(40.0)));
        assert!(!merged.title.estimated_votes);
    }

    #[test]
    fn drops_a_stranger_only_when_its_genres_are_known() {
        let taste = taste_of(&[(film(&[CRIME], "sv"), 3.0), (film(&[ACTION], "en"), 0.3)]);
        let new = |genres: &[u16], language| Title { released: on("2026-09-01"), ..film(genres, language) };
        let alien = cand(2, new(&[16], "ja"));
        let barely = cand(4, new(&[ACTION], "en"));
        let unknown = cand(3, new(&[], ""));
        assert_eq!(ids(&pick(vec![alien.clone(), barely, unknown], now(), 40, all, Some(&taste))), vec![3]);
        assert_eq!(ids(&pick(vec![alien], now(), 40, all, None)), vec![2]);
    }

    #[test]
    fn filters_before_cutting_to_the_slide_count() {
        let pool: Vec<Candidate<'static>> =
            (0..60).map(|i| cand(i, Title { released: on("2026-08-01"), ..Title::default() })).collect();
        let picked = pick(pool, now(), 20, |c| c.key.1 % 2 == 0, None);
        assert_eq!(picked.len(), 20);
        assert!(picked.iter().all(|(c, _)| c.key.1 % 2 == 0));
    }

    #[test]
    fn hides_by_the_households_rules() {
        let series = |title| Candidate { key: (MediaType::Tv, 1), ..cand(1, title) };
        let rules =
            Hide { min_year: Some(1990), genres: vec![10765], languages: vec!["HI".into()], anime: true };
        assert!(hidden(&cand(1, Title { released: on("1985-01-01"), ..Title::default() }), &rules));
        assert!(hidden(&series(film(&[878], "")), &rules), "a series rule catches the film name");
        assert!(!hidden(&cand(1, film(&[878], "")), &rules), "…but not a film");
        let hindi = Title { original_language: Some(*b"hi"), ..Title::default() };
        assert!(hidden(&cand(1, hindi), &rules));
        let anime = Title { original_language: Some(*b"ja"), ..film(&[ANIMATION], "") };
        assert!(hidden(&cand(1, anime), &rules));
        assert!(hidden(&cand(1, Title { adult: true, ..Title::default() }), &Hide::default()));
        assert!(!hidden(&cand(1, Title::default()), &rules));
    }

    #[test]
    fn reads_a_catalog_body_and_a_clock() {
        let body = r#"{"metas":[{"id":"tt1","imdb_id":"tt1","type":"movie","name":"A","moviedb_id":42,
                     "imdbRating":"7.4","releaseInfo":"1999"},{"id":"tt2","imdb_id":"tt2","name":"B"}]}"#;
        assert_eq!(
            listed(body, MediaType::Movie),
            vec![Listed {
                key: (MediaType::Movie, 42),
                imdb_id: Some("tt1".into()),
                rating: Some(7.4),
                year: Some(1999)
            }]
        );
        assert_eq!(parse_now("2026-09-12T12:00:00Z"), Some(day(2026, 9, 12) + 0.5));
        let half_second = parse_now("2026-09-12T12:00:00.500Z").unwrap() - (day(2026, 9, 12) + 0.5);
        assert!((half_second * 86_400.0 - 0.5).abs() < 1e-3);
        assert_eq!(parse_now("2026-09-12 12:00"), None);
        assert_eq!(fold_genre(10759), &[28, 12]);
        assert_eq!(fold_genre(80), &[80]);
        assert!(fold_genre(1).is_empty());
    }
}
