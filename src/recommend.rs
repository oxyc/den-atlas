//! `POST /recommend` — the titles a featured surface leads with (the web's billboard, and later the TV's
//! heroes), ranked here so that no client ranks.
//!
//! A port of the web app's `billboard.ts` and of the pipeline in `Library.svelte` that fed it. Where the web
//! asked TMDB what a title is, this reads what atlas already holds: the dataset's labels (primary genre,
//! animation, subgenres, moods), `facets.bin` (country, original language, year) and, once the dataset
//! publishes them, the Wikidata facts (dates with their precision, genres, makers, cast, franchise). What a
//! client got from TMDB lists it already fetched for its own rows — release date, genres, popularity and rating —
//! arrives as a per-candidate hint and fills only what atlas doesn't know. Hints live for this request alone: no
//! TMDB rating is copied into Atlas's catalogs, dataset or kept replay fixtures.
//!
//! What a billboard shows is not the leading row. A "Because you watched X" row is the nearest neighbours of
//! a title already watched, and after enough history that neighbourhood IS the history. A billboard is the
//! top of a page opened to find something to watch, so this ranks what fits this household (`fit.rs`) and, among
//! what fits, what is good and new — new in the world, or newly on a service the household has.
//!
//! Pure and deterministic given `now`, so a recorded request replays to the same answer.

use crate::catalog::{new_catalog_id, provider_by_code, Provider, TRENDING_ID};
use crate::config::Config;
use crate::facts::{Facts, Released};
use crate::fit::{Features, Fit, FitReason, Fitted};
use crate::queries::Indexes;
use crate::AppState;
use den_index::MediaType;
use den_titlesearch::TitleIndex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Names the scoring rules, so a kept answer can say which rules chose it.
pub const SCORER: &str = "fit-2";

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
/// Votes a counted rating needs before it replaces an upstream catalog score. A 7.2 on 18 votes must not read
/// over an established IMDb score and put a poorly received release at the top of a billboard.
const COUNTED_ENOUGH: f64 = 100.0;
/// Days after its release beyond which a title arriving on a service is catalogue joining it, not something new.
const CATALOGUE_DAYS: f64 = 730.0;
/// What an arrival is worth when it is catalogue: Interstellar on "New on Prime" is a title the household has had
/// every chance to meet.
const CATALOGUE_ARRIVAL: f64 = 0.25;
/// What merit, timeliness and buzz can each do to a score. A title with none of one keeps this much of it, so fit —
/// squared — decides, and the rest choose between titles that fit alike.
///
/// Merit's floor was 0.5, so the worst-received title kept half its score and a poorly rated release that fit well
/// led a billboard (four of a horror household's first ten had quality under 0.2). At 0.2 a title nobody liked costs
/// as much as one nobody is talking about: 5×, the same span as timeliness.
const MERIT_FLOOR: f64 = 0.2;
const TIMELY_FLOOR: f64 = 0.2;
const BUZZ_BONUS: f64 = 0.1;
/// The slides that must fit at least `LEAD_FIT`: a billboard opens on what this household might want, never on a
/// title only everyone else is watching.
const LEAD_SLIDES: usize = 10;
const LEAD_FIT: f64 = 0.25;
/// A billboard is assembled `LEAD_SLIDES` at a time, each ten as a set rather than a cut of one score order
/// (`assemble`): a pure score sort gave an animation household 39 films in 40 slides, and a crime household six old
/// titles in its first ten, because fit² spans ~35× across a pool and nothing else in the score can outweigh it.
///
/// A slide is stale — catalogue, neither new nor newly arrived — when it came out over a year ago (freshness 0.05 is
/// ~360 days after release) and no "new on" list places it high. At most `STALE_SLIDES` of each ten are: the
/// household's old favourites still get a place, without the billboard turning into its back catalogue.
pub const STALE_FRESH: f64 = 0.05;
pub const STALE_ARRIVAL: f64 = 0.3;
const STALE_SLIDES: usize = 3;
/// `quality` under this is a settled rating under 6.4: the band a horror household's poorly received new releases sat
/// in. At most `POOR_SLIDES` of each ten are, so no rule fills a ten with them either.
pub const POOR_QUALITY: f64 = 0.2;
const POOR_SLIDES: usize = 1;
/// On a surface of both types, each ten holds at least this many of each while the pool has them to give.
const EACH_TYPE_SLIDES: usize = 3;
/// Two slides whose plot vectors are at least this close are one interest twice — a sequel, a spin-off, the same
/// premise again: Dune and Dune: Part Two 0.83, Bosch and Bosch: Legacy 0.77, Friends and How I Met Your Mother 0.69.
/// Measured over the made-up households' own top 40 (`billboard-check`), which already all suit one taste: the
/// median pair sits at 0.56–0.60 and this is about the closest 3%. Each ten holds one slide of an interest.
pub const SAME_INTEREST: f64 = 0.68;
/// What the mix, catalogue and interest rules may put ahead of a better-scoring slide: one that fits at least
/// `RULE_FIT` — fit's midpoint, the index's top 2% for this household — and scores at least 1 / `RULE_COST` of the
/// best slide passed over. Each bound alone let a bad slide through. Without the fit bound the rules filled an
/// animation household's last places with live-action titles that barely fit (Lanterns, fit 0.26), because the
/// catalogue they displaced already carried timeliness's 5×; without the score bound, a comedy household's with
/// three well-fitting releases nobody liked (quality 0, a sixth of the score they displaced).
const RULE_FIT: f64 = 0.5;
const RULE_COST: f64 = 3.0;
/// A title's fit while the library likes nothing yet.
const NO_TASTE: f64 = 0.5;
/// Cast members that count as much as a maker. Wikidata's cast is unordered and about ten deep where the web
/// read the top three billed, so each cast member counts `3 / cast size`, at most one.
const CAST_BILLED: f64 = 3.0;
/// Judged candidates enough to drop the unjudged ones.
const JUDGED_ENOUGH: usize = 20;
/// Unjudged titles an answer names for the client to describe, best first. Until the facts cover what is new
/// on the services, a title atlas has never seen is otherwise dropped however much attention it has.
const UNJUDGED_NAMED: usize = 20;
/// Slides by default, and at most.
const SLIDES: usize = 40;
const MAX_SLIDES: usize = 100;
/// Service lists read for one request, "new on" before "popular on": every household service in each type, and
/// then some.
const SERVICE_LISTS: usize = 48;
/// The household's own pool (`personal`): titles from the whole catalogue out within this many days, or due within
/// this many, since freshness has all but gone by then (`FRESH_DAYS`, `ANTICIPATION_DAYS`) …
const PERSONAL_BEHIND: i64 = 2 * FRESH_DAYS as i64;
const PERSONAL_AHEAD: i64 = 2 * ANTICIPATION_DAYS as i64;
/// … narrowed to this many on a sketch of their fit, and to this many of the best fits after the whole of it.
const PERSONAL_SKETCHED: usize = 400;
const PERSONAL: usize = 60;
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
    /// A service channel: the surface of one service in one country, whose slides are only titles on it
    /// (`on_service`). `surface` still says which types it shows.
    #[serde(default)]
    pub service: Option<ServicePick>,
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
    /// ISO 3166-1 alpha-2; absent or unreadable, the install's country (`Config::country`).
    #[serde(default)]
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
    /// What the client knows about the title, for a library title atlas holds nothing on.
    #[serde(default)]
    pub hint: Hint,
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
    /// ISO 3166-1 alpha-2.
    pub countries: Option<Vec<String>>,
    pub popularity: Option<f64>,
    /// A transient TMDB score supplied by the caller. Never written into an Atlas artifact or response.
    pub rating: Option<f64>,
    /// The TMDB vote count behind `rating`, under the same transient boundary.
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
pub(crate) fn genre_named(name: &str) -> Option<u16> {
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
    /// TMDB's popularity: a client hint's, else TMDB's daily export (`attend`). Both are ~30-day activity scores.
    pub popularity: Option<f64>,
    /// Whether `popularity` is TMDB's all-time vote count on the export's scale (`search::VOTES_PER_POPULARITY`), for a
    /// title the export doesn't hold. It says how widely a title was seen, never that it is popular now, so it
    /// counts towards buzz but is never given as the reason.
    pub popularity_from_votes: bool,
    /// TMDB genre ids, series' own folded into films' (`fold_genre`).
    pub genres: Vec<u16>,
    /// Its original language, which the hide rules read.
    pub original_language: Option<[u8; 2]>,
    pub languages: Vec<[u8; 2]>,
    pub countries: Vec<[u8; 2]>,
    /// Who made it and who is in it, each with how much it counts (see `CAST_BILLED`).
    pub people: Vec<(u32, f64)>,
    /// Every franchise series it is in, most specific first.
    pub franchise: Vec<u32>,
    /// Where a series aired.
    pub broadcasters: Vec<u32>,
    pub labels: Option<LabelSet<'a>>,
    /// Every genre a client's hint named, folded like `genres`. Taste may read only the first of them; the hide
    /// rules read them all, so a hidden genre named second still hides the title.
    pub hint_genres: Vec<u16>,
    pub adult: bool,
    pub imdb_id: Option<String>,
}

/// A title from one of Atlas's own upstream lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub key: Key,
    pub imdb_id: Option<String>,
    /// The upstream catalog's score; JustWatch supplies its IMDb score here.
    pub rating: Option<f64>,
    pub year: Option<i64>,
}

/// Atlas's lists for one request: each "new on <service>" list, Trending Everywhere with both types interleaved,
/// each "popular on <service>" list, and each service's own Top 10 (`charts`, a ranking).
#[derive(Default)]
pub struct Lists {
    pub arrivals: Vec<Vec<Listed>>,
    pub everywhere: Vec<Listed>,
    pub popular: Vec<Vec<Listed>>,
    pub charts: Vec<Vec<Listed>>,
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

fn hinted_rating(hint: Option<&Hint>) -> Option<(f64, Option<f64>)> {
    hint.and_then(|h| {
        h.rating.filter(|rating| rating.is_finite() && *rating > 0.0 && *rating <= 10.0).map(|rating| {
            let votes = h.votes.filter(|votes| {
                votes.is_finite()
                    && *votes >= 0.0
                    && votes.fract() == 0.0
                    && *votes <= 9_007_199_254_740_991.0
            });
            (rating, votes)
        })
    })
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

        // Every genre anything named, for the hide rules.
        let mut named: Vec<u16> = record.map(|r| r.genres.clone()).unwrap_or_default();
        let from_facts = named.len();
        for &genre in hint.and_then(|h| h.genre_ids.as_deref()).unwrap_or_default() {
            for &folded in fold_genre(genre) {
                if !named.contains(&folded) {
                    named.push(folded);
                }
            }
        }
        // Taste reads one genre a title until the facts describe most of what atlas holds. The labels name one,
        // and a title the facts or a client's list described with three would otherwise be favoured over the
        // rest for it, whatever it is. Until then the facts are a delta for titles the corpus lacks.
        let rich = self.facts().is_some_and(|f| f.len() * 2 >= self.indexes.plot.len());
        let mut genres: Vec<u16> = Vec::new();
        if let Some(labels) = &labels {
            genres.extend(genre_named(labels.primary_genre));
            if labels.animated && !genres.contains(&ANIMATION) {
                genres.push(ANIMATION);
            }
            if rich {
                genres.extend(named[..from_facts].iter().filter(|g| !genres.contains(g)).collect::<Vec<_>>());
            }
        } else {
            // The facts' genres before a list's: a record says what the title is, a list only what it was filed as.
            let source = if from_facts > 0 { &named[..from_facts] } else { &named[..] };
            genres = source.iter().take(if rich { source.len() } else { 1 }).copied().collect();
        }
        title.genres = genres;
        title.hint_genres = named;

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
        title.countries = match (record, facets.and_then(|f| f.country)) {
            (Some(r), _) if !r.countries.is_empty() => r.countries.clone(),
            (_, Some(country)) => vec![country],
            _ => hint
                .and_then(|h| h.countries.as_deref())
                .unwrap_or_default()
                .iter()
                .filter_map(|c| match c.as_bytes() {
                    [a, b] if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => {
                        Some([a.to_ascii_uppercase(), b.to_ascii_uppercase()])
                    }
                    _ => None,
                })
                .collect(),
        };
        if let Some(r) = record {
            title.people = r.makers.iter().map(|&id| (id, 1.0)).collect();
            let each = if r.cast.is_empty() { 0.0 } else { (CAST_BILLED / r.cast.len() as f64).min(1.0) };
            title.people.extend(r.cast.iter().filter(|id| !r.makers.contains(*id)).map(|&id| (id, each)));
            // Every series: The Batman and The Hobbit match a followed franchise only through their second.
            title.franchise = r.franchise.clone();
            title.broadcasters = r.broadcasters.clone();
        }
        title.released = hint
            .and_then(|h| h.release_date.as_deref())
            .and_then(|date| Released::parse(date, "day"))
            .or_else(|| record.and_then(|r| r.released))
            .or_else(|| facets.and_then(|f| f.year).map(|y| Released::year(i64::from(y))))
            .or_else(|| listed.and_then(|l| l.year).map(Released::year));
        // TMDB's own score and count for this title, kept on the box and joined onto the store (`ratings`,
        // `tmdb`): a real rating for 99.9% of the corpus, where the branches below used to depend on some
        // upstream list having named the title at all. Asked for only where it is needed — this runs once per
        // candidate, and the first arm below has a counted rating already.
        let kept = || self.indexes.rating(media_type, id);
        let hinted = hinted_rating(hint);
        match (hinted, listed.and_then(|l| l.rating)) {
            // A transient TMDB score replaces an upstream score only when enough votes stand behind it.
            (Some((rating, votes)), listed_rating) if listed_rating.is_none() || counted(votes) => {
                title.rating = Some(rating);
                title.votes = votes;
            }
            // JustWatch gives IMDb's score without its vote count. The facets' count for the title, where
            // they hold one, says how far it stands, then TMDB's kept one; without either it is trusted as far
            // as the prior's own weight, and `estimated_votes` says the number is a stand-in.
            (_, Some(rating)) => {
                title.rating = Some(rating);
                let stands = facets
                    .map(|f| f.votes)
                    .filter(|&votes| votes > 0)
                    .map(f64::from)
                    .or_else(|| kept().map(|(votes, _)| f64::from(votes)));
                match stands {
                    Some(votes) => title.votes = Some(votes),
                    None => {
                        title.votes = Some(RATING_PRIOR_VOTES);
                        title.estimated_votes = true;
                    }
                }
            }
            // Nothing upstream scored it. Its rating stayed `None`, and `quality` then read RATING_PRIOR
            // for it — the same 6.6 for every title no list happened to name, which is a guess dressed as
            // a score. TMDB's is a real one, on a real count, so nothing here is estimated.
            _ => {
                if let Some((votes, rating)) = kept() {
                    title.rating = Some(f64::from(rating));
                    title.votes = Some(f64::from(votes));
                }
            }
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

/// Popularity for a title no client hint gave one. Hints describe only what a client's own lists fetched, so without
/// this five to seven of every ten slides scored no buzz at all — every title from atlas's lists and the personal pool.
///
/// TMDB's daily export first: the same ~30-day activity score a hint carries, so the two share `buzz`'s scale. Else
/// TMDB's kept vote count, on that scale by the factor search measured (`search::VOTES_PER_POPULARITY`) and never above
/// what search calls fully popular (`search::POPULAR_VOTES`): an all-time count otherwise puts a decades-old classic
/// far above anything anyone is watching this month, and flattens every other title's buzz under it.
pub fn attend(indexes: &Indexes, export: Option<&TitleIndex>, candidate: &mut Candidate<'_>) {
    let title = &mut candidate.title;
    if title.popularity.is_some() {
        return;
    }
    let (media_type, id) = candidate.key;
    let kind = match media_type {
        MediaType::Movie => den_titlesearch::MediaType::Movie,
        MediaType::Tv => den_titlesearch::MediaType::Tv,
    };
    if let Some(popularity) = export.and_then(|e| e.popularity_of(kind, id)) {
        title.popularity = Some(popularity);
    } else if let Some((votes, _)) = indexes.rating(media_type, id) {
        let votes = f64::from(votes).min(crate::search::POPULAR_VOTES);
        title.popularity = Some(votes / crate::search::VOTES_PER_POPULARITY);
        title.popularity_from_votes = true;
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

/// Newly watchable on a service this household has — half as much when it is catalogue (`CATALOGUE_DAYS`). A title
/// of unknown date counts in full: nothing says it is old.
pub fn arrival(candidate: &Candidate<'_>, now: f64) -> f64 {
    let catalogue = candidate.title.released.is_some_and(|r| now - r.first_day as f64 > CATALOGUE_DAYS);
    standing(candidate.arrival) * if catalogue { CATALOGUE_ARRIVAL } else { 1.0 }
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
    pub fit: Fitted,
    pub fresh: f64,
    pub arrived: f64,
    pub quality: f64,
    pub buzz: f64,
    pub score: f64,
    pub reason: Option<Reason>,
}

/// The strongest meaningful contribution to a title's score, already compared on the scorer's scale. Clients map
/// this stable code to their own short copy instead of comparing the diagnostic terms, which mix z-scores and 0…1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Similar,
    Profile,
    People,
    Franchise,
    Arrived,
    Recent,
    Upcoming,
    Timely,
    Quality,
    Buzz,
}

impl Reason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Similar => "similar",
            Self::Profile => "profile",
            Self::People => "people",
            Self::Franchise => "franchise",
            Self::Arrived => "arrived",
            Self::Recent => "recent",
            Self::Upcoming => "upcoming",
            Self::Timely => "timely",
            Self::Quality => "quality",
            Self::Buzz => "buzz",
        }
    }
}

/// Below this log-score lift, a reason is technically true but too slight to put in front of a viewer.
const REASON_LIFT: f64 = 0.05;

/// A title's score: its fit squared, so fit decides, then merit, timeliness — new, or newly on a household service —
/// and buzz. Across a real household's pool fit² spans about 35×, merit and timeliness 5× each and buzz 1.1×; what
/// fit alone can't balance, `assemble` does.
pub fn score(candidate: &Candidate<'_>, now: f64, busiest: f64, fit: Fitted) -> Why {
    let fresh = freshness(&candidate.title, now);
    let arrived = arrival(candidate, now);
    let quality = quality(&candidate.title);
    let buzz = buzz(candidate, busiest);
    let score = fit.fit
        * fit.fit
        * (MERIT_FLOOR + (1.0 - MERIT_FLOOR) * quality)
        * (TIMELY_FLOOR + (1.0 - TIMELY_FLOOR) * fresh.max(arrived))
        * (1.0 + BUZZ_BONUS * buzz);
    // Compare the multiplicative parts by their log lift over each factor's floor. Household fit reports the same
    // unit from `fit.rs`, so a z-score can never beat a 0…1 term merely because its raw number is larger.
    let mut strongest = fit.reason.map(|reason| {
        let reason = match reason {
            FitReason::Similar => Reason::Similar,
            FitReason::Profile => Reason::Profile,
            FitReason::People => Reason::People,
            FitReason::Franchise => Reason::Franchise,
        };
        (reason, fit.reason_lift)
    });
    let mut consider = |reason, lift| {
        if lift >= REASON_LIFT && strongest.is_none_or(|(_, held)| lift > held) {
            strongest = Some((reason, lift));
        }
    };
    if quality >= 0.65 && candidate.title.rating.is_some() {
        consider(Reason::Quality, ((MERIT_FLOOR + (1.0 - MERIT_FLOOR) * quality) / MERIT_FLOOR).ln());
    }
    let timely = fresh.max(arrived);
    if timely > 0.0 {
        let reason = if arrived >= fresh {
            Reason::Arrived
        } else {
            candidate.title.released.map_or(Reason::Timely, |released| {
                if released.first_day as f64 > now {
                    Reason::Upcoming
                } else if (released.first_day + released.span_days.max(1) - 1) as f64 <= now {
                    Reason::Recent
                } else {
                    Reason::Timely
                }
            })
        };
        consider(reason, ((TIMELY_FLOOR + (1.0 - TIMELY_FLOOR) * timely) / TIMELY_FLOOR).ln());
    }
    // "Popular now" only when a ranked list or a current activity score says so, not an all-time vote count.
    let now_popular = standing(candidate.rank) >= buzz || !candidate.title.popularity_from_votes;
    if buzz > 0.0 && now_popular {
        consider(Reason::Buzz, (1.0 + BUZZ_BONUS * buzz).ln());
    }
    let reason = strongest.filter(|(_, lift)| *lift >= REASON_LIFT).map(|(reason, _)| reason);
    Why { fit, fresh, arrived, quality, buzz, score, reason }
}

/// Whether a vote count is enough for its rating to replace JustWatch's IMDb score.
fn counted(votes: Option<f64>) -> bool {
    votes.is_some_and(|votes| votes >= COUNTED_ENOUGH)
}

/// How far a title's rating can be taken at its word, to choose between two sources' ratings: one counted on
/// enough votes, then JustWatch's IMDb score, then a count too small to go by, then none.
fn rating_standing(title: &Title<'_>) -> u8 {
    match (title.rating, title.estimated_votes) {
        (None, _) => 0,
        (Some(_), true) => 2,
        (Some(_), false) if counted(title.votes) => 3,
        (Some(_), false) => 1,
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
/// and may name a title by its year and a vote-less score, so a client candidate's exact date and counted rating
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
    // A current activity score over a count converted from all-time votes, whichever copy holds it.
    let (popularity, popularity_from_votes) = match (x.popularity, y.popularity) {
        (Some(_), Some(p)) if x.popularity_from_votes && !y.popularity_from_votes => (Some(p), false),
        (Some(p), _) => (Some(p), x.popularity_from_votes),
        (None, p) => (p, y.popularity_from_votes),
    };
    let second_rating = rating_standing(&y) > rating_standing(&x);
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
            popularity,
            popularity_from_votes,
            genres: either(x.genres, y.genres),
            original_language: x.original_language.or(y.original_language),
            languages: either(x.languages, y.languages),
            countries: either(x.countries, y.countries),
            people: either(x.people, y.people),
            franchise: either(x.franchise, y.franchise),
            broadcasters: either(x.broadcasters, y.broadcasters),
            labels: x.labels.or(y.labels),
            hint_genres: either(x.hint_genres, y.hint_genres),
            adult: x.adult || y.adult,
            imdb_id: x.imdb_id.or(y.imdb_id),
        },
        rank: best(a.rank, b.rank),
        arrival: best(a.arrival, b.arrival),
    }
}

/// The slides, best first: deduped, filtered, scored, and only then assembled into `slides` (`assemble`). `near` says
/// whether two titles are one interest (`SAME_INTEREST`).
pub fn pick<'a>(
    candidates: Vec<Candidate<'a>>,
    now: f64,
    slides: usize,
    keep: impl Fn(&Candidate<'a>) -> bool,
    fit: impl Fn(&Candidate<'a>) -> Fitted,
    near: impl Fn(&Candidate<'a>, &Candidate<'a>) -> bool,
) -> Vec<(Candidate<'a>, Why)> {
    let mut order: Vec<Key> = Vec::new();
    let mut by_key: HashMap<Key, Candidate<'a>> = HashMap::new();
    for candidate in candidates {
        let merged = match by_key.remove(&candidate.key) {
            Some(already) => merge(already, candidate),
            None => {
                order.push(candidate.key);
                candidate
            }
        };
        by_key.insert(merged.key, merged);
    }
    // Judged once everything known about a title is together: filtering each copy first let a copy that knew
    // nothing — atlas's own lists name a title by id — through a rule its described copy was dropped by.
    let running: Vec<Candidate<'a>> =
        order.into_iter().filter_map(|key| by_key.remove(&key)).filter(|c| keep(c)).collect();
    let busiest = running.iter().filter_map(|c| c.title.popularity).fold(0.0, f64::max);
    let mut scored: Vec<(Candidate<'a>, Why)> = running
        .into_iter()
        .map(|candidate| {
            let why = score(&candidate, now, busiest, fit(&candidate));
            (candidate, why)
        })
        .collect();
    // Stable, so equal scores keep the order their sources put them in.
    scored.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));
    assemble(scored, slides, near)
}

/// Whether a slide is catalogue: neither new nor newly arrived (`STALE_FRESH`, `STALE_ARRIVAL`).
pub fn stale(why: &Why) -> bool {
    why.fresh < STALE_FRESH && why.arrived < STALE_ARRIVAL
}

/// `scored`, best first, as slides: `LEAD_SLIDES` at a time, each ten filled place by place with the best slide that
/// keeps it a set —
/// 1. the first ten fit at least `LEAD_FIT`;
/// 2. at most `POOR_SLIDES` of each ten are poorly rated (`POOR_QUALITY`);
/// 3. at most `STALE_SLIDES` of each ten are catalogue;
/// 4. each ten holds `EACH_TYPE_SLIDES` films and series, while the rest of the pool can give them;
/// 5. no two of each ten are one interest (`near`).
///
/// The first rule is kept whenever any slide can keep it. The others are weighed in order: a slide that keeps rule 2
/// beats any that breaks it, whatever they do for 3–5, so a pool with nothing left but catalogue fills the ten with an
/// old title people liked before a new one they didn't. And rules 2–5 pass over the best-scoring slide only for one
/// within `RULE_FIT` and `RULE_COST`, so a title that barely fits, or that nobody liked, never takes a place for the
/// mix's sake alone.
fn assemble<'a>(
    mut scored: Vec<(Candidate<'a>, Why)>,
    slides: usize,
    near: impl Fn(&Candidate<'a>, &Candidate<'a>) -> bool,
) -> Vec<(Candidate<'a>, Why)> {
    let mut out: Vec<(Candidate<'a>, Why)> = Vec::new();
    while out.len() < slides && !scored.is_empty() {
        let first_ten = out.len() < LEAD_SLIDES;
        let ten = &out[out.len() / LEAD_SLIDES * LEAD_SLIDES..];
        let open = LEAD_SLIDES - ten.len();
        let fits = |slide: &(Candidate<'a>, Why)| !first_ten || slide.1.fit.fit >= LEAD_FIT;
        // What each type is still short of in this ten, where the pool has one to give. Once the open places are no
        // more than the shortfall, only a type that is short may take one.
        let short: Vec<(MediaType, usize)> = [MediaType::Movie, MediaType::Tv]
            .into_iter()
            .filter(|&kind| scored.iter().any(|s| s.0.key.0 == kind && fits(s)))
            .map(|kind| {
                (kind, EACH_TYPE_SLIDES.saturating_sub(ten.iter().filter(|s| s.0.key.0 == kind).count()))
            })
            .filter(|&(_, n)| n > 0)
            .collect();
        let owed = open <= short.iter().map(|&(_, n)| n).sum();
        let typed =
            |slide: &(Candidate<'a>, Why)| !owed || short.iter().any(|&(kind, _)| slide.0.key.0 == kind);
        let poor_room = ten.iter().filter(|s| s.1.quality < POOR_QUALITY).count() < POOR_SLIDES;
        let rated = |slide: &(Candidate<'a>, Why)| poor_room || slide.1.quality >= POOR_QUALITY;
        let stale_room = ten.iter().filter(|s| stale(&s.1)).count() < STALE_SLIDES;
        let timely = |slide: &(Candidate<'a>, Why)| stale_room || !stale(&slide.1);
        let apart = |slide: &(Candidate<'a>, Why)| !ten.iter().any(|s| near(&s.0, &slide.0));
        // The best the ten could have had with only the lead's rule: the others choose over it only within bounds, and
        // then the slide that keeps the most important of them, the better score between two that keep the same.
        let at = scored.iter().position(&fits).map_or(0, |first| {
            let best = scored[first].1.score;
            let affordable = |slide: &(Candidate<'a>, Why)| {
                fits(slide) && slide.1.fit.fit >= RULE_FIT && slide.1.score * RULE_COST >= best
            };
            let kept =
                |slide: &(Candidate<'a>, Why)| [rated(slide), timely(slide), typed(slide), apart(slide)];
            let mut choice = (first, kept(&scored[first]));
            for (at, slide) in scored.iter().enumerate().skip(first + 1).filter(|(_, s)| affordable(s)) {
                let keeps = kept(slide);
                if keeps > choice.1 {
                    choice = (at, keeps);
                }
            }
            choice.0
        });
        out.push(scored.remove(at));
    }
    out
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
    genres.extend(title.hint_genres.iter().filter(|g| !title.genres.contains(g)));
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

/// The answer to a request, given atlas's lists for it.
pub fn answer(
    indexes: &Indexes,
    export: Option<&TitleIndex>,
    request: &Request,
    lists: &Lists,
    now: f64,
) -> serde_json::Value {
    let known = Knowledge { indexes };
    let only = request.only();
    let slides = request.limit.unwrap_or(SLIDES).clamp(1, MAX_SLIDES);

    let weighed: Vec<(Key, &LibraryEntry)> = request
        .library
        .iter()
        .filter(|e| e.weight.is_finite() && e.weight != 0.0)
        .filter_map(|e| Some(((media_type(&e.type_)?, e.id), e)))
        .collect();
    let library: Vec<(Features, f64)> = weighed
        .iter()
        .map(|&(key, e)| (Features::of(indexes, key, &known.title(key, Some(&e.hint), None)), e.weight))
        .collect();
    let library_unjudged =
        weighed.iter().filter(|&&(key, e)| known.title(key, Some(&e.hint), None).genres.is_empty()).count();
    let library_indexed = library.iter().filter(|(features, _)| features.indexed()).count();
    let taste = Fit::new(indexes, indexes.corpus(), &library);
    let fitted = |c: &Candidate<'_>| match &taste {
        Some(taste) => taste.of(&Features::of(indexes, c.key, &c.title)),
        None => Fitted { fit: NO_TASTE, ..Fitted::default() },
    };

    // In the web app's order: what just landed, what is trending everywhere, what is on the household's services,
    // then the client's own lists.
    let mut pool: Vec<Candidate<'_>> = Vec::new();
    let mut push = |key: Key, title, rank: Option<Placing>, arrival: Option<Placing>| {
        pool.push(Candidate { key, title, rank, arrival });
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
    for list in &lists.charts {
        let of = list.len() as f64;
        for (at, item) in list.iter().enumerate() {
            let placing = Placing { rank: at as f64, of };
            push(item.key, known.title(item.key, None, Some(item)), Some(placing), None);
        }
    }
    for item in lists.popular.iter().flatten() {
        push(item.key, known.title(item.key, None, Some(item)), None, None);
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
    let owned: HashSet<Key> =
        request.owned.iter().filter_map(|r| Some((media_type(&r.type_)?, r.id))).collect();
    let on = on_service(request, lists);
    let keep = |c: &Candidate<'_>| {
        only.is_none_or(|t| t == c.key.0)
            && on.as_ref().is_none_or(|on| on.contains(&c.key))
            && !owned.contains(&c.key)
            && !hidden(c, &request.hide)
    };
    // Then what no list knows to push: the catalogue's recent and coming titles nearest this household's taste.
    let personal = taste.as_ref().map_or_else(Vec::new, |taste| personal(indexes, &known, taste, now, keep));
    let personal_count = personal.len();
    pool.extend(personal);
    pool.iter_mut().for_each(|candidate| attend(indexes, export, candidate));
    let near = |a: &Candidate<'_>, b: &Candidate<'_>| {
        let row = |c: &Candidate<'_>| indexes.plot.row_of(c.key.1, c.key.0);
        match (row(a), row(b)) {
            (Some(a), Some(b)) => indexes.plot.similarity(a, b) >= SAME_INTEREST,
            _ => false,
        }
    };

    // Only titles something is known about, when there are enough of them: an unjudged title can't be matched
    // against this library's taste or dropped for missing it, so it competes on attention alone. Judged per title,
    // not per list entry: a title one list named only by id is judged when another list described it, and keeps the
    // place the first list gave it.
    let judged: HashSet<Key> = pool.iter().filter(|c| !c.title.genres.is_empty()).map(|c| c.key).collect();
    let unjudged_count =
        pool.iter().map(|c| c.key).filter(|key| !judged.contains(key)).collect::<HashSet<_>>().len();
    let pooled = pool.iter().map(|c| c.key).collect::<HashSet<_>>().len();
    let catalogue = pool
        .iter()
        .filter(|c| c.title.released.is_some_and(|r| now - r.first_day as f64 > CATALOGUE_DAYS))
        .map(|c| c.key)
        .collect::<HashSet<_>>()
        .len();
    // The unjudged titles most worth describing, by what they are worth before taste: a client that can say what
    // they are asks again with that as their hints.
    let unjudged: Vec<Key> = pick(
        pool.iter().filter(|c| !judged.contains(&c.key)).cloned().collect(),
        now,
        UNJUDGED_NAMED,
        keep,
        |_: &Candidate<'_>| Fitted { fit: NO_TASTE, ..Fitted::default() },
        |_: &Candidate<'_>, _: &Candidate<'_>| false,
    )
    .into_iter()
    .map(|(c, _)| c.key)
    .collect();
    if pool.iter().filter(|c| judged.contains(&c.key)).count() >= JUDGED_ENOUGH {
        pool.retain(|c| judged.contains(&c.key));
    }
    let picked = pick(pool, now, slides, keep, fitted, near);

    let round = |x: f64| (x * 1000.0).round() / 1000.0;
    let slides: Vec<serde_json::Value> = picked
        .iter()
        .map(|(c, why)| {
            let mut slide = serde_json::json!({
                "type": type_name(c.key.0),
                "id": c.key.1,
                "why": {
                    "score": round(why.score), "fit": round(why.fit.fit), "similar": why.fit.similar.map(round),
                    "profile": round(why.fit.profile), "people": round(why.fit.people),
                    "confidence": round(why.fit.confidence), "fresh": round(why.fresh),
                    "arrived": round(why.arrived), "quality": round(why.quality), "buzz": round(why.buzz),
                    "reason": why.reason.map(Reason::code),
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
        "unjudged": unjudged
            .iter()
            .map(|&(media_type, id)| serde_json::json!({ "type": type_name(media_type), "id": id }))
            .collect::<Vec<_>>(),
        "unjudgedCount": unjudged_count,
        "libraryUnjudged": library_unjudged,
        "pool": { "titles": pooled, "catalogue": catalogue, "personal": personal_count, "libraryIndexed": library_indexed },
    })
}

/// The titles a service channel may show; `None` for any other surface, which shows whatever fits.
///
/// Atlas holds no per-title availability: no JustWatch offers, and the facts' broadcasters are Wikidata entities
/// with no mapping to a service, which say where a series first aired rather than where it streams now. What it
/// can prove is on a service is what a list of that service named: its "new on" and "popular on" rows (JustWatch
/// filtered by the country's own package code), its Movie of the Night Top 10 and additions for that market, and
/// Trending Everywhere, which `lists` reads over that one service alone, so it is the service's trending chart.
/// Every list `lists` reads for a channel is one of these, so every listed title counts. So does every client
/// candidate: a channel page sends its own rows, which are that service's catalogue. Nothing else is proven, so
/// the personal pool's whole-catalogue titles, and any list or candidate outside this set, are dropped.
fn on_service(request: &Request, lists: &Lists) -> Option<HashSet<Key>> {
    request.service.as_ref()?;
    let listed =
        lists.arrivals.iter().chain(&lists.popular).chain(&lists.charts).flatten().chain(&lists.everywhere);
    let offered = request.candidates.iter().filter_map(|o| Some((media_type(&o.type_)?, o.id)));
    Some(listed.map(|l| l.key).chain(offered).collect())
}

/// The catalogue's titles out lately or coming soon (`PERSONAL_BEHIND`, `PERSONAL_AHEAD`) that fit this household
/// best, `PERSONAL` of them at most and none below `LEAD_FIT`. The lists only know what services and charts push; a
/// film from a director the household follows, or a series like the ones it finishes, can be on none of them. Each
/// is sketched first (`Fit::sketch`) and only the best `PERSONAL_SKETCHED` get the whole fit.
fn personal<'a>(
    indexes: &'a Indexes,
    known: &Knowledge<'a>,
    taste: &Fit<'_>,
    now: f64,
    keep: impl Fn(&Candidate<'a>) -> bool,
) -> Vec<Candidate<'a>> {
    let today = now as i64;
    let mut sketched: Vec<(f64, Candidate<'a>, Features)> = indexes
        .corpus()
        .released_between(today - PERSONAL_BEHIND, today + PERSONAL_AHEAD)
        .filter_map(|key| {
            let candidate = Candidate { key, title: known.title(key, None, None), rank: None, arrival: None };
            if candidate.title.genres.is_empty() || !keep(&candidate) {
                return None;
            }
            let features = Features::of(indexes, key, &candidate.title);
            Some((taste.sketch(&features), candidate, features))
        })
        .collect();
    sketched.sort_by(|a, b| b.0.total_cmp(&a.0));
    sketched.truncate(PERSONAL_SKETCHED);
    let mut fitted: Vec<(f64, Candidate<'a>)> =
        sketched.into_iter().map(|(_, candidate, features)| (taste.of(&features).fit, candidate)).collect();
    fitted.sort_by(|a, b| b.0.total_cmp(&a.0));
    fitted.into_iter().take(PERSONAL).take_while(|(fit, _)| *fit >= LEAD_FIT).map(|(_, c)| c).collect()
}

/// Slides an answer's log line names.
const SUMMARY_SLIDES: usize = 5;

/// One slide of an answer: its name from the metadata cards, else its IMDb id, and why it scored what it did.
pub fn describe(indexes: &Indexes, slide: &serde_json::Value) -> String {
    let key = slide["type"]
        .as_str()
        .and_then(media_type)
        .zip(slide["id"].as_u64().and_then(|id| id.try_into().ok()));
    let name = key
        .and_then(|key| indexes.cards.as_ref()?.get(&key))
        .map(|card| card.title.clone())
        .or_else(|| slide["imdbId"].as_str().map(str::to_owned))
        .unwrap_or_else(|| slide["id"].to_string());
    let term = |name: &str| slide["why"][name].as_f64().unwrap_or(0.0);
    let similar = slide["why"]["similar"].as_f64().map_or("-".to_owned(), |z| format!("{z:.1}"));
    let reason = slide["why"]["reason"].as_str().unwrap_or("-");
    format!(
        "{name} {:.3} (fit {:.2}: similar {similar}, profile {:.1}, people {:.2}; fresh {:.2}, arrived {:.2}, \
         quality {:.2}, buzz {:.2}; reason {reason})",
        term("score"),
        term("fit"),
        term("profile"),
        term("people"),
        term("fresh"),
        term("arrived"),
        term("quality"),
        term("buzz")
    )
}

/// A request as `den-atlas replay` ranks it again: the body as the client sent it, atlas's lists for it, and the
/// moment it was ranked at — the lists move between calls, so the same answer needs the same lists.
pub fn fixture(raw: &serde_json::Value, lists: &Lists, now: f64) -> serde_json::Value {
    let listed = |items: &[Listed]| -> Vec<serde_json::Value> {
        items
            .iter()
            .map(|l| {
                serde_json::json!({
                    "type": type_name(l.key.0), "id": l.key.1, "imdbId": l.imdb_id, "rating": l.rating, "year": l.year,
                })
            })
            .collect()
    };
    // Replay files are durable diagnostic artifacts. Keep the request shape, but never copy transient TMDB rating
    // metadata into one; those fields are allowed only in the live request or the bounded den-edge cache.
    fn scrub(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Array(items) => items.iter_mut().for_each(scrub),
            serde_json::Value::Object(object) => {
                object.retain(|key, value| {
                    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                    let transient = matches!(
                        normalized.to_ascii_lowercase().as_str(),
                        "rating"
                            | "ratings"
                            | "votes"
                            | "voteaverage"
                            | "votecount"
                            | "imdbrating"
                            | "tmdbrating"
                            | "tmdbvotes"
                    );
                    if !transient {
                        scrub(value);
                    }
                    !transient
                });
            }
            _ => {}
        }
    }
    let mut request = raw.clone();
    scrub(&mut request);
    serde_json::json!({
        "now": now,
        "request": request,
        "lists": {
            "arrivals": lists.arrivals.iter().map(|list| listed(list)).collect::<Vec<_>>(),
            "everywhere": listed(&lists.everywhere),
            "popular": lists.popular.iter().map(|list| listed(list)).collect::<Vec<_>>(),
            "charts": lists.charts.iter().map(|list| listed(list)).collect::<Vec<_>>(),
        },
    })
}

/// A kept `fixture` read back.
pub fn replayed(fixture: &serde_json::Value) -> Result<(Request, Lists, f64), String> {
    let now = fixture["now"].as_f64().ok_or("the fixture names no moment it was ranked at")?;
    let request = Request::deserialize(&fixture["request"]).map_err(|e| format!("request: {e}"))?;
    let items = |value: &serde_json::Value| value.as_array().map(Vec::as_slice).unwrap_or_default().to_vec();
    let listed = |value: &serde_json::Value| -> Vec<Listed> {
        items(value)
            .iter()
            .filter_map(|item| {
                Some(Listed {
                    key: (media_type(item["type"].as_str()?)?, item["id"].as_u64()?.try_into().ok()?),
                    imdb_id: item["imdbId"].as_str().map(str::to_owned),
                    rating: item["rating"].as_f64(),
                    year: item["year"].as_i64(),
                })
            })
            .collect()
    };
    let lists = Lists {
        arrivals: items(&fixture["lists"]["arrivals"]).iter().map(listed).collect(),
        everywhere: listed(&fixture["lists"]["everywhere"]),
        popular: items(&fixture["lists"]["popular"]).iter().map(listed).collect(),
        charts: items(&fixture["lists"]["charts"]).iter().map(listed).collect(),
    };
    Ok((request, lists, now))
}

/// The file a request's fixture is kept in: `<surface>.json`, and a service channel's
/// `<surface>-service-<id>[-<country>].json`, so a channel never replaces home's or another channel's.
fn fixture_name(raw: &serde_json::Value) -> String {
    let surface: String =
        raw["surface"].as_str().unwrap_or("home").chars().filter(char::is_ascii_lowercase).collect();
    let mut name = if surface.is_empty() { "home".to_owned() } else { surface };
    if let Some(id) = raw["service"]["id"].as_i64() {
        name.push_str(&format!("-service-{id}"));
        let country = raw["service"]["country"].as_str().unwrap_or_default().to_ascii_lowercase();
        if country.len() == 2 && country.bytes().all(|b| b.is_ascii_lowercase()) {
            name.push_str(&format!("-{country}"));
        }
    }
    name + ".json"
}

/// Keep a request's `fixture` in `dir` (`fixture_name`), replacing the last one for that surface.
pub fn keep_fixture(dir: &std::path::Path, raw: &serde_json::Value, lists: &Lists, now: f64) {
    let path = dir.join(fixture_name(raw));
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&path, fixture(raw, lists, now).to_string()));
    if let Err(e) = written {
        eprintln!("recommend fixture not kept at {}: {e}", path.display());
    }
}

/// An answer as one log line: what the request carried, and the slides it leads with, each with why — so a billboard
/// that leads with something odd can be read off the log rather than reproduced. A slide is named from the metadata
/// cards, else by its IMDb id. The library's titles are never named, only counted.
pub fn summary(indexes: &Indexes, request: &Request, answer: &serde_json::Value) -> String {
    let slides = answer["slides"].as_array().map(Vec::as_slice).unwrap_or_default();
    let top: Vec<String> = slides
        .iter()
        .take(SUMMARY_SLIDES)
        .enumerate()
        .map(|(at, slide)| format!("{}. {}", at + 1, describe(indexes, slide)))
        .collect();
    let on = match &request.service {
        Some(pick) if pick.country.is_empty() => format!(" on service {}", pick.id),
        Some(pick) => format!(" on service {} in {}", pick.id, pick.country),
        None => String::new(),
    };
    format!(
        "recommend {}{on}: library {} ({} unjudged, {} indexed), owned {}, candidates {}, pool {} ({} catalogue, \
         {} unjudged, {} personal), {} slides; {}",
        request.surface.as_deref().unwrap_or("home"),
        request.library.len(),
        answer["libraryUnjudged"],
        answer["pool"]["libraryIndexed"],
        request.owned.len(),
        request.candidates.len(),
        answer["pool"]["titles"],
        answer["pool"]["catalogue"],
        answer["unjudgedCount"],
        answer["pool"]["personal"],
        slides.len(),
        top.join("; ")
    )
}

/// A kept Movie of the Night list's titles of one type, as a list the billboard reads.
fn motn_listed(shows: &[crate::motn::Show], series: bool) -> Vec<Listed> {
    shows
        .iter()
        .filter(|s| s.series == series)
        .map(|s| Listed {
            key: (if series { MediaType::Tv } else { MediaType::Movie }, s.tmdb),
            imdb_id: s.imdb.clone(),
            rating: s.rating,
            year: s.year,
        })
        .collect()
}

/// The services whose lists a request reads, each in its country (`country` resolves a picked one, `""` for none):
/// a channel's one service, else the household's, else every service this install carries.
///
/// A channel for a service this install doesn't carry reads none. Atlas then knows nothing that is on it, so the
/// channel ranks the client's candidates alone (`on_service`) rather than refusing: the page still gets a hero from
/// its own rows, and a service a client knows before atlas does isn't an error.
fn services_read(
    providers: &[&'static Provider],
    request: &Request,
    country: impl Fn(&str) -> String,
) -> Vec<(&'static Provider, String)> {
    if let Some(pick) = &request.service {
        let provider = providers.iter().find(|p| p.package_ids.contains(&pick.id));
        return provider.map(|&p| (p, country(&pick.country))).into_iter().collect();
    }
    let mut services = Vec::new();
    for &provider in providers {
        if request.services.is_empty() {
            services.push((provider, country("")));
        }
        for pick in request.services.iter().filter(|s| provider.package_ids.contains(&s.id)) {
            services.push((provider, country(&pick.country)));
        }
    }
    services
}

/// Atlas's own lists for a request: "new on" and "popular on" each of the household's services in its own country
/// (every service this install carries when none are picked), of the surface's types, and Trending Everywhere. A
/// list that can't be had is empty; the catalog already degrades and caches. A service channel reads only lists of
/// its service (`on_service`).
pub async fn lists(state: &Arc<AppState>, config: &Config, request: &Request) -> Lists {
    /// A catalog to read: its id, Stremio type, country and the providers it is for.
    type Wanted = (String, &'static str, String, Vec<&'static Provider>);
    let country = config.country(None, &state.default_country);
    let types: Vec<&'static str> = ["movie", "series"]
        .into_iter()
        .filter(|t| request.only().is_none_or(|only| media_type(t) == Some(only)))
        .collect();
    let services = services_read(&config.providers, request, |there| {
        config.country(Some(there), &state.default_country)
    });
    if request.service.is_some() && services.is_empty() {
        return Lists::default();
    }
    // Trending Everywhere is a union over the providers it is read for, so a channel reads it over its one service in
    // that service's country: the service's own trending chart, every title of it on the service.
    let trending = match (&request.service, services.first()) {
        (Some(_), Some((provider, there))) => (there.clone(), vec![*provider]),
        _ => (country.clone(), config.providers.clone()),
    };
    // Movie of the Night's lists stand in for JustWatch's where they are kept (`motn.rs`): a service's own Top 10 is a
    // chart, and what was added to it an arrival. Netflix's US Top 10 goes to every household too: the one chart that
    // says what most people are watching now — but not to a channel, since it says nothing of what is on a service
    // elsewhere, or on any other service.
    let mut out = Lists::default();
    let (mut arrivals, mut popular): (Vec<Wanted>, Vec<Wanted>) = (Vec::new(), Vec::new());
    let netflix = provider_by_code("nfx");
    for &t in &types {
        let series = t == "series";
        for (provider, there) in &services {
            state.motn.want(provider, there);
            match state.motn.added(provider, there) {
                Some(shows) => out.arrivals.push(motn_listed(&shows, series)),
                None => arrivals.push((new_catalog_id(provider), t, there.clone(), vec![*provider])),
            }
            match state.motn.top(provider, there) {
                Some(shows) => out.charts.push(motn_listed(&shows, series)),
                None => popular.push((provider.id.to_owned(), t, there.clone(), vec![*provider])),
            }
        }
        if let (None, Some(netflix)) = (&request.service, netflix) {
            state.motn.want(netflix, "US");
            out.charts.extend(state.motn.top(netflix, "US").map(|shows| motn_listed(&shows, series)));
        }
    }
    arrivals.truncate(SERVICE_LISTS);
    popular.truncate(SERVICE_LISTS - arrivals.len());

    let mut set = tokio::task::JoinSet::new();
    let (new_lists, popular_lists) = (arrivals.len(), popular.len());
    let wanted = arrivals
        .into_iter()
        .chain(popular)
        .chain(types.iter().map(|&t| (TRENDING_ID.to_owned(), t, trending.0.clone(), trending.1.clone())));
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
    let (mut movies, mut series) = (Vec::new(), Vec::new());
    for (at, stremio_type, items) in answers {
        if at < new_lists {
            out.arrivals.push(items);
        } else if at < new_lists + popular_lists {
            out.popular.push(items);
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
    const DRAMA: u16 = 18;

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
        Candidate { key: (MediaType::Movie, id), title, rank: None, arrival: None }
    }

    fn ids(picked: &[(Candidate<'_>, Why)]) -> Vec<u32> {
        picked.iter().map(|(c, _)| c.key.1).collect()
    }

    fn all(_: &Candidate<'_>) -> bool {
        true
    }

    /// Every title fitting alike, so the rest of the score decides.
    fn neutral(_: &Candidate<'_>) -> Fitted {
        Fitted { fit: NO_TASTE, ..Fitted::default() }
    }

    /// No two titles one interest.
    fn apart(_: &Candidate<'_>, _: &Candidate<'_>) -> bool {
        false
    }

    /// Ids below 100 fit `high`, the rest `low`.
    fn fitting(high: f64, low: f64) -> impl Fn(&Candidate<'_>) -> Fitted {
        move |c: &Candidate<'_>| Fitted { fit: if c.key.1 < 100 { high } else { low }, ..Fitted::default() }
    }

    fn rated(released: &str, quality_rating: f64) -> Title<'static> {
        Title {
            released: on(released),
            rating: Some(quality_rating),
            votes: Some(5000.0),
            ..Title::default()
        }
    }

    #[test]
    fn at_most_three_of_ten_are_catalogue_when_new_titles_fit_well_enough() {
        // Twelve old favourites that fit best and outscore seven titles from this year that fit well but less.
        let mut pool: Vec<Candidate<'static>> = (0..12).map(|i| cand(i, rated("2015-01-01", 8.0))).collect();
        pool.extend((100..107).map(|i| cand(i, rated("2026-01-01", 8.0))));
        let picked = pick(pool, now(), 10, all, fitting(0.9, 0.55), apart);
        let old = picked.iter().filter(|(_, why)| stale(why)).count();
        assert_eq!(old, STALE_SLIDES, "{:?}", ids(&picked));
        assert_eq!(ids(&picked)[..2], [0, 1], "the best old favourites still lead");
    }

    #[test]
    fn no_rule_puts_a_title_that_barely_fits_ahead_of_one_that_fits() {
        let mut pool: Vec<Candidate<'static>> = (0..12).map(|i| cand(i, rated("2015-01-01", 8.0))).collect();
        // New enough to be well within `RULE_COST`: only the fit keeps them out.
        pool.extend((100..107).map(|i| cand(i, rated("2026-08-01", 8.0))));
        let picked = pick(pool, now(), 10, all, fitting(0.9, RULE_FIT - 0.1), apart);
        assert!(picked.iter().all(|(c, _)| c.key.1 < 100), "{:?}", ids(&picked));
    }

    #[test]
    fn nor_one_nobody_liked() {
        let mut pool: Vec<Candidate<'static>> = (0..12).map(|i| cand(i, rated("2015-01-01", 8.0))).collect();
        pool.extend((100..105).map(|i| cand(i, rated("2026-08-01", 4.0))));
        let picked = pick(pool, now(), 10, all, fitting(0.9, 0.9), apart);
        let poor = picked.iter().filter(|(_, why)| why.quality < POOR_QUALITY).count();
        assert!(poor <= POOR_SLIDES, "{:?}", ids(&picked));
    }

    #[test]
    fn each_ten_mixes_films_and_series() {
        let series = |id| Candidate { key: (MediaType::Tv, id), ..cand(id, rated("2026-08-01", 7.5)) };
        let mut pool: Vec<Candidate<'static>> = (0..20).map(|i| cand(i, rated("2026-08-01", 8.0))).collect();
        pool.extend((100..110).map(series));
        let picked = pick(pool, now(), 20, all, fitting(0.9, 0.8), apart);
        for ten in picked.chunks(LEAD_SLIDES) {
            let shows = ten.iter().filter(|(c, _)| c.key.0 == MediaType::Tv).count();
            assert_eq!(shows, EACH_TYPE_SLIDES, "{:?}", ids(ten));
        }
        // A pool of one type fills the ten with it.
        let films: Vec<Candidate<'static>> = (0..12).map(|i| cand(i, rated("2026-08-01", 8.0))).collect();
        assert_eq!(pick(films, now(), 10, all, neutral, apart).len(), 10);
    }

    #[test]
    fn one_interest_takes_one_place_in_ten() {
        // Ids 0-4 are one interest (a franchise and its spin-offs); 100-109 are all different.
        let mut pool: Vec<Candidate<'static>> = (0..5).map(|i| cand(i, rated("2026-08-01", 8.0))).collect();
        pool.extend((100..110).map(|i| cand(i, rated("2026-08-01", 8.0))));
        let near = |a: &Candidate<'_>, b: &Candidate<'_>| a.key.1 < 100 && b.key.1 < 100;
        let picked = pick(pool, now(), 10, all, fitting(0.9, 0.8), near);
        assert_eq!(picked.iter().filter(|(c, _)| c.key.1 < 100).count(), 1, "{:?}", ids(&picked));
        assert_eq!(picked[0].0.key.1, 0, "the best of the interest keeps its place");
    }

    #[test]
    fn a_title_nobody_liked_costs_as_much_as_one_nobody_is_talking_about() {
        let fit = Fitted { fit: 0.8, ..Fitted::default() };
        let liked = cand(1, rated("2026-09-01", 8.5));
        let disliked = cand(2, rated("2026-09-01", 5.0));
        let (liked, disliked) = (score(&liked, now(), 0.0, fit), score(&disliked, now(), 0.0, fit));
        assert!((liked.score / disliked.score - 1.0 / MERIT_FLOOR).abs() < 1e-9);
    }

    #[test]
    fn an_all_time_vote_count_counts_as_buzz_but_is_never_the_reason() {
        let seen = |from_votes| {
            cand(
                1,
                Title {
                    released: on("1997-06-01"),
                    popularity: Some(100.0),
                    popularity_from_votes: from_votes,
                    ..Title::default()
                },
            )
        };
        let fit = Fitted::default();
        let (now_popular, widely_seen) =
            (score(&seen(false), now(), 100.0, fit), score(&seen(true), now(), 100.0, fit));
        assert_eq!((now_popular.buzz, widely_seen.buzz), (1.0, 1.0));
        assert_eq!(now_popular.reason, Some(Reason::Buzz));
        assert_eq!(widely_seen.reason, None);
        // A current score from another copy of the title wins over the converted count.
        let current = cand(1, Title { popularity: Some(3.0), ..Title::default() });
        let (merged, _) = pick(vec![seen(true), current], now(), 40, all, neutral, apart).remove(0);
        assert_eq!((merged.title.popularity, merged.title.popularity_from_votes), (Some(3.0), false));
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
        let picked = pick(vec![cand(1, new(2.0)), cand(2, new(900.0))], now(), 40, all, neutral, apart);
        assert_eq!(ids(&picked), vec![2, 1]);
    }

    #[test]
    fn a_timely_reason_says_whether_a_title_is_recent_or_upcoming() {
        let upcoming = cand(1, Title { released: on("2026-09-20"), ..Title::default() });
        let recent = cand(2, Title { released: on("2026-09-10"), ..Title::default() });
        assert_eq!(score(&upcoming, now(), 0.0, neutral(&upcoming)).reason, Some(Reason::Upcoming));
        assert_eq!(score(&recent, now(), 0.0, neutral(&recent)).reason, Some(Reason::Recent));
    }

    #[test]
    fn a_reason_compares_score_lifts_instead_of_incomparable_raw_terms() {
        let popular = Candidate {
            rank: Some(Placing { rank: 0.0, of: 10.0 }),
            ..cand(1, Title { released: on("1997-06-01"), ..Title::default() })
        };
        let fitted = Fitted {
            fit: 0.6,
            people: 0.2,
            reason: Some(FitReason::People),
            reason_lift: 0.2,
            ..Fitted::default()
        };
        let why = score(&popular, now(), 0.0, fitted);
        assert_eq!(why.buzz, 1.0);
        assert_eq!(why.reason, Some(Reason::People), "a 10% buzz lift must not beat a 22% taste lift");
    }

    #[test]
    fn a_rating_is_pulled_towards_the_prior_by_how_few_votes_it_rests_on() {
        let rated = |rating, votes| Title { rating: Some(rating), votes: Some(votes), ..Title::default() };
        assert!((quality(&rated(8.5, 12.0)) - 0.354).abs() < 0.001);
        assert!(quality(&rated(8.5, 500.0)) > 0.9);
        assert!(quality(&rated(6.0, 500.0)) < 0.1);
        assert!((quality(&Title::default()) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn fit_decides_between_a_title_for_this_household_and_one_for_everyone() {
        let everyone = Candidate {
            rank: Some(Placing { rank: 0.0, of: 100.0 }),
            arrival: Some(Placing { rank: 0.0, of: 100.0 }),
            ..cand(1, Title { released: on("2026-09-10"), popularity: Some(900.0), ..Title::default() })
        };
        let theirs = cand(2, Title { released: on("2026-05-01"), ..Title::default() });
        let fit =
            |c: &Candidate<'_>| Fitted { fit: if c.key.1 == 2 { 0.7 } else { 0.3 }, ..Fitted::default() };
        assert_eq!(ids(&pick(vec![everyone, theirs], now(), 40, all, fit, apart)), vec![2, 1]);
    }

    #[test]
    fn a_title_that_barely_fits_waits_below_the_lead_however_new() {
        let buzzing = Candidate {
            rank: Some(Placing { rank: 0.0, of: 10.0 }),
            ..cand(99, Title { released: on("2026-09-10"), ..Title::default() })
        };
        let mut pool: Vec<Candidate<'static>> =
            (0..12).map(|i| cand(i, Title { released: on("2024-01-01"), ..Title::default() })).collect();
        pool.push(buzzing);
        let fit =
            |c: &Candidate<'_>| Fitted { fit: if c.key.1 == 99 { 0.2 } else { 0.3 }, ..Fitted::default() };
        let picked = ids(&pick(pool, now(), 40, all, fit, apart));
        assert_eq!(picked[LEAD_SLIDES], 99, "{picked:?}");
    }

    #[test]
    fn an_arrival_lifts_an_old_title_over_the_same_title_not_arriving() {
        let landed = Candidate {
            arrival: Some(Placing { rank: 0.0, of: 10.0 }),
            ..cand(1, Title { released: on("1997-06-01"), ..Title::default() })
        };
        let still = cand(2, Title { released: on("1997-06-01"), ..Title::default() });
        assert_eq!(ids(&pick(vec![still, landed], now(), 40, all, neutral, apart)), vec![1, 2]);
        assert_eq!(
            arrival(
                &Candidate { arrival: Some(Placing { rank: 0.0, of: 0.0 }), ..cand(4, Title::default()) },
                now()
            ),
            0.0
        );
    }

    #[test]
    fn catalogue_joining_a_service_counts_a_quarter_of_something_new_arriving() {
        let arriving = |date| Candidate {
            arrival: Some(Placing { rank: 0.0, of: 10.0 }),
            ..cand(1, Title { released: on(date), ..Title::default() })
        };
        assert_eq!(arrival(&arriving("2014-11-05"), now()), 0.25);
        assert_eq!(arrival(&arriving("2026-03-01"), now()), 1.0);
        assert_eq!(
            arrival(
                &Candidate { arrival: Some(Placing { rank: 0.0, of: 10.0 }), ..cand(1, Title::default()) },
                now()
            ),
            1.0
        );
    }

    #[test]
    fn a_fixture_reads_back_as_the_request_lists_and_moment_it_was_ranked_with() {
        let raw = serde_json::json!({
            "surface": "movies",
            "library": [{
                "type": "movie", "id": 1, "weight": 1.0,
                "hint": {"genreIds": [18], "rating": 8.4, "votes": 1200}
            }],
            "candidates": [{
                "type": "movie", "id": 7,
                "hint": {"releaseDate": "2026-09-01", "voteAverage": 9.1, "voteCount": 20_000}
            }],
            "futureClient": {"vote_average": 9.9, "nested": [{"tmdbRating": 9.8, "imdbRating": 9.7}]}
        });
        let lists = Lists {
            arrivals: vec![vec![Listed {
                key: (MediaType::Movie, 7),
                imdb_id: Some("tt7".to_owned()),
                rating: Some(6.9),
                year: Some(2024),
            }]],
            everywhere: vec![Listed { key: (MediaType::Tv, 8), imdb_id: None, rating: None, year: None }],
            popular: vec![vec![Listed {
                key: (MediaType::Movie, 9),
                imdb_id: None,
                rating: Some(8.1),
                year: None,
            }]],
            charts: vec![vec![Listed { key: (MediaType::Tv, 10), imdb_id: None, rating: None, year: None }]],
        };
        let kept = fixture(&raw, &lists, 20_709.5);
        assert!(kept["request"]["library"][0]["hint"].get("rating").is_none());
        assert!(kept["request"]["library"][0]["hint"].get("votes").is_none());
        assert!(kept["request"]["candidates"][0]["hint"].get("voteAverage").is_none());
        assert!(kept["request"]["candidates"][0]["hint"].get("voteCount").is_none());
        assert!(kept["request"]["futureClient"].get("vote_average").is_none());
        assert!(kept["request"]["futureClient"]["nested"][0].get("tmdbRating").is_none());
        assert!(kept["request"]["futureClient"]["nested"][0].get("imdbRating").is_none());
        let (request, back, now) = replayed(&kept).unwrap();
        assert_eq!(now, 20_709.5);
        assert_eq!((request.surface.as_deref(), request.library.len()), (Some("movies"), 1));
        assert_eq!(request.library[0].hint.genre_ids, Some(vec![18]));
        assert_eq!((request.library[0].hint.rating, request.library[0].hint.votes), (None, None));
        assert_eq!(request.candidates[0].hint.release_date.as_deref(), Some("2026-09-01"));
        assert_eq!(
            (back.arrivals, back.everywhere, back.popular, back.charts),
            (lists.arrivals, lists.everywhere, lists.popular, lists.charts)
        );
        assert!(replayed(&serde_json::json!({})).is_err());
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
        assert_eq!(ids(&pick(vec![classic, new], now(), 40, all, neutral, apart)), vec![2, 1]);
    }

    #[test]
    fn merges_what_two_sources_offered_and_takes_the_better_placing() {
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
        assert_eq!(ids(&pick(vec![ranked, described, alone], now(), 40, all, neutral, apart)), vec![5, 6]);

        let arriving = |id, rank| Candidate {
            arrival: Some(Placing { rank, of: 100.0 }),
            ..cand(id, film(&[CRIME], ""))
        };
        let picked = pick(
            vec![arriving(5, 60.0), arriving(5, 0.0), arriving(6, 30.0)],
            now(),
            40,
            all,
            neutral,
            apart,
        );
        assert_eq!(ids(&picked), vec![5, 6]);
    }

    #[test]
    fn a_merge_keeps_the_exact_date_and_a_rating_counted_on_enough_votes_whichever_came_first() {
        let listed = || {
            cand(
                5,
                Title {
                    released: Some(Released::year(2026)),
                    rating: Some(5.4),
                    votes: Some(RATING_PRIOR_VOTES),
                    estimated_votes: true,
                    ..Title::default()
                },
            )
        };
        let described = |votes| {
            cand(
                5,
                Title {
                    released: on("2026-09-10"),
                    rating: Some(7.2),
                    votes: Some(votes),
                    ..Title::default()
                },
            )
        };
        let (merged, _) = pick(vec![listed(), described(400.0)], now(), 40, all, neutral, apart).remove(0);
        assert_eq!(merged.title.released, on("2026-09-10"));
        assert_eq!((merged.title.rating, merged.title.votes), (Some(7.2), Some(400.0)));
        assert!(!merged.title.estimated_votes);

        // Eighteen votes don't outweigh IMDb's score, in either order.
        for pool in [vec![listed(), described(18.0)], vec![described(18.0), listed()]] {
            let (merged, _) = pick(pool, now(), 40, all, neutral, apart).remove(0);
            assert_eq!(merged.title.released, on("2026-09-10"));
            assert_eq!(merged.title.rating, Some(5.4));
            assert!(merged.title.estimated_votes);
        }
    }

    #[test]
    fn filters_before_cutting_to_the_slide_count() {
        let pool: Vec<Candidate<'static>> =
            (0..60).map(|i| cand(i, Title { released: on("2026-08-01"), ..Title::default() })).collect();
        let picked = pick(pool, now(), 20, |c| c.key.1 % 2 == 0, neutral, apart);
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
        // Taste read only the first genre a hint named; the rules read them all.
        let hinted = Title { hint_genres: vec![18, 14], ..film(&[18], "") };
        assert!(hidden(&series(hinted), &rules));
    }

    #[test]
    fn a_rule_hides_a_title_even_when_a_copy_that_knew_nothing_came_first() {
        let rules = Hide { languages: vec!["ta".into()], ..Hide::default() };
        let listed =
            Candidate { arrival: Some(Placing { rank: 0.0, of: 10.0 }), ..cand(7, Title::default()) };
        let described = cand(7, Title { original_language: Some(*b"ta"), ..film(&[DRAMA], "") });
        let other = cand(8, Title { released: on("2026-09-01"), ..Title::default() });
        let picked = pick(vec![listed, described, other], now(), 40, |c| !hidden(c, &rules), neutral, apart);
        assert_eq!(ids(&picked), vec![8]);
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

    fn request(body: serde_json::Value) -> Request {
        Request::deserialize(&body).unwrap()
    }

    #[test]
    fn a_request_reads_with_and_without_a_service() {
        let home = request(serde_json::json!({"surface": "home", "services": [{"id": 8, "country": "FI"}]}));
        assert!(home.service.is_none());
        assert_eq!((home.services[0].id, home.services[0].country.as_str()), (8, "FI"));
        let channel =
            request(serde_json::json!({"surface": "movies", "service": {"id": 337, "country": "SE"}}));
        let pick = channel.service.as_ref().unwrap();
        assert_eq!((pick.id, pick.country.as_str(), channel.only()), (337, "SE", Some(MediaType::Movie)));
        assert_eq!(request(serde_json::json!({"service": {"id": 8}})).service.unwrap().country, "");
        assert!(Request::deserialize(&serde_json::json!({"service": {"country": "FI"}})).is_err());
    }

    #[test]
    fn rating_hints_accept_only_the_named_fields_and_valid_tmdb_ranges() {
        let aliases: Hint = serde_json::from_value(serde_json::json!({
            "voteAverage": 8.4, "voteCount": 1200, "vote_average": 9.1, "vote_count": 20_000
        }))
        .unwrap();
        assert_eq!((aliases.rating, aliases.votes), (None, None));

        for raw in [
            serde_json::json!({"rating": 0, "votes": 100}),
            serde_json::json!({"rating": 10.1, "votes": 100}),
            serde_json::json!({"rating": 8.4, "votes": -1}),
            serde_json::json!({"rating": 8.4, "votes": 1.5}),
        ] {
            let hint: Hint = serde_json::from_value(raw).unwrap();
            let got = hinted_rating(Some(&hint));
            if hint.rating.is_some_and(|rating| rating > 0.0 && rating <= 10.0) {
                assert_eq!(got, Some((8.4, None)));
            } else {
                assert_eq!(got, None);
            }
        }
    }

    #[test]
    fn a_channel_reads_its_one_service_and_an_unknown_one_reads_none() {
        let providers: Vec<&'static Provider> =
            ["nfx", "dnp", "prv"].iter().map(|code| provider_by_code(code).unwrap()).collect();
        let country = |there: &str| if there.is_empty() { "US".to_owned() } else { there.to_owned() };
        let read = |body| -> Vec<(&str, String)> {
            services_read(&providers, &request(body), country).into_iter().map(|(p, c)| (p.code, c)).collect()
        };
        let every = vec![("nfx", "US".to_owned()), ("dnp", "US".to_owned()), ("prv", "US".to_owned())];
        assert_eq!(read(serde_json::json!({})), every);
        assert_eq!(
            read(serde_json::json!({"services": [{"id": 9, "country": "FI"}, {"id": 8, "country": "SE"}]})),
            vec![("nfx", "SE".to_owned()), ("prv", "FI".to_owned())]
        );
        // The channel's service alone, whatever the household has, by any id it is known by.
        let channel = serde_json::json!({"service": {"id": 9, "country": "FI"}, "services": [{"id": 8, "country": "SE"}]});
        assert_eq!(read(channel), vec![("prv", "FI".to_owned())]);
        assert_eq!(read(serde_json::json!({"service": {"id": 337}})), vec![("dnp", "US".to_owned())]);
        // Not every service, as a household that picked none would get: none.
        assert!(read(serde_json::json!({"service": {"id": 283, "country": "FI"}})).is_empty());
    }

    #[test]
    fn a_channel_may_show_what_its_lists_named_and_the_client_offered() {
        let item = |media_type, id| Listed { key: (media_type, id), imdb_id: None, rating: None, year: None };
        let lists = Lists {
            arrivals: vec![vec![item(MediaType::Movie, 1)]],
            everywhere: vec![item(MediaType::Tv, 2)],
            popular: vec![vec![item(MediaType::Movie, 3)]],
            charts: vec![vec![item(MediaType::Tv, 4)]],
        };
        let offered = serde_json::json!([{"type": "movie", "id": 5}, {"type": "anime", "id": 6}]);
        assert!(on_service(&request(serde_json::json!({"candidates": offered})), &lists).is_none());
        let on =
            on_service(&request(serde_json::json!({"service": {"id": 8}, "candidates": offered})), &lists)
                .unwrap();
        let mut keys: Vec<Key> = on.into_iter().collect();
        keys.sort_unstable();
        let (movie, tv) = (MediaType::Movie, MediaType::Tv);
        assert_eq!(keys, vec![(movie, 1), (movie, 3), (movie, 5), (tv, 2), (tv, 4)]);
    }

    #[test]
    fn a_channels_fixture_is_kept_apart_from_homes() {
        let name = |body| fixture_name(&body);
        assert_eq!(name(serde_json::json!({})), "home.json");
        assert_eq!(name(serde_json::json!({"surface": "movies"})), "movies.json");
        assert_eq!(
            name(serde_json::json!({"service": {"id": 8, "country": "FI"}})),
            "home-service-8-fi.json"
        );
        assert_eq!(
            name(serde_json::json!({"surface": "series", "service": {"id": 337}})),
            "series-service-337.json"
        );
        assert_eq!(
            name(serde_json::json!({"service": {"id": 8, "country": "../x"}})),
            "home-service-8.json",
            "only a country code reaches the name"
        );
    }
}
