//! The per-title signals More Like This ranks on, read straight out of the store.
//!
//! `den-index` deliberately knows nothing about facts or the dataset — it holds vectors and labels. So
//! the scorer takes `Authorship` and `Facets`, and this module answers them.
//!
//! What each signal is, and why it exists, is in `den-dataset/scripts/v2/build_rail_facets.py` and
//! den-spec `wire/store-v1.md`. The short version: the vectors say what a title is ABOUT, the labels say
//! what KIND it is, and these say how it is TOLD (the facet choices), which world it is set in (`world`),
//! what it is made of (the taxonomy nouls) and what it ARGUES (the critique axes).
//!
//! # Nothing is parsed here
//!
//! This used to read a 50 MB JSON blob into `HashMap`s of owned `String`s — 47,529 titles × 12 axes × 2
//! strings, about 1.1M allocations representing 188 distinct values, and most of the 552 MB atlas held.
//! Now every per-title read is an index into a mapped column, and the only owned state is the three
//! aggregates below, which cannot be columns because they are corpus-wide statistics.

use den_index::{Axis, MediaType, ValueId, Weighted};
use den_store::{Row, Store};
use std::collections::HashMap;

use crate::facts::Facts;

/// Above this, a title is judged to hold that critique axis, for the idf count.
const DEFINING: f64 = 0.8;
/// The share of a media type holding an axis, for `ln(N / holders)`.
const HOLDS: f64 = 0.7;

/// How the store encodes a media type in the high half of its packed key.
fn media_code(media: MediaType) -> u8 {
    match media {
        MediaType::Movie => 0,
        MediaType::Tv => 1,
    }
}

/// The corpus-wide statistics the scorer needs, computed once at load.
///
/// These are the only things that cannot be read straight from a column: each is an aggregate over every
/// row of a media type, and computing one per request would mean scanning the corpus per request.
#[derive(Default)]
pub struct RailAggregates {
    /// Share of a media type carrying an axis value, for rarity weighting. A shared `chronology = linear`
    /// is worth almost nothing — 76% of titles — where a shared `conflict = person-vs-system` is worth a lot.
    prevalence: HashMap<(u8, Axis, ValueId), f64>,
    /// `ln(N / titles at or above 0.7 on this critique axis)`, per media type.
    idf: HashMap<(u8, ValueId), f64>,
    /// The per-axis mean of the critique profile within a media type.
    ///
    /// Raw cosine over seventeen mostly-low values is dominated by a shared baseline: it scored Oz 0.885
    /// and Angel 0.792 against The Wire, ranking them correctly and separating them by almost nothing.
    /// Centered, the same pair is +0.700 and +0.274.
    critique_mean: HashMap<(u8, ValueId), f64>,
    /// Rows of each media type, so `critique_top`'s scan knows what it is scanning.
    rows_of_media: HashMap<u8, Vec<Row>>,
}

impl RailAggregates {
    /// One pass over the store per statistic. Called once, at load.
    pub fn build(store: &Store<'_>) -> Result<Self, String> {
        let keys = store.per_row::<u64>("keys").map_err(|e| e.to_string())?;
        let facet_v = store.column::<u32>("facet_v").map_err(|e| e.to_string())?;
        let facet_c = store.column::<u8>("facet_c").map_err(|e| e.to_string())?;
        let critique = store.column::<u8>("critique").map_err(|e| e.to_string())?;
        let critique_names = store.column::<u32>("critique_names").map_err(|e| e.to_string())?;
        let axes = critique_names.len();
        let rows = keys.len();

        let mut out = RailAggregates::default();
        let mut totals: HashMap<u8, f64> = HashMap::new();
        let mut counts: HashMap<(u8, Axis, ValueId), f64> = HashMap::new();
        let mut above: HashMap<(u8, ValueId), f64> = HashMap::new();
        let mut sums: HashMap<(u8, ValueId), f64> = HashMap::new();

        for (row, &key) in keys.iter().enumerate() {
            let media = (key >> 32) as u8;
            *totals.entry(media).or_insert(0.0) += 1.0;
            out.rows_of_media.entry(media).or_default().push(Row(row));

            for axis in 0..den_store::FACET_AXES.len() {
                let at = row * den_store::FACET_AXES.len() + axis;
                let (Some(&value), Some(&conf)) = (facet_v.get(at), facet_c.get(at)) else { continue };
                // A declined axis is stored absent, never as a value: a scorer counting "does-not-apply"
                // as agreement would pair every declining title with every other.
                if value == den_store::NONE_U32 || conf == 0 {
                    continue;
                }
                *counts.entry((media, axis as Axis, value)).or_insert(0.0) += 1.0;
            }

            for (axis, &name) in critique_names.iter().enumerate() {
                let p = f64::from(critique[row * axes + axis]) / 100.0;
                *sums.entry((media, name)).or_insert(0.0) += p;
                if p >= HOLDS {
                    *above.entry((media, name)).or_insert(0.0) += 1.0;
                }
            }
        }

        for ((media, axis, value), n) in counts {
            let total = totals.get(&media).copied().unwrap_or(1.0).max(1.0);
            out.prevalence.insert((media, axis, value), n / total);
        }
        for ((media, name), n) in &above {
            let total = totals.get(media).copied().unwrap_or(1.0).max(1.0);
            out.idf.insert((*media, *name), (total / n.max(1.0)).ln().max(0.0));
        }
        for ((media, name), sum) in sums {
            let total = totals.get(&media).copied().unwrap_or(1.0).max(1.0);
            out.critique_mean.insert((media, name), sum / total);
        }
        let _ = rows;
        Ok(out)
    }
}

/// One seed's view of the store — the shape `den_index::Facets` wants.
pub struct SeedFacets<'a> {
    pub store: Store<'a>,
    pub agg: &'a RailAggregates,
    pub media: MediaType,
}

impl SeedFacets<'_> {
    fn media(&self) -> u8 {
        media_code(self.media)
    }

    fn row(&self, tmdb_id: u32) -> Option<Row> {
        self.store.row_of(self.media(), tmdb_id).ok().flatten()
    }

    /// The raw critique profile of a row, as (name id, probability).
    fn critique_at(&self, row: Row) -> Vec<Weighted> {
        let (Ok(names), Ok(values)) =
            (self.store.column::<u32>("critique_names"), self.store.column::<u8>("critique"))
        else {
            return Vec::new();
        };
        let axes = names.len();
        names
            .iter()
            .enumerate()
            .filter_map(|(i, &name)| {
                values.get(row.0 * axes + i).map(|&v| (name, f64::from(v) / 100.0))
            })
            .collect()
    }
}

impl den_index::Facets for SeedFacets<'_> {
    fn facets(&self, tmdb_id: u32) -> Vec<(Axis, ValueId, f64)> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let (Ok(values), Ok(confs)) =
            (self.store.column::<u32>("facet_v"), self.store.column::<u8>("facet_c"))
        else {
            return Vec::new();
        };
        let n = den_store::FACET_AXES.len();
        (0..n)
            .filter_map(|axis| {
                let at = row.0 * n + axis;
                let (&value, &conf) = (values.get(at)?, confs.get(at)?);
                (value != den_store::NONE_U32 && conf > 0)
                    .then_some((axis as Axis, value, f64::from(conf) / 100.0))
            })
            .collect()
    }

    fn prevalence(&self, axis: Axis, value: ValueId) -> f64 {
        self.agg.prevalence.get(&(self.media(), axis, value)).copied().unwrap_or(1.0)
    }

    fn world(&self, tmdb_id: u32) -> f64 {
        let Some(row) = self.row(tmdb_id) else { return 0.0 };
        self.store
            .per_row::<u8>("world")
            .ok()
            .and_then(|w| w.get(row.0).copied())
            .map_or(0.0, |v| f64::from(v) / 100.0)
    }

    fn nouls(&self, tmdb_id: u32) -> Vec<Weighted> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let (Ok(names), Ok(keys), Ok(values)) = (
            self.store.column::<u32>("noul_names"),
            self.store.list::<u8>("noul_k_v", "noul_k_o"),
            self.store.list::<u8>("noul_v_v", "noul_v_o"),
        ) else {
            return Vec::new();
        };
        let (ks, vs) = (keys.get(row), values.get(row));
        ks.iter()
            .zip(vs)
            .filter_map(|(&k, &v)| names.get(k as usize).map(|&name| (name, f64::from(v) / 100.0)))
            .collect()
    }

    fn critique(&self, tmdb_id: u32) -> Vec<Weighted> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let media = self.media();
        // Centered here rather than stored centered, because the mean is a property of the corpus and the
        // store is a property of a title. Every title carries every axis, so an unanswered one centers to
        // -mean rather than being skipped by a cosine's name intersection.
        self.critique_at(row)
            .into_iter()
            .map(|(name, p)| {
                (name, p - self.agg.critique_mean.get(&(media, name)).copied().unwrap_or(0.0))
            })
            .collect()
    }

    fn critique_raw(&self, tmdb_id: u32) -> Vec<Weighted> {
        self.row(tmdb_id).map(|row| self.critique_at(row)).unwrap_or_default()
    }

    fn critique_defining(&self, tmdb_id: u32) -> Vec<Weighted> {
        let media = self.media();
        self.critique_raw(tmdb_id)
            .into_iter()
            .filter(|(_, p)| *p >= DEFINING)
            .filter_map(|(name, _)| {
                let w = self.agg.idf.get(&(media, name)).copied().unwrap_or(0.0);
                (w > 0.0).then_some((name, w))
            })
            .collect()
    }

    fn critique_top(&self, seed: u32, other: u32, n: usize) -> bool {
        let defining = self.critique_defining(seed);
        if defining.is_empty() {
            return false;
        }
        let total: f64 = defining.iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            return false;
        }
        let score = |row: Row| -> f64 {
            let theirs = self.critique_at(row);
            defining
                .iter()
                .filter_map(|(a, w)| theirs.iter().find(|(name, _)| name == a).map(|(_, p)| w * p))
                .sum::<f64>()
                / total
        };
        let Some(other_row) = self.row(other) else { return false };
        let mine = score(other_row);
        let empty = Vec::new();
        let rows = self.agg.rows_of_media.get(&self.media()).unwrap_or(&empty);
        // Only reached for a candidate the tone floor would otherwise cut, so the scan is rare.
        rows.iter().filter(|&&row| score(row) > mine).count() < n
    }
}

/// One seed's authorship, out of the facts: who made it, where it lived, and what shares either.
pub struct SeedAuthorship<'a> {
    pub facts: &'a Facts,
    pub media: MediaType,
    /// The seed's own credited makers and broadcasters, as interned Q-ids.
    pub makers: Vec<u32>,
    pub homes: Vec<u32>,
}

impl<'a> SeedAuthorship<'a> {
    pub fn of(facts: &'a Facts, media: MediaType, tmdb_id: u32) -> SeedAuthorship<'a> {
        let record = facts.get(tmdb_id, media);
        SeedAuthorship {
            facts,
            media,
            makers: record.map(|r| r.makers.clone()).unwrap_or_default(),
            homes: record.map(|r| r.broadcasters.clone()).unwrap_or_default(),
        }
    }

    fn share(mine: &[u32], theirs: &[u32]) -> f64 {
        if mine.is_empty() || theirs.is_empty() {
            return 0.0;
        }
        let hit = mine.iter().filter(|m| theirs.contains(m)).count();
        hit as f64 / mine.len() as f64
    }
}

impl den_index::Authorship for SeedAuthorship<'_> {
    fn nominate(&self) -> Vec<u32> {
        // Makers only. Nominating everything sharing a HOME floods the pool — HBO alone is 131 titles —
        // and measured worse: it kept The Deuce but pushed Show Me a Hero out entirely, and raised mean
        // same-genre share from 46% to 48%. A home is where a title lived, not evidence that it is the
        // same kind of thing.
        if self.makers.is_empty() {
            return Vec::new();
        }
        self.facts.titles_sharing_makers(self.media, &self.makers)
    }

    fn makers(&self, tmdb_id: u32) -> f64 {
        let theirs = self.facts.get(tmdb_id, self.media).map(|r| r.makers.as_slice()).unwrap_or(&[]);
        Self::share(&self.makers, theirs)
    }

    fn home(&self, tmdb_id: u32) -> f64 {
        let theirs = self.facts.get(tmdb_id, self.media).map(|r| r.broadcasters.as_slice()).unwrap_or(&[]);
        Self::share(&self.homes, theirs)
    }
}
