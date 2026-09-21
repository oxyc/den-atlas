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

// The three floors `build_rail_facets.py` applied when it wrote the JSON blob, re-applied HERE because
// the store deliberately keeps full fidelity: a value below a floor is real data the store should hold
// and the scorer should ignore, and baking a ranking decision into the artifact is what made the old
// blob impossible to re-tune without a rebuild.
//
// They are not cosmetic. Dropping the noul floor took the corpus mean noul cosine from 0.38 to 0.51 and
// changed 5 of The Wire's top 20 — the shared-baseline problem the critique centering exists to avoid,
// reintroduced in a different term. `world` below its floor made 50% of rows move. Restoring them keeps
// this migration what it claimed to be: the same ranking, read from a different place.
/// A noul below this is noise, not a signal: ~75 dimensions per row become ~15.
const NOUL_FLOOR: f64 = 0.20;
/// Below this a title is simply realist; the distance is not meaningful.
const WORLD_FLOOR: f64 = 0.05;
/// A critique axis below this says nothing about what the work argues.
const CRITIQUE_FLOOR: f64 = 0.10;

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
                // Floored, like the read path: an axis the scorer never sees must not move the mean it
                // is centered against.
                let raw = f64::from(critique[row * axes + axis]) / 100.0;
                let p = if raw >= CRITIQUE_FLOOR { raw } else { 0.0 };
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
        Ok(out)
    }
}

/// One seed's view of the store — the shape `den_index::Facets` wants.
///
/// Holds the COLUMNS, not the store. `Store::column` finds a section by scanning the section table and
/// comparing 16-byte names, and every method below is called once per CANDIDATE — `POOL_K` is 400, so
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
}

impl<'a> SeedFacets<'a> {
    /// Resolve every column the rail reads, once.
    ///
    /// `None` when any one of them is missing or the wrong width. All-or-nothing on purpose: a rail that
    /// dropped only the absent signal would score some titles on fewer terms than others and still
    /// return twenty confident-looking answers. `MappedStore::check` requires this same set at load, so
    /// `None` here means a store got past that gate.
    pub fn new(store: &Store<'a>, agg: &'a RailAggregates, media: MediaType) -> Option<Self> {
        Some(SeedFacets {
            agg,
            media: media_code(media),
            keys: store.per_row::<u64>("keys").ok()?,
            facet_v: store.column::<u32>("facet_v").ok()?,
            facet_c: store.column::<u8>("facet_c").ok()?,
            critique_names: store.column::<u32>("critique_names").ok()?,
            critique: store.column::<u8>("critique").ok()?,
            world: store.per_row::<u8>("world").ok()?,
            noul_names: store.column::<u32>("noul_names").ok()?,
            noul_k: store.list::<u8>("noul_k_v", "noul_k_o").ok()?,
            noul_v: store.list::<u8>("noul_v_v", "noul_v_o").ok()?,
        })
    }

    /// The row for a title. Binary search over the keys column, the same as `Store::row_of` — done here
    /// so it reads the slice resolved above rather than looking `keys` up again on every candidate.
    fn row(&self, tmdb_id: u32) -> Option<Row> {
        let want = (u64::from(self.media) << 32) | u64::from(tmdb_id);
        self.keys.binary_search(&want).ok().map(Row)
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
                (p >= CRITIQUE_FLOOR).then_some((name, p))
            })
            .collect()
    }
}

impl den_index::Facets for SeedFacets<'_> {
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
        if raw >= WORLD_FLOOR {
            raw
        } else {
            0.0
        }
    }

    fn nouls(&self, tmdb_id: u32) -> Vec<Weighted> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let (ks, vs) = (self.noul_k.get(row), self.noul_v.get(row));
        ks.iter()
            .zip(vs)
            .filter_map(|(&k, &v)| {
                let p = f64::from(v) / 100.0;
                (p >= NOUL_FLOOR).then(|| self.noul_names.get(k as usize).map(|&name| (name, p)))?
            })
            .collect()
    }

    fn critique(&self, tmdb_id: u32) -> Vec<Weighted> {
        let Some(row) = self.row(tmdb_id) else { return Vec::new() };
        let media = self.media;
        // Centered here rather than stored centered, because the mean is a property of the corpus and the
        // store is a property of a title. Every title carries every axis, so an unanswered one centers to
        // -mean rather than being skipped by a cosine's name intersection.
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
        let rows = self.agg.rows_of_media.get(&self.media).unwrap_or(&empty);
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

#[cfg(test)]
mod tests {
    use super::*;
    use den_index::Facets as _;

    /// den-spec's three-title fixture. These tests exist because the first version of this file had
    /// none, and that is exactly what let three thresholds quietly change the ranking: the store still
    /// loaded, every row still returned twelve facets, and the numbers were simply different.
    ///
    /// `crate::store::spec_fixture` fails rather than returns `None` when den-spec is absent, so these
    /// cannot go back to reporting a pass over a store they never opened.
    macro_rules! loaded {
        () => {
            match crate::store::spec_fixture() {
                Some(path) => crate::store::LoadedStore::open(&path).expect("the fixture loads"),
                None => return,
            }
        };
    }

    fn seed(loaded: &crate::store::LoadedStore, media: MediaType) -> SeedFacets<'_> {
        SeedFacets::new(&loaded.view(), &loaded.aggregates, media)
            .expect("the fixture carries every column the rail reads")
    }

    /// The fixture's `movie:1` answers `era` and `tone`, and DECLINES `pacing`. A declined axis must be
    /// absent, not a value: a scorer that counted `does-not-apply` as agreement would pair every
    /// declining title with every other.
    #[test]
    fn facets_carry_confidence_and_declines_are_absent() {
        let loaded = loaded!();
        let facets = seed(&loaded, MediaType::Movie).facets(1);
        let era = den_store::FACET_AXES.iter().position(|a| *a == "era").unwrap() as Axis;
        let pacing = den_store::FACET_AXES.iter().position(|a| *a == "pacing").unwrap() as Axis;

        let (_, _, conf) = facets.iter().find(|(a, _, _)| *a == era).expect("era is answered");
        assert!((conf - 0.96).abs() < 1e-9, "confidence is hundredths, got {conf}");
        assert!(!facets.iter().any(|(a, _, _)| *a == pacing), "a declined axis must not appear");
    }

    /// Prevalence is per media type and per axis. With one movie answering `era`, that value's
    /// prevalence among movies is 1 of 2 movie rows.
    #[test]
    fn prevalence_is_scoped_to_one_media_type() {
        let loaded = loaded!();
        let movies = seed(&loaded, MediaType::Movie);
        let era = den_store::FACET_AXES.iter().position(|a| *a == "era").unwrap() as Axis;
        let (_, value, _) = movies.facets(1).into_iter().find(|(a, _, _)| *a == era).unwrap();

        assert!((movies.prevalence(era, value) - 0.5).abs() < 1e-9, "1 of 2 movie rows");
        // The same value id, asked of the other media type, must not read the movie statistic.
        assert!((seed(&loaded, MediaType::Tv).prevalence(era, value) - 1.0).abs() < 1e-9);
    }

    /// The floors this file re-applies. `movie:1` holds `theme__vampire` at 0.25 (kept) and the fixture
    /// gives it a `world` of 0.25 from that; a value below `WORLD_FLOOR` must read as 0.
    #[test]
    fn nouls_and_world_are_floored() {
        let loaded = loaded!();
        let movies = seed(&loaded, MediaType::Movie);

        assert!(movies.nouls(1).iter().all(|(_, p)| *p >= NOUL_FLOOR), "every noul clears the floor");
        let world = movies.world(1);
        assert!(world == 0.0 || world >= WORLD_FLOOR, "world is floored, got {world}");
        // A row with nothing at all reads as zero distance, not as a missing value.
        assert_eq!(movies.world(2), 0.0);
    }

    /// "Unknown is not none." A row with no critique must return NOTHING, so the cosine term is skipped
    /// — not a negated mean vector that scores a real, usually negative, similarity.
    #[test]
    fn a_row_without_critique_returns_nothing() {
        let loaded = loaded!();
        let movies = seed(&loaded, MediaType::Movie);

        assert!(!movies.critique(1).is_empty(), "movie:1 argues about something");
        assert!(movies.critique_raw(2).is_empty(), "movie:2 has no critique at all");
        assert!(movies.critique(2).is_empty(), "and centering must not manufacture seventeen values for it");
    }

    /// Centering subtracts the per-media mean, so a title above the corpus on an axis reads positive.
    #[test]
    fn critique_is_centered_on_its_own_media_type() {
        let loaded = loaded!();
        let movies = seed(&loaded, MediaType::Movie);
        let raw = movies.critique_raw(1);
        let centered = movies.critique(1);

        assert_eq!(raw.len(), centered.len());
        for ((name, r), (name2, c)) in raw.iter().zip(&centered) {
            assert_eq!(name, name2, "centering preserves order");
            assert!(c <= r, "centering subtracts a non-negative mean: {r} -> {c}");
        }
        assert!(centered.iter().any(|(_, c)| *c > 0.0), "something must be above its own mean");
    }
}
