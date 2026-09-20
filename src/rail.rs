//! The per-title signals More Like This ranks on, and the traits `den_index` asks for them through.
//!
//! `den-index` deliberately knows nothing about facts or the dataset — it holds vectors and labels. So the
//! scorer takes `Authorship` and `Facets`, and this module is where den-atlas answers them: authorship out
//! of the facts file it already loads, the rest out of `rail-facets-<version>.json`.
//!
//! What that blob carries, and why each part exists, is in `den-dataset/scripts/v2/build_rail_facets.py`.
//! The short version: the vectors say what a title is ABOUT, the labels say what KIND it is, and these say
//! how it is TOLD (the facet choices), which world it is set in (`__world`), what it is made of (the 75
//! nouls, of which `labels-t02.json` ships only a thresholded top three) and what it ARGUES (the critique
//! axes).

use den_index::MediaType;
use std::collections::HashMap;
use std::path::Path;

use crate::facts::Facts;

/// A title, in the key both the blob and the indexes use.
type Title = (MediaType, u32);
/// One title's facet choices: `axis`, `value`, the model's confidence.
type Choices = Vec<(String, String, f64)>;
/// A named probability per axis — the nouls and the critique profile share this shape.
type Weights = Vec<(String, f64)>;

/// One media type's rail facets, read once at load.
#[derive(Default)]
pub struct RailFacets {
    /// `axis -> (value, confidence)` per title.
    choices: HashMap<Title, Choices>,
    /// How far from a realist world, 0..=1.
    world: HashMap<Title, f64>,
    /// The 75 taxonomy nouls.
    nouls: HashMap<Title, Weights>,
    /// The 17 critique axes, raw.
    critique_raw: HashMap<Title, Weights>,
    /// The same, centered on the per-axis corpus mean within the title's own media type.
    critique: HashMap<Title, Weights>,
    /// Share of this media type carrying an axis value, for rarity weighting.
    prevalence: HashMap<(MediaType, String, String), f64>,
    /// `ln(N / titles >= 0.7 on this axis)`, per media type.
    idf: HashMap<(MediaType, String), f64>,
}

fn media_of(key: &str) -> Option<Title> {
    let (kind, id) = key.split_once(':')?;
    let media = match kind {
        "tv" => MediaType::Tv,
        "movie" => MediaType::Movie,
        _ => return None,
    };
    Some((media, id.parse().ok()?))
}

impl RailFacets {
    /// Read the blob. A malformed file degrades to no rail facets rather than failing the load: the rail
    /// still works on vectors and labels, which is how it worked before this existed.
    pub fn load(path: &Path) -> Option<RailFacets> {
        let raw = std::fs::read(path).ok()?;
        let parsed: HashMap<String, serde_json::Value> = serde_json::from_slice(&raw).ok()?;
        let mut out = RailFacets::default();
        let mut counts: HashMap<(MediaType, String, String), f64> = HashMap::new();
        let mut totals: HashMap<MediaType, f64> = HashMap::new();
        let mut critique_sum: HashMap<(MediaType, String), f64> = HashMap::new();
        let mut above: HashMap<(MediaType, String), f64> = HashMap::new();

        for (key, value) in &parsed {
            let Some(id) = media_of(key) else { continue };
            *totals.entry(id.0).or_insert(0.0) += 1.0;
            let Some(object) = value.as_object() else { continue };
            let mut choices = Vec::new();
            for (axis, entry) in object {
                match axis.as_str() {
                    "__world" => {
                        out.world.insert(id, entry.as_f64().unwrap_or(0.0));
                    }
                    "__nouls" => {
                        out.nouls.insert(id, as_pairs(entry));
                    }
                    "__critique" => {
                        let pairs = as_pairs(entry);
                        for (name, p) in &pairs {
                            *critique_sum.entry((id.0, name.clone())).or_insert(0.0) += p;
                            if *p >= 0.7 {
                                *above.entry((id.0, name.clone())).or_insert(0.0) += 1.0;
                            }
                        }
                        out.critique_raw.insert(id, pairs);
                    }
                    _ => {
                        let (Some(v), Some(c)) = (entry[0].as_str(), entry[1].as_f64()) else { continue };
                        choices.push((axis.clone(), v.to_string(), c));
                        *counts.entry((id.0, axis.clone(), v.to_string())).or_insert(0.0) += 1.0;
                    }
                }
            }
            out.choices.insert(id, choices);
        }

        for ((media, axis, value), n) in counts {
            let total = totals.get(&media).copied().unwrap_or(1.0).max(1.0);
            out.prevalence.insert((media, axis, value), n / total);
        }
        for ((media, axis), n) in &above {
            let total = totals.get(media).copied().unwrap_or(1.0).max(1.0);
            out.idf.insert((*media, axis.clone()), (total / n.max(1.0)).ln().max(0.0));
        }
        // Center the critique profile on the per-axis mean within its media type. Raw cosine over
        // seventeen mostly-low values is dominated by a shared baseline — it scored Oz 0.885 and Angel
        // 0.792 against The Wire, ranking them correctly and separating them by almost nothing. Centered,
        // the same pair is +0.700 and +0.274. Every title gets every axis, so an unanswered one is
        // centered to -mean rather than silently skipped by the cosine's name intersection.
        let mut axes: Vec<(MediaType, String)> = critique_sum.keys().cloned().collect();
        axes.sort();
        axes.dedup();
        let means: HashMap<(MediaType, String), f64> = axes
            .iter()
            .map(|k| {
                let total = totals.get(&k.0).copied().unwrap_or(1.0).max(1.0);
                (k.clone(), critique_sum.get(k).copied().unwrap_or(0.0) / total)
            })
            .collect();
        for (id, raw) in &out.critique_raw {
            let centered = axes
                .iter()
                .filter(|(media, _)| *media == id.0)
                .map(|(_, axis)| {
                    let mine = raw.iter().find(|(n, _)| n == axis).map(|(_, p)| *p).unwrap_or(0.0);
                    (axis.clone(), mine - means.get(&(id.0, axis.clone())).copied().unwrap_or(0.0))
                })
                .collect();
            out.critique.insert(*id, centered);
        }
        Some(out)
    }
}

fn as_pairs(value: &serde_json::Value) -> Weights {
    value
        .as_object()
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect())
        .unwrap_or_default()
}

/// One seed's view of the rail facets — the shape `den_index::Facets` wants.
pub struct SeedFacets<'a> {
    pub rail: &'a RailFacets,
    pub media: MediaType,
}

impl den_index::Facets for SeedFacets<'_> {
    fn facets(&self, tmdb_id: u32) -> Vec<(String, String, f64)> {
        self.rail.choices.get(&(self.media, tmdb_id)).cloned().unwrap_or_default()
    }

    fn prevalence(&self, axis: &str, value: &str) -> f64 {
        self.rail.prevalence.get(&(self.media, axis.to_string(), value.to_string())).copied().unwrap_or(1.0)
    }

    fn world(&self, tmdb_id: u32) -> f64 {
        self.rail.world.get(&(self.media, tmdb_id)).copied().unwrap_or(0.0)
    }

    fn nouls(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        self.rail.nouls.get(&(self.media, tmdb_id)).cloned().unwrap_or_default()
    }

    fn critique(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        self.rail.critique.get(&(self.media, tmdb_id)).cloned().unwrap_or_default()
    }

    fn critique_raw(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        self.rail.critique_raw.get(&(self.media, tmdb_id)).cloned().unwrap_or_default()
    }

    fn critique_defining(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        self.critique_raw(tmdb_id)
            .into_iter()
            .filter(|(_, p)| *p >= 0.8)
            .filter_map(|(axis, _)| {
                let w = self.rail.idf.get(&(self.media, axis.clone())).copied().unwrap_or(0.0);
                (w > 0.0).then_some((axis, w))
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
        let score = |id: &Title| -> f64 {
            let Some(theirs) = self.rail.critique_raw.get(id) else { return 0.0 };
            defining
                .iter()
                .filter_map(|(a, w)| theirs.iter().find(|(name, _)| name == a).map(|(_, p)| w * p))
                .sum::<f64>()
                / total
        };
        let mine = score(&(self.media, other));
        // Only reached for a candidate the tone floor would otherwise cut, so the scan is rare.
        self.rail.critique_raw.keys().filter(|k| k.0 == self.media && score(k) > mine).count() < n
    }
}

/// One seed's authorship, out of the facts file: who made it, where it lived, and what shares either.
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
