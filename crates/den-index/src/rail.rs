//! The per-title signals More Like This ranks on, read straight out of the store: the `Facets` and
//! `Authorship` the scorer takes.
//!
//! Here, beside the scorer, so everything that ranks a row — den-atlas serving, its tools, and a build of
//! this crate in a browser — reads the store through one implementation. They were den-atlas's until the
//! browser needed them too, and a second copy is how the two would have drifted.
//!
//! What each signal is, and why it exists, is in `den-dataset/scripts/v2/build_rail_facets.py` and
//! den-spec `wire/store-v1.md`. The short version: the vectors say what a title is ABOUT, the labels say
//! what KIND it is, and these say how it is TOLD (the facet choices), which world it is set in (`world`),
//! what it is made of (the taxonomy nouls), what it ARGUES (the critique axes), and who made it.
//!
//! # Nothing is parsed here
//!
//! This used to read a 50 MB JSON blob into `HashMap`s of owned `String`s — 47,529 titles × 12 axes × 2
//! strings, about 1.1M allocations representing 188 distinct values. Now every per-title read is an index
//! into a mapped column, and the only owned state is the aggregates below, which cannot be columns
//! because they are corpus-wide statistics.

use crate::{Axis, MediaType, SimilarParams, ValueId, Weighted};
use den_store::{Row, Store, StoreError};
use std::collections::HashMap;

/// The critique level at or above which a title "holds" an axis, for the idf `ln(N / holders)`. Baked into
/// the aggregates, so a request with another value (`SimilarParams::holds`) needs aggregates built for it.
pub(crate) const HOLDS: f64 = 0.7;

// The floors `build_rail_facets.py` applied when it wrote the JSON blob, re-applied HERE because the store
// deliberately keeps full fidelity: a value below a floor is real data the store should hold and the scorer
// should ignore, and baking a ranking decision into the artifact is what made the old blob impossible to
// re-tune without a rebuild. The noul and world floors are per-request knobs in `SimilarParams`; the
// critique floor is here because the centering mean is built with it.
//
// They are not cosmetic. Dropping the noul floor took the corpus mean noul cosine from 0.38 to 0.51 and
// changed 5 of The Wire's top 20 — the shared-baseline problem the critique centering exists to avoid,
// reintroduced in a different term. `world` below its floor made 50% of rows move.
/// A critique axis below this says nothing about what the work argues. The centering mean is built with
/// it, so another value (`SimilarParams::critique_floor`) needs aggregates built for it too.
pub(crate) const CRITIQUE_FLOOR: f64 = 0.10;

/// How the store encodes a media type in the high half of its packed key.
fn media_code(media: MediaType) -> u8 {
    match media {
        MediaType::Movie => 0,
        MediaType::Tv => 1,
    }
}

/// A title's row: binary search over the keys column, the same as `Store::row_of`, but over a slice
/// resolved once rather than looked up again for every candidate.
fn row_in(keys: &[u64], media: u8, tmdb_id: u32) -> Option<Row> {
    let want = (u64::from(media) << 32) | u64::from(tmdb_id);
    keys.binary_search(&want).ok().map(Row)
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
}

impl RailAggregates {
    /// Production's aggregates. One pass over the store; called once, at load.
    pub fn build(store: &Store<'_>) -> Result<Self, String> {
        Self::build_with(store, CRITIQUE_FLOOR, HOLDS)
    }

    /// The aggregates for another critique floor and holds share — the same pass, with those two values.
    /// A pass over the whole corpus, so a caller serving many requests with the same values keeps them.
    pub fn build_with(store: &Store<'_>, critique_floor: f64, holds: f64) -> Result<Self, String> {
        let keys = store.per_row::<u64>("keys").map_err(|e| e.to_string())?;
        let facet_v = store.column::<u32>("facet_v").map_err(|e| e.to_string())?;
        let facet_c = store.column::<u8>("facet_c").map_err(|e| e.to_string())?;
        let critique = store.column::<u8>("critique").map_err(|e| e.to_string())?;
        let critique_names = store.column::<u32>("critique_names").map_err(|e| e.to_string())?;
        let axes = critique_names.len();
        let rows = keys.len();

        // Checked once, here, rather than indexed per row: a short section used to panic where every
        // other read in this file returns. The same for the facet arrays, whose stride is the only
        // thing in the store with no self-describing names — a writer that added a 13th axis would make
        // every row read misaligned but still-valid ids, which nothing else would notice.
        if critique.len() != rows * axes {
            return Err(format!("critique holds {} cells for {rows} rows x {axes} axes", critique.len()));
        }
        let facet_axes = den_store::FACET_AXES.len();
        if facet_v.len() != rows * facet_axes || facet_c.len() != rows * facet_axes {
            return Err(format!(
                "facets hold {}/{} cells for {rows} rows x {facet_axes} axes",
                facet_v.len(),
                facet_c.len()
            ));
        }

        let mut out = RailAggregates::default();
        let mut totals: HashMap<u8, f64> = HashMap::new();
        let mut counts: HashMap<(u8, Axis, ValueId), f64> = HashMap::new();
        let mut above: HashMap<(u8, ValueId), f64> = HashMap::new();
        let mut sums: HashMap<(u8, ValueId), f64> = HashMap::new();

        for (row, &key) in keys.iter().enumerate() {
            let media = (key >> 32) as u8;
            *totals.entry(media).or_insert(0.0) += 1.0;

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
                // Floored, like the read path: an axis the scorer never sees must not move the mean it
                // is centered against.
                let raw = f64::from(critique[row * axes + axis]) / 100.0;
                let p = if raw >= critique_floor { raw } else { 0.0 };
                *sums.entry((media, name)).or_insert(0.0) += p;
                if p >= holds {
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
            // `libm::log`, not `f64::ln`: see den-index's Cargo.toml.
            out.idf.insert((*media, *name), libm::log(total / n.max(1.0)).max(0.0));
        }
        for ((media, name), sum) in sums {
            let total = totals.get(&media).copied().unwrap_or(1.0).max(1.0);
            out.critique_mean.insert((media, name), sum / total);
        }
        Ok(out)
    }
}

/// One seed's view of the store — the shape `Facets` wants.
///
/// Holds the COLUMNS, not the store. `Store::column` finds a section by scanning the section table and
/// comparing 16-byte names, and every method below is called once per CANDIDATE — `pool_k` is 400, so
/// looking a column up per call cost roughly fifteen hundred name comparisons per candidate to re-find
/// slices that cannot move while the store is mapped.
pub struct SeedFacets<'a> {
    agg: &'a RailAggregates,
    media: u8,
    keys: &'a [u64],
    facet_v: &'a [u32],
    facet_c: &'a [u8],
    critique_names: &'a [u32],
    critique: &'a [u8],
    world: &'a [u8],
    noul_names: &'a [u32],
    noul_k: den_store::List<'a, u8>,
    noul_v: den_store::List<'a, u8>,
    /// The card's release year, for the year term. Empty when the store lacks the section; the term is
    /// then 0 for every title, which is also what production weighs it at (`w_year = 0`).
    card_year: &'a [i16],
    /// The per-request floors, from `SimilarParams` (`tuned`); production's unless overridden.
    noul_floor: f64,
    world_floor: f64,
    defining: f64,
    /// Must be the floor the aggregates were built with, or the centering mean is over other values.
    critique_floor: f64,
}

impl<'a> SeedFacets<'a> {
    /// The same columns, read with the floors of `params` rather than production's. The aggregates must be
    /// the ones built for `params.critique_floor` and `params.holds`.
    pub fn tuned(self, params: &SimilarParams) -> Self {
        SeedFacets {
            noul_floor: params.noul_floor,
            world_floor: params.world_floor,
            defining: params.defining,
            critique_floor: params.critique_floor,
            ..self
        }
    }

    /// Resolve every column the rail reads, once.
    ///
    /// All-or-nothing on purpose: a rail that dropped only the absent signal would score some titles on
    /// fewer terms than others and still return twenty confident-looking answers.
    ///
    /// This is also the ONLY list of what the rail needs. den-atlas's load gate calls this constructor
    /// rather than keeping a second list of section names in step with it, so there is no way to add a
    /// column here and forget to require it there.
    pub fn new(store: &Store<'a>, agg: &'a RailAggregates, media: MediaType) -> Result<Self, StoreError> {
        let production = SimilarParams::default();
        Ok(SeedFacets {
            noul_floor: production.noul_floor,
            world_floor: production.world_floor,
            defining: production.defining,
            critique_floor: production.critique_floor,
            agg,
            media: media_code(media),
            keys: store.per_row::<u64>("keys")?,
            facet_v: store.column::<u32>("facet_v")?,
            facet_c: store.column::<u8>("facet_c")?,
            critique_names: store.column::<u32>("critique_names")?,
            critique: store.column::<u8>("critique")?,
            world: store.per_row::<u8>("world")?,
            noul_names: store.column::<u32>("noul_names")?,
            noul_k: store.list::<u8>("noul_k_v", "noul_k_o")?,
            noul_v: store.list::<u8>("noul_v_v", "noul_v_o")?,
            // Optional, unlike the rest: a term production does not weigh cannot be a reason to refuse a
            // store, and the producer has been dropping card sections (oxyc/den#118).
            card_year: store.per_row::<i16>("card_year").unwrap_or(&[]),
        })
    }

    fn row(&self, tmdb_id: u32) -> Option<Row> {
        row_in(self.keys, self.media, tmdb_id)
    }

    /// The raw critique profile of a row, as (name id, probability), floored.
    ///
    /// Empty when the row holds nothing above the floor — "unknown is not none" is the rule the whole
    /// scorer is built on, and a blank row that returned seventeen zeros scored a real negative cosine
    /// against the centered mean instead of contributing nothing.
    fn critique_at(&self, row: Row) -> Vec<Weighted> {
        let axes = self.critique_names.len();
        self.critique_names
            .iter()
            .enumerate()
            .filter_map(|(i, &name)| {
                let p = f64::from(*self.critique.get(row.0 * axes + i)?) / 100.0;
                (p >= self.critique_floor).then_some((name, p))
            })
            .collect()
    }
}

impl crate::Facets for SeedFacets<'_> {
    fn facets(&self, tmdb_id: u32) -> Vec<(Axis, ValueId, f64)> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let n = den_store::FACET_AXES.len();
        (0..n)
            .filter_map(|axis| {
                let at = row.0 * n + axis;
                let (&value, &conf) = (self.facet_v.get(at)?, self.facet_c.get(at)?);
                (value != den_store::NONE_U32 && conf > 0).then_some((
                    axis as Axis,
                    value,
                    f64::from(conf) / 100.0,
                ))
            })
            .collect()
    }

    fn prevalence(&self, axis: Axis, value: ValueId) -> f64 {
        self.agg.prevalence.get(&(self.media, axis, value)).copied().unwrap_or(1.0)
    }

    fn world(&self, tmdb_id: u32) -> f64 {
        let Some(row) = self.row(tmdb_id) else { return 0.0 };
        let raw = self.world.get(row.0).map_or(0.0, |&v| f64::from(v) / 100.0);
        if raw >= self.world_floor {
            raw
        } else {
            0.0
        }
    }

    fn year(&self, tmdb_id: u32) -> Option<f64> {
        let &year = self.card_year.get(self.row(tmdb_id)?.0)?;
        (year != den_store::NONE_I16).then(|| f64::from(year))
    }

    fn nouls(&self, tmdb_id: u32) -> Vec<Weighted> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let (ks, vs) = (self.noul_k.get(row), self.noul_v.get(row));
        ks.iter()
            .zip(vs)
            .filter_map(|(&k, &v)| {
                let p = f64::from(v) / 100.0;
                (p >= self.noul_floor).then(|| self.noul_names.get(k as usize).map(|&name| (name, p)))?
            })
            .collect()
    }

    fn critique(&self, tmdb_id: u32) -> Vec<Weighted> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let media = self.media;
        // Centered here rather than stored centered, because the mean is a property of the corpus and the
        // store is a property of a title.
        //
        // Centering applies to the axes that survive CRITIQUE_FLOOR, not to all seventeen. An axis the
        // title reads near zero on is dropped before this, so it is skipped by the cosine's name
        // intersection rather than centered to -mean — and a row that is blank throughout contributes
        // nothing instead of scoring a real negative against the mean. "Unknown is not none" is the rule;
        // a floored axis is unknown.
        self.critique_at(row)
            .into_iter()
            .map(|(name, p)| (name, p - self.agg.critique_mean.get(&(media, name)).copied().unwrap_or(0.0)))
            .collect()
    }

    fn critique_raw(&self, tmdb_id: u32) -> Vec<Weighted> {
        self.row(tmdb_id).map(|row| self.critique_at(row)).unwrap_or_default()
    }

    fn critique_defining(&self, tmdb_id: u32) -> Vec<Weighted> {
        let media = self.media;
        self.critique_raw(tmdb_id)
            .into_iter()
            .filter(|(_, p)| *p >= self.defining)
            .filter_map(|(name, _)| {
                let w = self.agg.idf.get(&(media, name)).copied().unwrap_or(0.0);
                (w > 0.0).then_some((name, w))
            })
            .collect()
    }
}

/// One seed's authorship, out of the store's credit lists: who made it, where it lived, and what shares
/// either.
///
/// Makers are the `makers_*` list — directors, creators and screenwriters — and homes the
/// `broadcasters_*` list. Both hold indices into the entity table, and are compared as the Q-ids those
/// indices name, which is what den-atlas's facts compared when authorship was read from them: an index
/// the table cannot resolve names nobody and counts for nothing, on either side of a share.
pub struct SeedAuthorship<'a> {
    media: u8,
    keys: &'a [u64],
    ent_qid: &'a [u32],
    makers: den_store::List<'a, u32>,
    broadcasters: den_store::List<'a, u32>,
    /// The seed's own makers and homes, as Q-ids.
    mine_makers: Vec<u32>,
    mine_homes: Vec<u32>,
}

impl<'a> SeedAuthorship<'a> {
    /// `tmdb_id`'s authorship; a title the store does not hold has none, and nominates and matches nothing.
    pub fn of(store: &Store<'a>, media: MediaType, tmdb_id: u32) -> Result<Self, StoreError> {
        let mut out = SeedAuthorship {
            media: media_code(media),
            keys: store.per_row::<u64>("keys")?,
            ent_qid: store.column::<u32>("ent_qid")?,
            makers: store.list::<u32>("makers_v", "makers_o")?,
            broadcasters: store.list::<u32>("broadcasters_v", "broadcasters_o")?,
            mine_makers: Vec::new(),
            mine_homes: Vec::new(),
        };
        if let Some(row) = row_in(out.keys, out.media, tmdb_id) {
            out.mine_makers = out.qids(out.makers.get(row)).collect();
            out.mine_homes = out.qids(out.broadcasters.get(row)).collect();
        }
        Ok(out)
    }

    fn qids<'s>(&'s self, entities: &'s [u32]) -> impl Iterator<Item = u32> + 's {
        entities.iter().filter_map(|&i| self.ent_qid.get(i as usize).copied())
    }

    /// Share of `mine` found among `theirs` (entity indices), 0..=1.
    fn share(&self, mine: &[u32], theirs: &[u32]) -> f64 {
        if mine.is_empty() || self.qids(theirs).next().is_none() {
            return 0.0;
        }
        let hit = mine.iter().filter(|m| self.qids(theirs).any(|q| q == **m)).count();
        hit as f64 / mine.len() as f64
    }

    fn list_of(&self, list: &den_store::List<'a, u32>, tmdb_id: u32) -> &'a [u32] {
        row_in(self.keys, self.media, tmdb_id).map_or(&[], |row| list.get(row))
    }
}

impl crate::Authorship for SeedAuthorship<'_> {
    /// Every title of the seed's type crediting one of its makers, by id — the seed's own siblings,
    /// whatever the vectors think of them. The Wire and The Deuce share a creator and The Deuce is premise
    /// rank 764, plot 628, outside any sane pool.
    ///
    /// Makers only. Nominating everything sharing a HOME floods the pool — HBO alone is 131 titles — and
    /// measured worse: it kept The Deuce but pushed Show Me a Hero out entirely, and raised mean
    /// same-genre share from 46% to 48%. A home is where a title lived, not evidence that it is the same
    /// kind of thing.
    fn nominate(&self) -> Vec<u32> {
        if self.mine_makers.is_empty() {
            return Vec::new();
        }
        // Keys are sorted, so one type's ids come out ascending.
        self.keys
            .iter()
            .enumerate()
            .filter(|&(row, &key)| {
                (key >> 32) as u8 == self.media
                    && self.qids(self.makers.get(Row(row))).any(|q| self.mine_makers.contains(&q))
            })
            .map(|(_, &key)| key as u32)
            .collect()
    }

    fn makers(&self, tmdb_id: u32) -> f64 {
        self.share(&self.mine_makers, self.list_of(&self.makers, tmdb_id))
    }

    fn home(&self, tmdb_id: u32) -> f64 {
        self.share(&self.mine_homes, self.list_of(&self.broadcasters, tmdb_id))
    }
}
