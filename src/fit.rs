//! How well a title fits one household's taste (`/recommend`): the part of a billboard's score that makes it this
//! household's rather than everyone's.
//!
//! Three signals, each read against the index as a whole, so a fit says how far a title stands out rather than a level
//! every mainstream title reaches:
//! - **similar**: its plot vector's closeness to the household's nearest liked titles, less its closeness to the index
//!   as a whole, so a generically central title doesn't match everything;
//! - **profile**: how over-represented its subgenres, moods, genres, language, country, decade, type and network are in
//!   the library against the index — a smoothed log-lift, averaged within a family so more genres isn't more taste;
//! - **people**: the makers and cast the library keeps coming back to, a rarer person counting for more.
//!
//! Similar and profile become z-scores against a fixed sample of the index. A title without a vector leans on its
//! profile alone, shrunk by how little is known of it. The sum passes a sigmoid centred about the index's top 2%.
//!
//! Chosen on a real 145-title library by hiding a fifth of its liked titles and ranking the billboard pool: this found
//! 38% of the hidden titles in its top 40, the facet affinity it replaced 7%.

use crate::queries::Indexes;
use crate::recommend::{Knowledge, Title};
use den_index::MediaType;
use std::collections::{HashMap, HashSet};

type Key = (MediaType, u32);

/// Index titles the calibration reads, spread evenly through it.
const SAMPLE: usize = 3000;
/// Liked titles a title's plot is compared with.
const NEAREST: usize = 5;
/// Titles' worth of the index's own mix a library's count of a value is blended with, so a value seen once in a
/// small library isn't read as a passion.
const LIFT_PRIOR: f64 = 3.0;
/// The share of a value the index never carries.
const UNHEARD_OF: f64 = 1e-4;
/// How the signals add up, in z units.
const W_SIMILAR: f64 = 0.3;
const W_PROFILE: f64 = 0.7;
const W_PEOPLE: f64 = 2.0;
/// What a title without a vector keeps of its profile when its genre, language and country are all known.
const THIN: f64 = 0.5;
/// The z at which a title fits one half: about the index's top 2%.
const MIDPOINT: f64 = 2.0;
/// How much closeness to a disliked title, beyond closeness to a liked one, takes away.
const REPULSION: f64 = 0.5;
/// How far towards a full fit a title carries for being the next of a franchise already followed.
const FRANCHISE_LIFT: f64 = 0.5;
/// Rarity-weighted shared credits at which the people signal is about two thirds of the way to one.
const PEOPLE_SCALE: f64 = 12.0;

/// The families a profile is read over.
const SUBGENRE: usize = 0;
const MOOD: usize = 1;
const GENRE: usize = 2;
const LANGUAGE: usize = 3;
const COUNTRY: usize = 4;
const DECADE: usize = 5;
const TYPE: usize = 6;
const BROADCASTER: usize = 7;
/// How much each family counts. A subgenre says most about what a title is; a network least.
const FAMILY_WEIGHTS: [f64; 8] = [1.0, 0.4, 0.6, 0.5, 0.4, 0.3, 0.4, 0.2];

/// One value of one family: a label's name as a hash, a genre id, a language's letters, and so on.
type Value = (usize, u64);

/// What fit reads of a title, owned, so a sample of the index can be kept for the life of the indexes.
pub struct Features {
    row: Option<u32>,
    /// Each family's values once, with the label's confidence (1 for the rest).
    values: Vec<(Value, f64)>,
    /// The people behind it, each once, with how much they count (`recommend::CAST_BILLED`).
    people: Vec<(u32, f64)>,
    franchise: Option<u32>,
}

impl Features {
    pub fn of(indexes: &Indexes, (media_type, id): Key, title: &Title<'_>) -> Features {
        let mut values: Vec<(Value, f64)> = Vec::new();
        let mut put = |family: usize, value: u64, confidence: f64| {
            if !values.iter().any(|&(held, _)| held == (family, value)) {
                values.push(((family, value), confidence));
            }
        };
        if let Some(labels) = &title.labels {
            labels.subgenres.iter().for_each(|&(name, confidence)| put(SUBGENRE, hashed(name), confidence));
            labels.moods.iter().for_each(|&(name, confidence)| put(MOOD, hashed(name), confidence));
        }
        title.genres.iter().for_each(|&genre| put(GENRE, u64::from(genre), 1.0));
        title
            .languages
            .iter()
            .take(1)
            .for_each(|&code| put(LANGUAGE, u64::from(u16::from_be_bytes(code)), 1.0));
        title
            .countries
            .iter()
            .take(2)
            .for_each(|&code| put(COUNTRY, u64::from(u16::from_be_bytes(code)), 1.0));
        if let Some(released) = title.released {
            put(DECADE, released.year_of().div_euclid(10).cast_unsigned(), 1.0);
        }
        put(TYPE, u64::from(media_type == MediaType::Tv), 1.0);
        title.broadcasters.iter().for_each(|&network| put(BROADCASTER, u64::from(network), 1.0));
        let mut people: Vec<(u32, f64)> = Vec::new();
        for &(person, share) in &title.people {
            if !people.iter().any(|&(held, _)| held == person) {
                people.push((person, share));
            }
        }
        Features { row: indexes.plot.row_of(id, media_type), values, people, franchise: title.franchise }
    }

    /// Whether the plot index holds a vector for the title.
    pub fn indexed(&self) -> bool {
        self.row.is_some()
    }

    fn has(&self, family: usize) -> bool {
        self.values.iter().any(|&((held, _), _)| held == family)
    }
}

/// What fit reads off the index as a whole: worked out once for the life of the indexes (`Indexes::corpus`).
pub struct Corpus {
    /// Each value's share of the titles carrying its family.
    shares: HashMap<Value, f64>,
    /// How many titles credit each person.
    credits: HashMap<u32, u32>,
    titles: f64,
    /// The mean of the index's vectors: a title's projection on it is how central the title is.
    centre: Vec<f64>,
    /// An even sample of the index with vectors, the yardstick every fit is read against.
    sample: Vec<Features>,
    /// Every dated title by the first day it could have come out, with how many days that date spans.
    dated: Vec<(i64, i64, Key)>,
}

impl Corpus {
    pub fn of(indexes: &Indexes) -> Corpus {
        let known = Knowledge { indexes };
        let mut keys: Vec<Key> = indexes.plot.titles().collect();
        if let Some(facts) = &indexes.facts {
            let indexed: HashSet<Key> = keys.iter().copied().collect();
            let mut described: Vec<Key> = facts.keys().filter(|key| !indexed.contains(key)).collect();
            described.sort_unstable();
            keys.extend(described);
        }
        let step = (indexes.plot.len() / SAMPLE).max(1);
        let (mut counts, mut carrying, mut credits) = (HashMap::new(), [0.0; 8], HashMap::new());
        let (mut sample, mut dated) = (Vec::with_capacity(SAMPLE), Vec::new());
        for (at, &key) in keys.iter().enumerate() {
            let title = known.title(key, None, None);
            if let Some(released) = title.released {
                dated.push((released.first_day, released.span_days.max(1), key));
            }
            let features = Features::of(indexes, key, &title);
            for (family, carried) in carrying.iter_mut().enumerate() {
                if features.has(family) {
                    *carried += 1.0;
                }
            }
            for &(value, _) in &features.values {
                *counts.entry(value).or_insert(0.0) += 1.0;
            }
            for &(person, _) in &features.people {
                *credits.entry(person).or_insert(0) += 1;
            }
            if features.row.is_some() && at % step == 0 && sample.len() < SAMPLE {
                sample.push(features);
            }
        }
        let shares =
            counts.into_iter().map(|(value, n): (Value, f64)| (value, n / carrying[value.0])).collect();
        dated.sort_unstable();
        Corpus {
            shares,
            credits,
            titles: keys.len() as f64,
            centre: indexes.plot.mean_vector(),
            sample,
            dated,
        }
    }

    /// The titles that could have come out between two days, inclusive: a date known only to its year counts when
    /// any day of that year does.
    pub fn released_between(&self, from: i64, to: i64) -> impl Iterator<Item = Key> + '_ {
        released_between(&self.dated, from, to)
    }

    fn share(&self, value: Value) -> f64 {
        self.shares.get(&value).copied().unwrap_or(UNHEARD_OF)
    }
}

/// A household's taste, read off its library against the index.
pub struct Fit<'a> {
    indexes: &'a Indexes,
    corpus: &'a Corpus,
    liked: Vec<(u32, f64)>,
    disliked: Vec<u32>,
    lift: HashMap<Value, f64>,
    /// The lift of a value the library never carries, by family.
    unseen: [f64; 8],
    people: HashMap<u32, f64>,
    franchises: HashMap<u32, f64>,
    /// The sample's mean and standard deviation of similar, and of profile.
    similar: (f64, f64),
    profile: (f64, f64),
}

/// A title's fit, and what it rests on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fitted {
    /// 0…1; one half at about the index's top 2%.
    pub fit: f64,
    /// Its plot's closeness to the nearest liked titles, as a z-score; `None` without a vector.
    pub similar: Option<f64>,
    /// Its profile's lift, as a z-score.
    pub profile: f64,
    /// 0…1.
    pub people: f64,
    /// How much of its similar and profile evidence counted: 1 with a vector.
    pub confidence: f64,
}

impl<'a> Fit<'a> {
    /// The taste of `library`, each title with its signed weight; `None` when it likes nothing, since then no title
    /// fits better than another.
    pub fn new(indexes: &'a Indexes, corpus: &'a Corpus, library: &[(Features, f64)]) -> Option<Fit<'a>> {
        if !library.iter().any(|&(_, weight)| weight > 0.0) {
            return None;
        }
        let (mut counts, mut carrying) = (HashMap::new(), [0.0; 8]);
        let (mut liked, mut disliked, mut people, mut franchises) =
            (Vec::new(), Vec::new(), HashMap::new(), HashMap::new());
        for (features, weight) in library {
            let weight = *weight;
            match features.row {
                Some(row) if weight > 0.0 => liked.push((row, weight)),
                Some(row) => disliked.push(row),
                None => {}
            }
            if let Some(franchise) = features.franchise {
                *franchises.entry(franchise).or_insert(0.0) += weight;
            }
            // A dislike takes away from a value's count, never below none.
            for &(value, confidence) in &features.values {
                *counts.entry(value).or_insert(0.0) += weight * confidence;
            }
            if weight > 0.0 {
                for (family, carried) in carrying.iter_mut().enumerate() {
                    if features.has(family) {
                        *carried += weight;
                    }
                }
                for &(person, share) in &features.people {
                    *people.entry(person).or_insert(0.0) += weight * share;
                }
            }
        }
        let lift = counts
            .into_iter()
            .map(|(value, count): (Value, f64)| {
                let share = corpus.share(value);
                let blended =
                    (count.max(0.0) + LIFT_PRIOR * share) / ((carrying[value.0] + LIFT_PRIOR) * share);
                (value, blended.ln())
            })
            .collect();
        let unseen = carrying.map(|carried| (LIFT_PRIOR / (carried + LIFT_PRIOR)).ln());
        let mut fit = Fit {
            indexes,
            corpus,
            liked,
            disliked,
            lift,
            unseen,
            people,
            franchises,
            similar: (0.0, 1.0),
            profile: (0.0, 1.0),
        };
        let similar: Vec<f64> = corpus.sample.iter().filter_map(|f| fit.nearest(f.row?)).collect();
        let profile: Vec<f64> = corpus.sample.iter().map(|f| fit.lifted(f)).collect();
        fit.similar = spread(&similar);
        fit.profile = spread(&profile);
        Some(fit)
    }

    pub fn of(&self, features: &Features) -> Fitted {
        let profile = z(self.lifted(features), self.profile);
        let people = self.following(features);
        let similar = features.row.and_then(|row| self.nearest(row)).map(|s| z(s, self.similar));
        let mut evidence = match similar {
            Some(similar) => W_SIMILAR * similar + W_PROFILE * profile,
            None => profile,
        };
        let confidence = if similar.is_some() {
            1.0
        } else {
            THIN * [GENRE, LANGUAGE, COUNTRY].iter().filter(|&&family| features.has(family)).count() as f64
                / 3.0
        };
        evidence *= confidence;
        if let (Some(similar), Some(row)) = (similar, features.row) {
            if let Some(against) = self.repelled(row).map(|s| z(s, self.similar)) {
                evidence -= REPULSION * (against - similar).max(0.0);
            }
        }
        let mut fit = 1.0 / (1.0 + (MIDPOINT - evidence - W_PEOPLE * people).exp());
        if features.franchise.is_some_and(|f| self.franchises.get(&f).copied().unwrap_or(0.0) > 0.0) {
            fit += (1.0 - fit) * FRANCHISE_LIFT;
        }
        Fitted { fit, similar, profile, people, confidence }
    }

    /// What fit makes of a title before its plot is read: its profile and its people, weighed as `of` weighs them. The
    /// plot's nearest-liked search is most of what a fit costs, so a large set is narrowed on this first.
    pub fn sketch(&self, features: &Features) -> f64 {
        W_PROFILE * z(self.lifted(features), self.profile) + W_PEOPLE * self.following(features)
    }

    /// The weighted mean closeness of a row to its nearest liked titles, less its closeness to the index as a whole.
    fn nearest(&self, row: u32) -> Option<f64> {
        if self.liked.is_empty() {
            return None;
        }
        let plot = &self.indexes.plot;
        let mut near: Vec<(f64, f64)> =
            self.liked.iter().map(|&(liked, w)| (plot.similarity(row, liked), w)).collect();
        near.sort_by(|a, b| b.0.total_cmp(&a.0));
        near.truncate(NEAREST);
        let weight: f64 = near.iter().map(|&(_, w)| w).sum();
        let hub = plot.projection(row, &self.corpus.centre);
        Some(near.iter().map(|&(s, w)| w * s).sum::<f64>() / weight - hub)
    }

    /// A row's closeness to the nearest disliked title, less its closeness to the index as a whole.
    fn repelled(&self, row: u32) -> Option<f64> {
        let plot = &self.indexes.plot;
        let nearest =
            self.disliked.iter().map(|&disliked| plot.similarity(row, disliked)).max_by(f64::total_cmp)?;
        Some(nearest - plot.projection(row, &self.corpus.centre))
    }

    fn lifted(&self, features: &Features) -> f64 {
        let mut families = [(0.0, 0.0); 8];
        for &(value, confidence) in &features.values {
            let lift = self.lift.get(&value).copied().unwrap_or(self.unseen[value.0]);
            families[value.0].0 += confidence * lift;
            families[value.0].1 += confidence;
        }
        families
            .iter()
            .zip(FAMILY_WEIGHTS)
            .filter(|((_, total), _)| *total > 0.0)
            .map(|(&(sum, total), weight)| weight * sum / total)
            .sum()
    }

    fn following(&self, features: &Features) -> f64 {
        let met: f64 = features
            .people
            .iter()
            .filter_map(|&(person, share)| {
                let followed = self.people.get(&person)?;
                let credits = f64::from(self.corpus.credits.get(&person).copied().unwrap_or(1).max(1));
                Some(followed * share * (1.0 + self.corpus.titles / credits).ln())
            })
            .sum();
        1.0 - (-met / PEOPLE_SCALE).exp()
    }
}

/// The keys of `dated` (sorted by first day) whose span of days touches `from..=to`.
fn released_between(dated: &[(i64, i64, Key)], from: i64, to: i64) -> impl Iterator<Item = Key> + '_ {
    // No date spans more than a year, so nothing starting earlier than a year before `from` can reach it.
    let start = dated.partition_point(|&(first, _, _)| first < from - 366);
    let end = dated.partition_point(|&(first, _, _)| first <= to);
    dated[start..end.max(start)]
        .iter()
        .filter(move |&&(first, span, _)| first + span > from)
        .map(|&(_, _, key)| key)
}

fn z(value: f64, (mean, sd): (f64, f64)) -> f64 {
    (value - mean) / sd
}

/// Mean and standard deviation, the deviation never zero.
fn spread(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 1.0);
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let sd = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
    (mean, sd.max(1e-9))
}

/// A label's name as a family value: FNV-1a.
fn hashed(name: &str) -> u64 {
    name.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::{write_fixture, IndexQueries};
    use std::sync::Arc;

    const ONE: Key = (MediaType::Movie, 1);
    const TWO: Key = (MediaType::Movie, 2);
    const THREE: Key = (MediaType::Movie, 3);

    /// The route fixture: movies 1 and 2 have nearly the same plot vector, movie 3 another.
    async fn indexes(name: &str) -> Arc<Indexes> {
        let dir = std::env::temp_dir().join(format!("den-atlas-fit-{name}-{}", std::process::id()));
        IndexQueries::new(&write_fixture(&dir)).get(|| ()).await.unwrap().0
    }

    fn features(indexes: &Indexes, key: Key) -> Features {
        Features::of(indexes, key, &Knowledge { indexes }.title(key, None, None))
    }

    #[test]
    fn a_title_is_released_between_two_days_when_any_day_it_could_be_falls_between() {
        // Sorted by first day: a dated day, a year known only as a year, and a day well after.
        let dated = [(100, 1, ONE), (200, 365, TWO), (900, 1, THREE)];
        let between = |from, to| released_between(&dated, from, to).collect::<Vec<_>>();
        assert_eq!(between(100, 100), vec![ONE], "both ends are inclusive");
        assert_eq!(between(101, 500), vec![TWO], "a year counts while any of its days does");
        assert_eq!(between(564, 564), vec![TWO], "its last day still counts");
        assert_eq!(between(565, 899), Vec::<Key>::new(), "the day after it does not");
        assert_eq!(between(0, 1000), vec![ONE, TWO, THREE]);
        assert_eq!(between(1000, 0), Vec::<Key>::new(), "an empty span holds nothing");
    }

    #[tokio::test]
    async fn a_title_close_to_what_was_liked_fits_better_than_one_far_from_it() {
        let indexes = indexes("near").await;
        let taste = Fit::new(&indexes, indexes.corpus(), &[(features(&indexes, ONE), 1.0)]).unwrap();
        let (near, far) = (taste.of(&features(&indexes, TWO)), taste.of(&features(&indexes, THREE)));
        assert!(near.fit > far.fit, "{near:?} {far:?}");
        assert!(near.similar > far.similar);
        assert_eq!(near.confidence, 1.0);
    }

    #[tokio::test]
    async fn nothing_liked_is_no_taste_and_a_title_without_a_vector_leans_on_what_is_known_of_it() {
        let indexes = indexes("thin").await;
        assert!(Fit::new(&indexes, indexes.corpus(), &[(features(&indexes, ONE), -1.5)]).is_none());
        let taste = Fit::new(&indexes, indexes.corpus(), &[(features(&indexes, ONE), 1.0)]).unwrap();
        let unknown = taste.of(&Features::of(&indexes, (MediaType::Movie, 77), &Title::default()));
        assert_eq!((unknown.similar, unknown.confidence), (None, 0.0));
    }

    #[tokio::test]
    async fn a_dislike_pushes_away_what_is_close_to_it() {
        let indexes = indexes("dislike").await;
        let liked = || (features(&indexes, THREE), 1.0);
        let plain = Fit::new(&indexes, indexes.corpus(), &[liked()]).unwrap().of(&features(&indexes, TWO));
        let disliking = Fit::new(&indexes, indexes.corpus(), &[liked(), (features(&indexes, ONE), -1.5)])
            .unwrap()
            .of(&features(&indexes, TWO));
        assert!(disliking.fit < plain.fit, "{disliking:?} {plain:?}");
    }
}
