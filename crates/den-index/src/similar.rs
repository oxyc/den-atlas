//! More Like This — the index half of the tvOS app's `refineMoreLikeThis`. Merging with TMDB's own
//! recommendations and the theme rerank stay with the client, which holds those.

use crate::{Index, Key, MediaType, ScanStats};
use std::collections::{HashMap, HashSet};

/// Plot neighbours asked for, and the premise candidates weighed before keeping the best of them.
const PLOT_K: usize = 20;
const PREMISE_K: usize = 40;
const KEEP: usize = 20;

/// How long a pooled row may be.
///
/// The rail is scrolled, not glanced at: "any list of titles should return plenty and keep loading on
/// scroll — never a small fixed slice. A hard cap that leaves 20 results where hundreds exist reads as
/// broken/empty." Twenty was that cap. Measured on The Wire, the twenty-first through hundredth results
/// include Generation Kill (45), Homicide: Life on the Street (74), The Sopranos (104) and Deadwood (169) —
/// all real answers that the cap simply threw away.
///
/// The row is computed once and memoised, so serving a later page costs a slice, not a rescore.
pub const MAX_ROW: usize = 200;

/// Neighbour ids for More Like This, best first.
///
/// The premise index leads when it holds the title: its nearest 40, never mixing animated with live action
/// (premise tags are audience-blind), a hit the plot index also likes lifted by a quarter, one of another
/// primary genre lowered by a quarter, then the best 20. Otherwise the plot index's nearest 20. Empty when
/// neither index holds the title — the client then embeds its synopsis instead.
pub fn more_like_this(
    plot: Option<&Index>,
    premise: Option<&Index>,
    tmdb_id: u32,
    media_type: MediaType,
) -> Vec<u32> {
    let plot_ids: Vec<u32> = plot.map_or_else(Vec::new, |p| {
        p.nearest(tmdb_id, media_type, PLOT_K).into_iter().map(|n| n.tmdb_id).collect()
    });
    let Some(premise) = premise else { return plot_ids };
    let Some(mine) = premise.labels(tmdb_id, media_type) else { return plot_ids };
    let agreeing: HashSet<u32> = plot_ids.iter().copied().collect();
    let mut scored: Vec<(u32, i32)> = premise
        .nearest(tmdb_id, media_type, PREMISE_K)
        .into_iter()
        .filter_map(|n| {
            let theirs = premise.labels(n.tmdb_id, media_type)?;
            if theirs.animated != mine.animated {
                return None;
            }
            let mut score = n.score;
            if agreeing.contains(&n.tmdb_id) {
                score += n.score / 4;
            }
            if theirs.primary_genre != mine.primary_genre {
                score -= score / 4;
            }
            Some((n.tmdb_id, score))
        })
        .collect();
    // Stable, so equal scores keep the premise index's own order.
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().take(KEEP).map(|(id, _)| id).collect()
}

/// Candidates drawn from EACH index. `nearest` is a full O(n) scan whatever `k` is (`index.rs`), so a wider
/// pool costs only a larger partial sort, not a larger scan.
const POOL_K: usize = 400;
/// How the two spaces are mixed. Premise leads because it discriminates story over subject matter; plot
/// carries the titles premise misses entirely — for The Wire, Homicide: Life on the Street is plot's 10th
/// nearest and premise's 2,880th.
const W_PREMISE: f64 = 0.55;
const W_PLOT: f64 = 0.45;
/// The tonal term, as a fraction of the pool's own score spread, so it is comparable across seeds whose
/// absolute cosines differ.
const W_TONE: f64 = 0.90;
/// A candidate carrying less than this share of the seed's labels is not a neighbour, whatever the vectors
/// say. Bates Motel scores 0.27 against The Wire; Homicide scores 0.77.
const TONE_FLOOR: f64 = 0.35;
/// At most this many titles whose strongest subgenre is the same one. Stops a row of twenty police
/// procedurals without needing a second similarity matrix, which is what MMR would cost.
const SUBGENRE_CAP: usize = 3;
/// Labels below this confidence are noise and are not part of what the seed IS.
const MIN_CONFIDENCE: f64 = 0.55;
/// Whether a row mixes films and series (oxyc/den-atlas#49): Breaking Bad leads to El Camino, The Next
/// Generation to First Contact, Twin Peaks to Fire Walk with Me.
///
/// Chosen with `den-atlas rail-eval` over both judged files (the ideal counts both types' goods, so a row of
/// one type is read against the same ideal as a mixed one):
///
/// ```text
///               dev nDCG   dev nDCG'   dev bad   test nDCG   test nDCG'   test bad
///   one type     0.765      0.775        27        0.703       0.744         33
///   mixed        0.792      0.794        26        0.711       0.745         29
/// ```
///
/// The seed type's own titles keep exactly the order a row of that type alone gives them (`rank_pool`).
const MIX_TYPES: bool = true;
/// A title of the other type this near the seed in its own type's plot neighbours skips the tonal floor.
const CROSS_TOP: u32 = 3;

/// A noul below this is noise, not a signal: ~75 dimensions per row become ~15. Applied by the `Facets`
/// implementation (`rail.rs`), which reads it from `SimilarParams`.
pub const NOUL_FLOOR: f64 = 0.20;
/// Below this a title is simply realist; the distance is not meaningful. Applied like `NOUL_FLOOR`.
pub const WORLD_FLOOR: f64 = 0.05;
/// A critique axis at or above this is one of the seed's DEFINING arguments, for coverage.
pub const DEFINING: f64 = 0.8;

/// Every knob the pooled More Like This scorer reads, in one place.
///
/// `Default` is exactly what ships: each field is read from the named constant above it in this file, whose
/// doc comment says why it has that value. Serving calls `more_like_this_pooled`, which ranks with
/// `SimilarParams::default()`, so a default here cannot drift from production — changing one IS changing
/// production. The tuning playground and any offline scorer take the same type and override fields.
///
/// The critique floor and the idf "holds" share are here too, though they are baked into corpus-wide
/// aggregates (`RailAggregates`): production's are built once at load, and a caller asking for other
/// values builds (and should memoise) aggregates for them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimilarParams {
    pub pool_k: usize,
    pub w_premise: f64,
    pub w_plot: f64,
    pub w_tone: f64,
    pub tone_floor: f64,
    pub min_confidence: f64,
    pub subgenre_cap: usize,
    /// How many leading titles the subgenre cap applies to.
    pub cap_window: usize,
    pub max_row: usize,
    pub w_maker: f64,
    /// A shared character (`Authorship::characters`). Above 0 the linked titles are also nominated into the
    /// pool, as a shared maker's are.
    pub w_character: f64,
    /// A shared origin (country, region, continent) for a seed from outside the English-language
    /// mainstream (`origin_affinity`).
    pub w_region: f64,
    pub w_home: f64,
    pub w_facet: f64,
    pub w_world: f64,
    pub w_noul: f64,
    pub w_critique: f64,
    pub w_coverage: f64,
    /// Never mix animated with live action.
    pub same_animation: bool,
    /// Mix films and series in one row: both types pooled, the other type's cosines read on the seed
    /// type's scale (`z_map`).
    pub mix_types: bool,
    pub noul_floor: f64,
    pub world_floor: f64,
    pub defining: f64,
    /// Release-year proximity (`year_proximity`). Off in production.
    pub w_year: f64,
    pub year_halflife: f64,
    /// A multiplier on `w_facet` per facet axis, in `den_store::FACET_AXES` order (`FACET_AXIS_KNOBS`).
    pub w_facet_axis: [f64; 12],
    /// Drop a candidate TMDB rates below this (0: off). A title with no rating is kept.
    pub min_rating: f64,
    /// Drop a candidate with fewer TMDB votes than this (0: off). A title with no count is kept.
    pub min_votes: f64,
    /// Weight of TMDB export popularity, log-scaled against the pool's most popular (`Audience`). Off.
    pub w_popularity: f64,
    /// Drop a candidate whose TMDB export popularity is below this (0: off). Unknown is kept.
    pub min_popularity: f64,
    /// A critique axis below this says nothing about what the work argues (`RailAggregates`).
    pub critique_floor: f64,
    /// The share of a media type at or above which an axis is "held", for the critique idf.
    pub holds: f64,
    /// The percentile of the pool a missing cosine is scored at.
    pub pool_floor_pct: usize,
    /// The percentiles of the pool's base scores whose difference is `spread`, the unit of every term.
    pub spread_low_pct: usize,
    pub spread_high_pct: usize,
}

/// Production filters on neither TMDB number nor popularity, and does not weigh popularity: More Like This
/// is about the seed, and a popularity term would pull every row towards the same few titles.
const MIN_RATING: f64 = 0.0;
const MIN_VOTES: f64 = 0.0;
const W_POPULARITY: f64 = 0.0;
const MIN_POPULARITY: f64 = 0.0;
/// Production's critique floor and holds share: `rail::CRITIQUE_FLOOR` and `rail::HOLDS`, whose doc
/// comments say why.
const CRITIQUE_FLOOR: f64 = crate::rail::CRITIQUE_FLOOR;
const HOLDS: f64 = crate::rail::HOLDS;
/// A missing cosine is read at the pool's 10th percentile, and the spread is the 90th minus the 10th: wide
/// enough to be stable against a few outliers at either end.
const POOL_FLOOR_PCT: usize = 10;
const SPREAD_LOW_PCT: usize = 10;
const SPREAD_HIGH_PCT: usize = 90;

/// The `pct`th percentile's position in a sorted list of `len`: `len * pct / 100`, in integers so production's
/// `len / 10` and `len * 9 / 10` come out exactly, and held inside the list.
fn percentile_at(len: usize, pct: usize) -> usize {
    (len * pct / 100).min(len.saturating_sub(1))
}

/// Production does not weigh release year at all: two titles' years are part of what the facet axis `era`
/// already reads, and nothing has measured a separate term as better. The knob exists so it can be.
const W_YEAR: f64 = 0.0;
/// Years apart at which the year term halves; see `year_proximity`.
const YEAR_HALFLIFE: f64 = 10.0;

/// The per-axis facet knobs, in `den_store::FACET_AXES` order: `w_facet_<axis>`. Each multiplies `w_facet`
/// for its axis alone, so 1.0 everywhere is exactly the single weight production ranks with.
pub const FACET_AXIS_KNOBS: [&str; 12] = [
    "w_facet_era",
    "w_facet_setting",
    "w_facet_scope",
    "w_facet_ending",
    "w_facet_pacing",
    "w_facet_chronology",
    "w_facet_continuity",
    "w_facet_conflict",
    "w_facet_ensemble",
    "w_facet_tone",
    "w_facet_timespan",
    "w_facet_archetype",
];

/// How close two release years are, in 0..=1: `2^(-|Δyear| / halflife)`. The same year is 1, `halflife`
/// years apart is 0.5, twice that 0.25 — a smooth decay with no cliff at a decade boundary. `libm`'s
/// `exp2`, so every target rounds it alike.
fn year_proximity(a: f64, b: f64, halflife: f64) -> f64 {
    libm::exp2(-(a - b).abs() / halflife)
}

impl Default for SimilarParams {
    fn default() -> Self {
        SimilarParams {
            pool_k: POOL_K,
            w_premise: W_PREMISE,
            w_plot: W_PLOT,
            w_tone: W_TONE,
            tone_floor: TONE_FLOOR,
            min_confidence: MIN_CONFIDENCE,
            subgenre_cap: SUBGENRE_CAP,
            cap_window: KEEP,
            max_row: MAX_ROW,
            w_maker: W_MAKER,
            w_character: W_CHARACTER,
            w_region: W_REGION,
            w_home: W_HOME,
            w_facet: W_FACET,
            w_world: W_WORLD,
            w_noul: W_NOUL,
            w_critique: W_CRITIQUE,
            w_coverage: W_COVERAGE,
            same_animation: true,
            mix_types: MIX_TYPES,
            noul_floor: NOUL_FLOOR,
            world_floor: WORLD_FLOOR,
            defining: DEFINING,
            w_year: W_YEAR,
            year_halflife: YEAR_HALFLIFE,
            w_facet_axis: [1.0; 12],
            min_rating: MIN_RATING,
            min_votes: MIN_VOTES,
            w_popularity: W_POPULARITY,
            min_popularity: MIN_POPULARITY,
            critique_floor: CRITIQUE_FLOOR,
            holds: HOLDS,
            pool_floor_pct: POOL_FLOOR_PCT,
            spread_low_pct: SPREAD_LOW_PCT,
            spread_high_pct: SPREAD_HIGH_PCT,
        }
    }
}

/// One knob as a client sees it: its name (the field's), what part of the ranking it affects, the range a
/// value must fall in, whether it is a whole number, and one line saying what it does.
#[derive(Clone, Copy, Debug)]
pub struct Knob {
    pub name: &'static str,
    pub group: &'static str,
    pub min: f64,
    pub max: f64,
    pub integer: bool,
    pub about: &'static str,
}

/// The groups `Knob::group` names, in the order a form shows them.
pub const KNOB_GROUPS: &[&str] = &["pool", "signals", "facet axes", "floors", "filters", "row"];

const fn knob(
    name: &'static str,
    group: &'static str,
    min: f64,
    max: f64,
    integer: bool,
    about: &'static str,
) -> Knob {
    Knob { name, group, min, max, integer, about }
}

impl SimilarParams {
    /// Every field, in the order a form shows them. `get`/`set` answer exactly these names.
    ///
    /// The ranges are also what bounds a request's cost, because the playground that sets these is reachable
    /// by anyone who can reach atlas. The size knobs stop at 2.5× (`pool_k`) and 2× (`max_row`) production:
    /// measured on the published store over 817 seeds (the judged seeds and every title in the golden rows)
    /// with every size at its maximum and every floor at 0, a request costs at most 30 ms of CPU (median 13)
    /// and 0.16 MB, where the old maxima (`pool_k = 5000`, `max_row = 1000`) cost up to ~100 ms (median 53)
    /// and 0.72 MB. The other knobs change what a candidate scores, not how many are scored.
    pub const KNOBS: &'static [Knob] = &[
        knob("pool_k", "pool", 1.0, 1000.0, true, "candidates drawn from EACH vector index"),
        knob("w_premise", "pool", 0.0, 10.0, false, "base: weight of the premise-space cosine"),
        knob("w_plot", "pool", 0.0, 10.0, false, "base: weight of the plot-space cosine"),
        knob("pool_floor_pct", "pool", 0.0, 50.0, true, "percentile of the pool a missing cosine is read at"),
        knob(
            "spread_low_pct",
            "pool",
            0.0,
            49.0,
            true,
            "spread = base at spread_high_pct minus base at this",
        ),
        knob("spread_high_pct", "pool", 51.0, 100.0, true, "... the upper percentile of that spread"),
        knob("w_maker", "signals", 0.0, 10.0, false, "shared director/writer/creator (share of the seed's)"),
        knob(
            "w_character",
            "signals",
            0.0,
            10.0,
            false,
            "shared character (spin-offs, sequels); above 0 also nominates the linked titles",
        ),
        knob(
            "w_region",
            "signals",
            0.0,
            10.0,
            false,
            "same country, then region, then continent — for a seed not from the US or UK nor in English",
        ),
        knob("w_home", "signals", 0.0, 10.0, false, "shared broadcaster/production company"),
        knob("w_facet", "signals", 0.0, 10.0, false, "agreement on the twelve facet axes, rarity-weighted"),
        knob(
            "w_world",
            "signals",
            0.0,
            10.0,
            false,
            "penalty for a different world (realist vs fantastical)",
        ),
        knob("w_noul", "signals", 0.0, 10.0, false, "cosine over the taxonomy nouls"),
        knob("w_critique", "signals", 0.0, 10.0, false, "centered cosine over what the works argue about"),
        knob(
            "w_coverage",
            "signals",
            0.0,
            10.0,
            false,
            "coverage of the seed's defining arguments, idf-weighted",
        ),
        knob("w_tone", "signals", 0.0, 10.0, false, "coverage of the seed's confident subgenres and moods"),
        knob(
            "w_year",
            "signals",
            0.0,
            10.0,
            false,
            "release-year proximity: 2^(-|years apart| / year_halflife)",
        ),
        knob(
            "w_popularity",
            "signals",
            0.0,
            10.0,
            false,
            "TMDB popularity, ln-scaled to the pool's most popular",
        ),
        knob("w_facet_era", "facet axes", 0.0, 10.0, false, "x w_facet for the era axis alone"),
        knob("w_facet_setting", "facet axes", 0.0, 10.0, false, "x w_facet for the setting axis alone"),
        knob("w_facet_scope", "facet axes", 0.0, 10.0, false, "x w_facet for the scope axis alone"),
        knob("w_facet_ending", "facet axes", 0.0, 10.0, false, "x w_facet for the ending axis alone"),
        knob("w_facet_pacing", "facet axes", 0.0, 10.0, false, "x w_facet for the pacing axis alone"),
        knob("w_facet_chronology", "facet axes", 0.0, 10.0, false, "x w_facet for the chronology axis alone"),
        knob("w_facet_continuity", "facet axes", 0.0, 10.0, false, "x w_facet for the continuity axis alone"),
        knob("w_facet_conflict", "facet axes", 0.0, 10.0, false, "x w_facet for the conflict axis alone"),
        knob("w_facet_ensemble", "facet axes", 0.0, 10.0, false, "x w_facet for the ensemble axis alone"),
        knob("w_facet_tone", "facet axes", 0.0, 10.0, false, "x w_facet for the tone axis alone"),
        knob("w_facet_timespan", "facet axes", 0.0, 10.0, false, "x w_facet for the timespan axis alone"),
        knob("w_facet_archetype", "facet axes", 0.0, 10.0, false, "x w_facet for the archetype axis alone"),
        knob(
            "min_confidence",
            "floors",
            0.0,
            1.0,
            false,
            "a label below this confidence is not part of a title",
        ),
        knob("noul_floor", "floors", 0.0, 1.0, false, "a noul below this is ignored"),
        knob("world_floor", "floors", 0.0, 1.0, false, "a world score below this reads as realist (0)"),
        knob("defining", "floors", 0.0, 1.0, false, "a critique axis at or above this defines the seed"),
        knob("critique_floor", "floors", 0.0, 1.0, false, "a critique axis below this says nothing"),
        knob("holds", "floors", 0.0, 1.0, false, "critique idf: ln(titles / titles at or above this)"),
        knob("year_halflife", "floors", 1.0, 100.0, false, "years apart at which the year term halves"),
        knob("same_animation", "filters", 0.0, 1.0, true, "1: never mix animated with live action"),
        knob("mix_types", "filters", 0.0, 1.0, true, "1: mix films and series in one row"),
        knob("tone_floor", "filters", 0.0, 1.0, false, "drop a labelled candidate covering less of the seed"),
        knob("min_rating", "filters", 0.0, 10.0, false, "drop below this TMDB rating (unrated kept)"),
        knob("min_votes", "filters", 0.0, 100_000.0, true, "drop below this many TMDB votes"),
        knob(
            "min_popularity",
            "filters",
            0.0,
            1000.0,
            false,
            "drop below this TMDB popularity (unknown kept)",
        ),
        knob(
            "subgenre_cap",
            "row",
            0.0,
            200.0,
            true,
            "at most this many titles sharing a dominant subgenre ...",
        ),
        knob("cap_window", "row", 0.0, 400.0, true, "... within this many leading titles"),
        knob("max_row", "row", 1.0, 400.0, true, "the longest row kept"),
    ];

    /// A knob's value, as a number.
    pub fn get(&self, name: &str) -> Option<f64> {
        Some(match name {
            "w_premise" => self.w_premise,
            "w_plot" => self.w_plot,
            "w_maker" => self.w_maker,
            "w_character" => self.w_character,
            "w_region" => self.w_region,
            "w_home" => self.w_home,
            "w_facet" => self.w_facet,
            "w_world" => self.w_world,
            "w_noul" => self.w_noul,
            "w_critique" => self.w_critique,
            "w_coverage" => self.w_coverage,
            "w_tone" => self.w_tone,
            "tone_floor" => self.tone_floor,
            "min_confidence" => self.min_confidence,
            "noul_floor" => self.noul_floor,
            "world_floor" => self.world_floor,
            "defining" => self.defining,
            "subgenre_cap" => self.subgenre_cap as f64,
            "cap_window" => self.cap_window as f64,
            "pool_k" => self.pool_k as f64,
            "max_row" => self.max_row as f64,
            "same_animation" => f64::from(u8::from(self.same_animation)),
            "mix_types" => f64::from(u8::from(self.mix_types)),
            "w_year" => self.w_year,
            "year_halflife" => self.year_halflife,
            "min_rating" => self.min_rating,
            "min_votes" => self.min_votes,
            "w_popularity" => self.w_popularity,
            "min_popularity" => self.min_popularity,
            "critique_floor" => self.critique_floor,
            "holds" => self.holds,
            "pool_floor_pct" => self.pool_floor_pct as f64,
            "spread_low_pct" => self.spread_low_pct as f64,
            "spread_high_pct" => self.spread_high_pct as f64,
            _ => self.w_facet_axis[FACET_AXIS_KNOBS.iter().position(|k| *k == name)?],
        })
    }

    /// Set a knob, refusing an unknown name, a value outside its range (NaN included) or a fraction where a
    /// whole number is wanted. The error names the knob.
    pub fn set(&mut self, name: &str, value: f64) -> Result<(), String> {
        let knob =
            Self::KNOBS.iter().find(|k| k.name == name).ok_or_else(|| format!("unknown parameter {name}"))?;
        if !(value >= knob.min && value <= knob.max) {
            return Err(format!("{name} must be between {} and {}, got {value}", knob.min, knob.max));
        }
        if knob.integer && value.fract() != 0.0 {
            return Err(format!("{name} must be a whole number, got {value}"));
        }
        let whole = value as usize;
        match name {
            "w_premise" => self.w_premise = value,
            "w_plot" => self.w_plot = value,
            "w_maker" => self.w_maker = value,
            "w_character" => self.w_character = value,
            "w_region" => self.w_region = value,
            "w_home" => self.w_home = value,
            "w_facet" => self.w_facet = value,
            "w_world" => self.w_world = value,
            "w_noul" => self.w_noul = value,
            "w_critique" => self.w_critique = value,
            "w_coverage" => self.w_coverage = value,
            "w_tone" => self.w_tone = value,
            "tone_floor" => self.tone_floor = value,
            "min_confidence" => self.min_confidence = value,
            "noul_floor" => self.noul_floor = value,
            "world_floor" => self.world_floor = value,
            "defining" => self.defining = value,
            "subgenre_cap" => self.subgenre_cap = whole,
            "cap_window" => self.cap_window = whole,
            "pool_k" => self.pool_k = whole,
            "max_row" => self.max_row = whole,
            "same_animation" => self.same_animation = whole == 1,
            "mix_types" => self.mix_types = whole == 1,
            "w_year" => self.w_year = value,
            "year_halflife" => self.year_halflife = value,
            "min_rating" => self.min_rating = value,
            "min_votes" => self.min_votes = value,
            "w_popularity" => self.w_popularity = value,
            "min_popularity" => self.min_popularity = value,
            "critique_floor" => self.critique_floor = value,
            "holds" => self.holds = value,
            "pool_floor_pct" => self.pool_floor_pct = whole,
            "spread_low_pct" => self.spread_low_pct = whole,
            "spread_high_pct" => self.spread_high_pct = whole,
            _ => {
                let axis = FACET_AXIS_KNOBS
                    .iter()
                    .position(|k| *k == name)
                    .ok_or_else(|| format!("unknown parameter {name}"))?;
                self.w_facet_axis[axis] = value;
            }
        }
        Ok(())
    }
}

/// The seed's confident labels, kept split by family because `tone` judges each family separately.
struct Seed {
    subgenres: Vec<(String, f64)>,
    moods: Vec<(String, f64)>,
}

/// The seed's labels a candidate is measured against: subgenres and moods it carries confidently.
///
/// Weighted by confidence alone. Weighting each label by its rarity as well — `ln(N / titles carrying it)`,
/// so a mood 1,842 titles share counts for less than a specific subgenre — was tried and measured WORSE on
/// this corpus: mean single-subgenre share rose 15% to 17%, The Corner fell from 10th to 13th in The Wire's
/// row, and it did not remove the miss it was aimed at (Angel, which carries both of The Wire's moods and
/// none of its subgenres, held 7th either way). Recorded here so the next person does not re-derive it.
fn seed_labels(labels: &crate::Labels<'_>, min_confidence: f64) -> Seed {
    let keep = |pairs: &[(&str, f64)]| -> Vec<(String, f64)> {
        pairs.iter().filter(|(_, c)| *c >= min_confidence).map(|(n, c)| ((*n).to_string(), *c)).collect()
    };
    Seed { subgenres: keep(&labels.subgenres), moods: keep(&labels.moods) }
}

/// How much of the SEED the candidate covers, confidence-weighted, averaged over the label families the
/// candidate actually has.
///
/// Coverage of the seed, deliberately not Jaccard: a candidate is not less like The Wire for carrying labels
/// The Wire lacks. Jaccard punishes exactly the broad, many-labelled titles this is meant to surface.
///
/// Per family, because "unknown is not none" has to hold within a title as well as across the corpus.
/// Flora and Son carries three moods and NO subgenres; scored against one pooled total it covered 0.23 of
/// Once and was cut by the floor — punished for missing subgenres it does not have rather than for being
/// unlike Once. Judged on the family it actually carries it scores 0.50 and survives, which is right: it is
/// the same director's film about the same thing.
fn tone(seed: &Seed, theirs: &crate::Labels<'_>, min_confidence: f64) -> f64 {
    // A family the candidate says nothing in asks nothing of it, and neither does one the seed is silent on.
    let covered = |family: &[(String, f64)], theirs: &[(&str, f64)]| -> Option<f64> {
        let total: f64 = family.iter().map(|(_, c)| c).sum();
        if total <= 0.0 || !theirs.iter().any(|(_, c)| *c >= min_confidence) {
            return None;
        }
        let has = |name: &str| theirs.iter().any(|(n, c)| *n == name && *c >= min_confidence);
        Some(family.iter().filter(|(n, _)| has(n)).map(|(_, c)| c).sum::<f64>() / total)
    };
    let sub_cov = covered(&seed.subgenres, &theirs.subgenres);
    let mood_cov = covered(&seed.moods, &theirs.moods);
    match (sub_cov, mood_cov) {
        (Some(s), Some(m)) => (s + m) / 2.0,
        (Some(s), None) => s,
        (None, Some(m)) => m,
        // The candidate carries no confident labels at all: unknown is not none, so it is not gated.
        (None, None) => 1.0,
    }
}

/// How hard a shared director/writer/creator pulls a candidate up, as a fraction of the pool's spread.
///
/// Large on purpose. Once, Begin Again, Sing Street and Flora and Son are one film made four times by John
/// Carney — a musician meets a musician and the songs carry the story — and all four credit him in the
/// shipped facts. Yet none of the other three reaches Once's row on vectors alone: their premise ranks are
/// 726, 2,725 and 2,237. Authorship is the strongest evidence of "you will want this next" that the dataset
/// holds, and nothing in the ranking path read it.
///
/// Swept with `den-atlas rail-ab`, against the mean rank of the titles a viewer would expect and — as the
/// counterweight that number needs — the share of the visible twenty crediting one of the seed's own
/// makers. That share is what raising this buys the row at: past some point it stops being "more like
/// this" and becomes "more by this person", which is a row the detail screen already has.
///
/// ```text
/// W_MAKER   want mean rank    same-maker share
///   0.00        57.9                7%
///   0.40        48.6                8%
///   0.80        43.2                8%
///   1.20        37.3               10%
///   1.60        34.9               13%
///   2.00        33.0               13%
///   2.60        30.3               15%
/// ```
///
/// The marginal return falls off here: 9.3 ranks per point of maker share below 0.4, 2.0 across 0.8→1.2,
/// 0.8 above 1.6. The level is optimistic — most expected titles are expected BECAUSE they share a maker,
/// so the scale flatters the weight — but the shape of the curve is what picks the knee, and it picks 1.20.
const W_MAKER: f64 = 1.20;
/// How hard a shared character pulls a candidate up, as a fraction of the pool's spread (the link's own
/// strength, 0..=1, is what it multiplies): Frasier for Cheers, Better Call Saul for Breaking Bad, Picard
/// for The Next Generation — sequels and spin-offs whose people and characters carry over and whose
/// vectors often do not.
///
/// Chosen with `den-atlas rail-eval` on the judged set's dev half (nDCG@10 / condensed):
///
/// ```text
/// w_character   dev nDCG   dev nDCG'
///   0.00         0.790      0.800
///   0.50         0.790      0.800
///   0.75-3.00    0.792      0.802
/// ```
///
/// A plateau from 0.75; 1.0 sits on it, a little under a shared maker. What it moves: Breaking Bad
/// 0.752 → 0.801, The Next Generation 0.857 → 0.912; and it costs Friends 0.771 → 0.710 (Joey, judged
/// only ok, rises to second) and Alien 0.775 → 0.766.
///
/// A linked title is nominated into the pool but is NOT exempt from the tonal floor as a same-maker one
/// is. With the exemption, the Ewok TV films and the Star Wars Holiday Special — every character of the
/// seed, none of the film — entered Star Wars' first five (nDCG 0.703 → 0.671); without it they stay out.
const W_CHARACTER: f64 = 1.0;
/// How hard a shared origin pulls a candidate up, as a fraction of the pool's spread, for a seed from outside
/// the English-language mainstream (`regional`): a Swedish title favours Swedish ones, then Nordic, then
/// European.
///
/// Off: the judged set cannot show a gain. `den-atlas rail-eval` over both judged files, dev half:
///
/// ```text
/// w_region   nDCG    nDCG'   bad   judged@10
///   0.00     0.792   0.794    26     293
///   0.25     0.791   0.795    26     293
///   0.50     0.789   0.796    24     290
///   1.00     0.787   0.799    23     287
///   2.00     0.763   0.800    24     276
/// ```
///
/// Condensed nDCG and bad@10 improve while plain nDCG falls and fewer of the first ten are judged: the rows
/// move onto titles nobody has graded, which is the judged set's gap, not evidence for the weight. Grading
/// what `RAIL_EVAL_UNJUDGED` lists at 0.5–1 is what would settle it.
const W_REGION: f64 = 0.0;
/// What each tier of `origin_affinity` counts for: the same country in full, the same region
/// (`regions::REGIONS`: Nordic, Slavic, East Asian …) this much, the same continent this much.
const REGION_TIER: f64 = 0.5;
const CONTINENT_TIER: f64 = 0.25;

/// Whether a seed's origin is a signal worth reading: its first country is not the US or Britain and its
/// original language is not English. For Hollywood the term would only reinforce Hollywood.
fn regional(countries: &[[u8; 2]], language: Option<[u8; 2]>) -> bool {
    countries.first().is_some_and(|home| home != b"US" && home != b"GB") && language != Some(*b"en")
}

/// How near a candidate's origin is to the seed's, 0..=1: the seed's first country against the candidate's
/// first two, the nearest tier of any.
fn origin_affinity(seed: &[[u8; 2]], theirs: &[[u8; 2]]) -> f64 {
    let Some(home) = seed.first().and_then(|c| std::str::from_utf8(c).ok()) else { return 0.0 };
    let shared = |groups: &[crate::Region], c: &str| {
        groups.iter().any(|g| g.countries.contains(&home) && g.countries.contains(&c))
    };
    theirs
        .iter()
        .take(2)
        .filter_map(|c| std::str::from_utf8(c).ok())
        .map(|c| {
            if c == home {
                1.0
            } else if shared(crate::REGIONS, c) {
                REGION_TIER
            } else if shared(crate::regions::CONTINENTS, c) {
                CONTINENT_TIER
            } else {
                0.0
            }
        })
        .fold(0.0, f64::max)
}

/// A shared home — the same broadcaster or production company — as a small tiebreak, never a lane of its
/// own. HBO is 131 titles in this corpus, so it discriminates; "made for television" would not.
const W_HOME: f64 = 0.15;

/// How hard shared narrative facets pull a candidate up, as a fraction of the pool's spread.
///
/// The axes come from the completed model pass and say what the vectors cannot: The Wire is
/// `ensemble-led 0.98` / `person-vs-system 0.97` / `single-city 1.00`, and Angel — which the vectors put
/// 12th on it — is `single-lead 1.00` / `person-vs-person 0.37` / `hybrid` continuity. Nothing in the label
/// taxonomy expresses that difference, which is why Angel survived the tonal floor.
const W_FACET: f64 = 2.00;
/// How hard a mismatch of WORLD is punished. A realist show and a show with vampires in it are not
/// neighbours however much tone they share — which is the whole of the Angel-on-The-Wire defect, and no
/// facet axis says it: Angel agrees with The Wire on `scope = single-city` and `setting = urban` because it
/// is set in Los Angeles. Asymmetric in effect rather than in form: sharing "not fantastical" is the corpus
/// default and evidence of nothing, so only the DIFFERENCE is scored, never the agreement.
const W_WORLD: f64 = 2.50;
/// Cosine over the 75 taxonomy nouls, which reach `labels-t02.json` only as a thresholded top-three and so
/// were invisible to `tone`.
///
/// This is the term `tone` should have been. Measured against The Wire, `tone` scores Oz, Bates Motel,
/// Generation Kill and The Deuce at an identical **0.257** — three titles we want and the one title the
/// harness names as a miss, indistinguishable. The noul cosine separates them, and drops Bates Motel from
/// the top twenty to rank 1,509 of 7,528 on its own.
const W_NOUL: f64 = 1.60;
/// Agreement on what two works ARGUE ABOUT.
///
/// The one signal that connects The Wire and Oz, which nothing shipped could: they are the same kind of
/// show to a human — a sociological study of a closed American institution — and every vector space,
/// facet axis and label family puts them far apart. Both now read high on `justice-system`, `institution`
/// and `the-state`.
///
/// Centered on the per-axis corpus mean before comparing, because raw cosine over seventeen mostly-low
/// values is dominated by a shared baseline: it scored Oz 0.885 and Angel 0.792, ranking them correctly
/// and separating them by almost nothing. Centered, the same pair is +0.700 and +0.274.
///
/// A WEIGHT and not a nominator, measured. Ranked by critique alone Oz sits 329th of 7,529 against The
/// Wire — against premise rank 4,450 and plot 1,134, so the signal is real — and Angel sits 2,893rd. But
/// nominating the 400 nearest critique profiles, which does reach Oz, still did not put it in the row and
/// cost mean same-genre share 47% -> 49%. Oz is reachable and not competitive; forcing it past twenty
/// better-scoring candidates would be tuning to one pair.
const W_CRITIQUE: f64 = 1.40;
/// Coverage of the seed's DEFINING arguments, idf-weighted — kept ALONGSIDE the cosine, not instead of it.
///
/// The cosine is what pushes a tonal impostor away: Bates Motel sits at -0.32 on it, and coverage alone
/// cannot say that. Coverage is what pulls the right titles in, because it asks only about the arguments
/// the seed is actually built on. Ranked by coverage against The Wire rather than cosine: Oz 329 -> 136,
/// Homicide 316 -> 60, Show Me a Hero 197 -> 67, We Own This City 184 -> 10, Deadwood 42 -> 13 — while
/// Bates Motel goes 4,830 -> 6,042 and Angel 2,893 -> 3,212.
const W_COVERAGE: f64 = 1.40;

/// Idf-weighted coverage of the seed's defining arguments by a candidate.
fn critique_coverage(defining: &[Weighted], theirs: &[Weighted]) -> Option<f64> {
    if defining.is_empty() || theirs.is_empty() {
        return None;
    }
    let total: f64 = defining.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        return None;
    }
    let covered: f64 = defining
        .iter()
        .filter_map(|(name, w)| theirs.iter().find(|(n, _)| n == name).map(|(_, p)| w * p))
        .sum();
    Some(covered / total)
}

/// Cosine between two titles' noul vectors, over the union of the dimensions either one carries.
fn noul_cosine(seed: &[Weighted], theirs: &[Weighted]) -> Option<f64> {
    if seed.is_empty() || theirs.is_empty() {
        return None;
    }
    let mut dot = 0.0;
    for (name, p) in seed {
        if let Some((_, q)) = theirs.iter().find(|(n, _)| n == name) {
            dot += p * q;
        }
    }
    let norm = |v: &[Weighted]| v.iter().map(|(_, p)| p * p).sum::<f64>().sqrt();
    let d = norm(seed) * norm(theirs);
    (d > 0.0).then(|| dot / d)
}

/// An axis, by its index in the fixed 12-axis order (den-spec store-v1 §Facets).
pub type Axis = u8;
/// A value, by its id in the store's one string table. Comparing ids compares strings, because the
/// table interns: two titles share a facet value exactly when their ids are equal.
pub type ValueId = u32;
/// A named probability — a noul, a critique axis — as (name id, value).
pub type Weighted = (ValueId, f64);

/// One title's facet choices: axis -> (value, confidence). Supplied by the caller for the same reason as
/// `Authorship` — `den-index` does not know where a facet comes from.
///
/// These were `String`s. On a single More Like This that meant cloning ~104 of them per candidate over a
/// 400-candidate pool — measured at 2.4 µs per candidate, about a millisecond per uncached request spent
/// entirely on the shape of this trait — plus two more allocations inside every `prevalence` call. Ids
/// are `Copy`, compare by equality exactly as the strings did, and the caller resolves a name only when
/// something is actually rendered.
pub trait Facets {
    fn facets(&self, key: Key) -> Vec<(Axis, ValueId, f64)>;
    /// The seed's DEFINING arguments — axes it reads >= 0.8 on — each with its idf weight, and a
    /// candidate's raw probability on them. Coverage of these, not cosine over all seventeen.
    fn critique_defining(&self, key: Key) -> Vec<Weighted> {
        let _ = key;
        Vec::new()
    }
    /// A candidate's raw (uncentered) critique probabilities, for coverage.
    fn critique_raw(&self, key: Key) -> Vec<Weighted> {
        let _ = key;
        Vec::new()
    }
    /// The critique profile — what the work argues about — CENTERED on the corpus mean per axis, so the
    /// caller does the centering once rather than every comparison.
    fn critique(&self, key: Key) -> Vec<Weighted> {
        let _ = key;
        Vec::new()
    }
    /// The 75 taxonomy nouls with their probabilities, for the cosine term.
    fn nouls(&self, key: Key) -> Vec<Weighted> {
        let _ = key;
        Vec::new()
    }
    /// How far this title is from a realist world: vampires, superheroes, time travel, the apocalypse.
    /// 0 for The Wire, 0.97 for Angel.
    fn world(&self, key: Key) -> f64 {
        let _ = key;
        0.0
    }
    /// The year the title was released, when known.
    fn year(&self, key: Key) -> Option<f64> {
        let _ = key;
        None
    }
    /// Its countries of origin, ISO 3166-1 alpha-2 upper-case, first the one Wikidata lists first.
    fn countries(&self, key: Key) -> Vec<[u8; 2]> {
        let _ = key;
        Vec::new()
    }
    /// Its original language, ISO 639-1 lower-case, when known.
    fn language(&self, key: Key) -> Option<[u8; 2]> {
        let _ = key;
        None
    }
    /// Share of the SEED's type carrying this axis value, for rarity weighting — read from the seed's type
    /// for a candidate of either. A shared `chronology = linear` is worth almost nothing (76% of titles)
    /// where a shared `conflict = person-vs-system` is worth a lot.
    fn prevalence(&self, axis: Axis, value: ValueId) -> f64;
}

/// Agreement between two titles' facets, confidence-weighted and rarity-weighted, in 0..=1 at production's
/// per-axis weights.
///
/// `axis_weight` multiplies what an agreeing axis adds and leaves the denominator alone, so it acts as a
/// per-axis `w_facet`: at 1.0 an axis counts as it always has (and `1.0 * weight` is `weight` exactly), at 0
/// agreeing on it adds nothing, at 2 it counts double.
fn facet_agreement(
    f: &dyn Facets,
    seed: &[(Axis, ValueId, f64)],
    other: Key,
    axis_weight: &[f64; 12],
) -> Option<f64> {
    let theirs = f.facets(other);
    if seed.is_empty() || theirs.is_empty() {
        return None; // Unknown is not none.
    }
    let mut num = 0.0;
    let mut den = 0.0;
    for (axis, value, conf) in seed {
        let Some((_, their_value, their_conf)) = theirs.iter().find(|(a, _, _)| a == axis) else { continue };
        // ln(1/prevalence): a value the whole corpus shares carries almost no evidence. `libm::log`, not
        // `f64::ln`, so every target rounds it the same way (see den-index's Cargo.toml).
        let weight = conf * libm::log(1.0 / f.prevalence(*axis, *value).max(1e-6)).max(0.0);
        den += weight;
        if their_value == value {
            num += axis_weight.get(usize::from(*axis)).copied().unwrap_or(1.0) * weight * their_conf;
        }
    }
    if den <= 0.0 {
        return None;
    }
    Some(num / den)
}

/// What the facts know about a title that the vectors cannot: who made it, and where it lived.
///
/// Two jobs, and the first is the one that matters. A weight can only re-order candidates the vectors
/// already proposed, and the vectors do not propose a seed's own siblings: The Wire and Oz are both HBO and
/// Oz is plot rank 1,134, far outside any sane pool. `nominate` lets authorship put a title into the pool on
/// its own evidence, where the rest of the scorer then judges it like anything else.
pub trait Authorship {
    /// Titles of either type that share a maker with the seed, whatever the vectors think of them. The
    /// scorer keeps the other type's only while `mix_types` is on.
    fn nominate(&self) -> Vec<Key>;
    /// Share of the seed's makers this candidate shares, 0..=1.
    fn makers(&self, key: Key) -> f64;
    /// Share of the seed's broadcasters/production companies this candidate shares, 0..=1.
    fn home(&self, key: Key) -> f64 {
        let _ = key;
        0.0
    }
    /// The titles of either type sharing a character with the seed, each with how strongly the link says
    /// the two are one franchise, 0..=1 — a spin-off or a sequel the vectors may rank nowhere.
    fn characters(&self) -> &[(Key, f64)] {
        &[]
    }
}

/// Neighbour ids for More Like This, best first — the pooled scorer.
///
/// Four differences from `more_like_this`, each answering a measured defect:
///
///  1. **The pool is the union of both indexes**, not premise's top 40. Today a plot neighbour can only add a
///     quarter to a premise candidate that was already there; it can never enter the row. Measured on The
///     Wire, plot's top 20 and premise's top 40 do not intersect at all, so the plot index contributes
///     nothing and the agreement bonus fires on no one.
///  2. **Score blends both spaces** rather than ranking on premise alone, with a candidate missing from one
///     side scored at that side's pool floor — never zero, since 3,008 titles have no premise vector.
///  3. **A tonal term over moods and subgenres**, which the shipped scorer never reads. It is the signal that
///     separates Homicide (0.77) from Bates Motel (0.27), both of which are `primaryGenre = Crime` and so
///     indistinguishable to the cross-genre penalty.
///  4. **Shared authorship**, via `authorship` — which both NOMINATES candidates the vectors rank nowhere
///     (The Wire and Oz are both HBO; Oz is plot rank 1,134) and weights them once they are in the pool. A
///     trait rather than a facts index because `den-index` deliberately does not know what a fact is.
///
/// The cross-genre penalty is gone: it punished every one of the seed's own siblings in another genre while
/// waving through anything that merely shared its genre label.
pub fn more_like_this_pooled(
    plot: Option<&Index>,
    premise: Option<&Index>,
    tmdb_id: u32,
    media_type: MediaType,
    authorship: Option<&dyn Authorship>,
    facets: Option<&dyn Facets>,
) -> Vec<Key> {
    let params = SimilarParams::default();
    more_like_this_scored(plot, premise, tmdb_id, media_type, authorship, facets, &params)
        .into_iter()
        .map(|s| s.key())
        .collect()
}

/// One title in a pooled row, with every signal that placed it there.
///
/// The signals are raw, in their own units (a share, a cosine, a distance). What each one added to `score`
/// is `spread × its weight × it` — negated for `world`, which is a penalty — so a caller holding the
/// `SimilarParams` it ranked with can show why a title sits where it does.
#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    pub media_type: MediaType,
    pub tmdb_id: u32,
    pub score: f64,
    /// `w_premise × premise + w_plot × plot`, a missing cosine read at that index's pool floor.
    pub base: f64,
    /// The cosines to the seed; for a title of the other type, mapped onto the seed type's scale
    /// (`z_map`).
    pub premise: Option<f64>,
    pub plot: Option<f64>,
    /// The seed type's score spread (90th − 10th percentile of `base`): the unit every term is scaled by.
    pub spread: f64,
    pub tone: f64,
    pub noul: f64,
    pub critique: f64,
    pub coverage: f64,
    pub maker: f64,
    /// The strongest character link to the seed (`Authorship::characters`), 0 without one.
    pub character: f64,
    /// How near its origin is to a regional seed's (`origin_affinity`); 0 for any other seed.
    pub region: f64,
    pub home: f64,
    pub facet: f64,
    pub world: f64,
    /// Release-year proximity (`year_proximity`), 0 when either year is unknown.
    pub year: f64,
    /// TMDB popularity against the pool's most popular, `ln(1+p) / ln(1+max)`; 0 when unknown.
    pub popularity: f64,
    /// The dominant confident subgenre the cap counts against; empty when there is none.
    pub subgenre: String,
    /// Held back by the subgenre cap and placed after the capped window instead of at its score rank.
    pub held: bool,
}

impl Scored {
    pub fn key(&self) -> Key {
        (self.media_type, self.tmdb_id)
    }
}

/// What viewers make of a title — its rating and vote count, TMDB's popularity — for the filters and the
/// popularity term. Supplied by the caller for the same reason as `Authorship`: `den-index` does not know
/// where a rating comes from. `None` is unknown, and an unknown title is kept by every filter.
pub trait Audience {
    /// (rating, vote count).
    fn rating(&self, key: Key) -> Option<(f64, f64)>;
    /// TMDB's popularity in its daily export: unbounded, most titles under 50.
    fn popularity(&self, key: Key) -> Option<f64>;
}

/// What a request adds beyond the corpus: who watches, and which candidates it will consider at all.
#[derive(Clone, Copy, Default)]
pub struct Extras<'a> {
    pub audience: Option<&'a dyn Audience>,
    /// A candidate this answers `false` for is dropped before anything is scored (a facet filter, titles
    /// already watched). Before, so the pool's own statistics — its floors and spread — are over what the
    /// row can actually hold.
    pub keep: Option<&'a dyn Fn(Key) -> bool>,
}

/// `more_like_this_pooled` with its knobs as an argument, and every title's signals kept. Serving ranks
/// through this with `SimilarParams::default()`; the tuning playground with whatever it was asked for.
pub fn more_like_this_scored(
    plot: Option<&Index>,
    premise: Option<&Index>,
    tmdb_id: u32,
    media_type: MediaType,
    authorship: Option<&dyn Authorship>,
    facets: Option<&dyn Facets>,
    p: &SimilarParams,
) -> Vec<Scored> {
    more_like_this_with(plot, premise, tmdb_id, media_type, authorship, facets, Extras::default(), p)
}

/// `more_like_this_scored`, with a request's `Extras`.
#[allow(clippy::too_many_arguments)]
pub fn more_like_this_with(
    plot: Option<&Index>,
    premise: Option<&Index>,
    tmdb_id: u32,
    media_type: MediaType,
    authorship: Option<&dyn Authorship>,
    facets: Option<&dyn Facets>,
    extras: Extras<'_>,
    p: &SimilarParams,
) -> Vec<Scored> {
    let seed: Key = (media_type, tmdb_id);
    let wanted = |key: Key| key != seed && (p.mix_types || key.0 == media_type);
    let mut pool: Vec<Key> = Vec::new();
    let mut seen: HashSet<Key> = HashSet::new();
    // Per index, the seed's own type's cosines and the other type's, for `z_map`; and each other-type
    // title's rank in its own type's plot neighbours, for the tonal floor.
    let mut scales: [Option<(ScanStats, ScanStats)>; 2] = [None, None];
    let mut plot_ranks: HashMap<Key, u32> = HashMap::new();
    for (at, index) in [premise, plot].into_iter().enumerate() {
        let Some(index) = index else { continue };
        if !p.mix_types {
            for n in index.nearest(tmdb_id, media_type, p.pool_k) {
                if seen.insert((n.media_type, n.tmdb_id)) {
                    pool.push((n.media_type, n.tmdb_id));
                }
            }
            continue;
        }
        // `pool_k` of each type, in one scan; the seed's own type's list is exactly `nearest`'s.
        let by_type = index.nearest_by_type(tmdb_id, media_type, p.pool_k);
        if let [own, other] = by_type.as_slice() {
            scales[at] = Some((own.stats, other.stats));
            if at == 1 {
                plot_ranks.extend(other.nearest.iter().zip(0..).map(|(n, r)| ((n.media_type, n.tmdb_id), r)));
            }
        }
        for n in by_type.iter().flat_map(|t| &t.nearest) {
            if seen.insert((n.media_type, n.tmdb_id)) {
                pool.push((n.media_type, n.tmdb_id));
            }
        }
    }
    // Facts nominate too. Without this a shared maker can only re-order what the vectors already found, and
    // the vectors do not find a seed's own siblings.
    for key in authorship.map(Authorship::nominate).unwrap_or_default() {
        if wanted(key) && seen.insert(key) {
            pool.push(key);
        }
    }
    // So do shared characters, while they are weighed at all: at `w_character = 0` the pool is exactly
    // what it was before they were read.
    if p.w_character > 0.0 {
        for &(key, _) in authorship.map(Authorship::characters).unwrap_or_default() {
            if wanted(key) && seen.insert(key) {
                pool.push(key);
            }
        }
    }
    if pool.is_empty() {
        return Vec::new();
    }

    // One index's cosine between the seed and a candidate, when that index holds both — on the seed type's
    // scale for a candidate of the other type.
    let sim = |at: usize, index: Option<&Index>, (kind, id): Key| -> Option<f64> {
        let index = index?;
        let a = index.row_of(tmdb_id, media_type)?;
        let b = index.row_of(id, kind)?;
        let cosine = index.similarity(a, b);
        match scales[at] {
            Some((own, other)) if kind != media_type => Some(z_map(cosine, own, other)),
            _ => Some(cosine),
        }
    };
    let pool: Vec<Candidate> = pool
        .iter()
        .map(|&key| Candidate {
            key,
            premise: sim(0, premise, key),
            plot: sim(1, plot, key),
            plot_rank: plot_ranks.get(&key).copied(),
        })
        .collect();
    // Labels come from whichever index holds the title; both carry the same label set.
    let labels = |(kind, id): Key| {
        premise.and_then(|x| x.labels(id, kind)).or_else(|| plot.and_then(|x| x.labels(id, kind)))
    };
    rank_pool(&pool, seed, &labels, authorship, facets, extras, p)
}

/// A cosine to a title of the other type, read on the seed type's scale: `μ_S + σ_S·(c − μ_C)/σ_C`, with
/// μ and σ the seed's cosines to every title of its own type (S) and of the other (C) in that index.
///
/// Measured on store 5b1c3213b6a1 (oxyc/den-atlas#49): the plot index puts films closer to films (random
/// pairs 0.555) than to series (0.510), because series articles read as premises and film articles as whole
/// stories, so raw cosines would rank every film above every series for a film seed. Standardised per
/// type, the nearest-neighbour tails line up almost exactly. The premise index has no such gap, and the map
/// is then close to the identity.
fn z_map(cosine: f64, own: ScanStats, other: ScanStats) -> f64 {
    if other.sd <= 0.0 {
        return cosine;
    }
    own.mean + own.sd * (cosine - other.mean) / other.sd
}

/// One member of a seed's candidate pool, with its cosine to the seed in each space (`None` where that
/// space does not hold both titles; on the seed type's scale for the other type, `z_map`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub key: Key,
    pub premise: Option<f64>,
    pub plot: Option<f64>,
    /// For a title of the other type, its rank among that type's plot neighbours of the seed, from 0.
    pub plot_rank: Option<u32>,
}

/// Everything More Like This does after retrieval: gate, score and order a pool that is already drawn.
///
/// Pure over its inputs — the pool with its cosines, the labels, and the two traits — so a caller holding
/// no vectors can rank a pool someone else retrieved, and rank it exactly as serving does. The pool's
/// ORDER matters as well as its membership: it is the candidates' order for every tie below.
/// `more_like_this_scored` is retrieval (both indexes' nearest `pool_k`, then authorship's nominations,
/// deduplicated in that order) followed by this.
pub fn rank_pool<'l>(
    pool: &[Candidate],
    seed_key: Key,
    labels: &dyn Fn(Key) -> Option<crate::Labels<'l>>,
    authorship: Option<&dyn Authorship>,
    facets: Option<&dyn Facets>,
    extras: Extras<'_>,
    p: &SimilarParams,
) -> Vec<Scored> {
    let Some(mine) = labels(seed_key) else {
        return Vec::new();
    };
    let own_type = |key: Key| key.0 == seed_key.0;
    let seed = seed_labels(&mine, p.min_confidence);
    let seed_facets: Vec<(Axis, ValueId, f64)> = facets.map(|f| f.facets(seed_key)).unwrap_or_default();
    let seed_world = facets.map_or(0.0, |f| f.world(seed_key));
    let seed_year = facets.and_then(|f| f.year(seed_key));
    let seed_nouls: Vec<Weighted> = facets.map(|f| f.nouls(seed_key)).unwrap_or_default();
    let seed_critique: Vec<Weighted> = facets.map(|f| f.critique(seed_key)).unwrap_or_default();
    let seed_defining: Vec<Weighted> = facets.map(|f| f.critique_defining(seed_key)).unwrap_or_default();
    let characters: &[(Key, f64)] = authorship.map(Authorship::characters).unwrap_or_default();
    let character = |key: Key| characters.iter().find(|&&(c, _)| c == key).map_or(0.0, |&(_, s)| s);
    // Read only while weighed, and only for a seed from outside the English-language mainstream.
    let seed_origin: Vec<[u8; 2]> = match facets {
        Some(f) if p.w_region > 0.0 => {
            let countries = f.countries(seed_key);
            if regional(&countries, f.language(seed_key)) {
                countries
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    let origin = |key: Key| -> f64 {
        match facets {
            Some(f) if !seed_origin.is_empty() => origin_affinity(&seed_origin, &f.countries(key)),
            _ => 0.0,
        }
    };

    let audience = extras.audience;
    // The request's filters. Each is skipped outright at its production value, so production's pool is
    // never even asked about them. Unknown passes: a title nobody has rated is not a badly rated one.
    // The other type is kept only while `mix_types` is on.
    let admitted = |id: Key| -> bool {
        if !p.mix_types && !own_type(id) {
            return false;
        }
        if extras.keep.is_some_and(|keep| !keep(id)) {
            return false;
        }
        if p.min_rating > 0.0 || p.min_votes > 0.0 {
            if let Some((rating, votes)) = audience.and_then(|a| a.rating(id)) {
                if rating < p.min_rating || votes < p.min_votes {
                    return false;
                }
            }
        }
        if p.min_popularity > 0.0 {
            if let Some(popularity) = audience.and_then(|a| a.popularity(id)) {
                if popularity < p.min_popularity {
                    return false;
                }
            }
        }
        true
    };
    let raw: Vec<&Candidate> = pool.iter().filter(|c| admitted(c.key)).collect();
    // A candidate one index has never seen is scored at that index's pool floor rather than zero, so a
    // missing vector costs it a little and does not disqualify it. The floor, like the spread below, is the
    // SEED'S type's ("anchoring"): read over both, the other type's cosines would move the seed type's own
    // order, which a mixed row must not.
    let floor = |values: Vec<f64>| -> f64 {
        let mut v = values;
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[percentile_at(v.len(), p.pool_floor_pct)]
    };
    let anchors: Vec<&&Candidate> = raw.iter().filter(|c| own_type(c.key)).collect();
    let premise_floor = floor(anchors.iter().filter_map(|c| c.premise).collect());
    let plot_floor = floor(anchors.iter().filter_map(|c| c.plot).collect());

    // (key, base, dominant subgenre, premise cosine, plot cosine) of a candidate past the gates.
    type Gated = (Key, f64, String, Option<f64>, Option<f64>);
    let mut scored: Vec<Gated> = Vec::new();
    for c in &raw {
        let (id, pc, l) = (c.key, c.premise, c.plot);
        let Some(theirs) = labels(id) else {
            continue;
        };
        if p.same_animation && theirs.animated != mine.animated {
            continue;
        }
        let t = tone(&seed, &theirs, p.min_confidence);
        let maker = authorship.map_or(0.0, |a| a.makers(id));
        // The floor is skipped when the candidate carries no confident labels at all — unknown is not none,
        // and filtering on it would silently drop every thinly-labelled title. A shared maker also exempts
        // it: labels are a guess about a title, authorship is a fact about it, and the fact wins. A shared
        // character does NOT (`W_CHARACTER`): the Star Wars Holiday Special has every character of the seed
        // and none of the film, and the floor is what keeps it out.
        //
        // Nor does a title of the other type in that type's plot top three: labels describe a film and a
        // series differently (Bingeable is a format), and the nearest few across the line are the titles a
        // mixed row exists for — El Camino for Breaking Bad.
        let unlabelled =
            theirs.subgenres.iter().chain(theirs.moods.iter()).all(|(_, c)| *c < p.min_confidence);
        let across = !own_type(id) && c.plot_rank.is_some_and(|rank| rank < CROSS_TOP);
        if !unlabelled && maker <= 0.0 && !across && t < p.tone_floor {
            continue;
        }
        let base = p.w_premise * pc.unwrap_or(premise_floor) + p.w_plot * l.unwrap_or(plot_floor);
        let dominant = theirs
            .subgenres
            .iter()
            .filter(|(_, c)| *c >= p.min_confidence)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(n, _)| (*n).to_string())
            .unwrap_or_default();
        scored.push((id, base, dominant, pc, l));
    }
    if scored.is_empty() {
        return Vec::new();
    }

    // The tonal term is expressed in the pool's own units so one weight works for every seed: the seed
    // type's pool, like the floors, unless nothing of that type passed the gates.
    let anchored: Vec<f64> = scored.iter().filter(|s| own_type(s.0)).map(|s| s.1).collect();
    let mut bases: Vec<f64> =
        if anchored.is_empty() { scored.iter().map(|s| s.1).collect() } else { anchored };
    bases.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let spread = (bases[percentile_at(bases.len(), p.spread_high_pct)]
        - bases[percentile_at(bases.len(), p.spread_low_pct)])
    .max(f64::EPSILON);
    // Popularity against the pool's most popular, on a log scale: TMDB's number is unbounded and heavy-tailed,
    // so linear would make one blockbuster the only title that scores.
    let most_popular =
        scored.iter().filter_map(|s| audience.and_then(|a| a.popularity(s.0))).fold(0.0f64, f64::max);
    let popularity_of = |id: Key| -> f64 {
        match audience.and_then(|a| a.popularity(id)) {
            Some(popularity) if most_popular > 0.0 => {
                libm::log1p(popularity.max(0.0)) / libm::log1p(most_popular)
            }
            _ => 0.0,
        }
    };
    let mut final_scored: Vec<Scored> = scored
        .into_iter()
        .map(|(id, base, dominant, pc, l)| {
            let theirs = labels(id);
            let t = theirs.as_ref().map_or(0.0, |th| tone(&seed, th, p.min_confidence));
            let maker = authorship.map_or(0.0, |a| a.makers(id));
            let home = authorship.map_or(0.0, |a| a.home(id));
            // A candidate with no facets scores the term at 0 rather than being penalised or exempted: it
            // simply brings no facet evidence, which is different from bringing disagreeing evidence.
            let fa =
                facets.and_then(|f| facet_agreement(f, &seed_facets, id, &p.w_facet_axis)).unwrap_or(0.0);
            let world = facets.map_or(0.0, |f| (f.world(id) - seed_world).abs());
            // A year missing on either side brings no evidence, so the term is 0 rather than a guess.
            let year = match (seed_year, facets.and_then(|f| f.year(id))) {
                (Some(a), Some(b)) => year_proximity(a, b, p.year_halflife),
                _ => 0.0,
            };
            let popularity = popularity_of(id);
            let ch = character(id);
            let region = origin(id);
            let nc = facets.and_then(|f| noul_cosine(&seed_nouls, &f.nouls(id))).unwrap_or(0.0);
            // Already centered, so this can be negative — arguing about different things is evidence
            // against a pair, not merely absence of evidence for it.
            let cr = facets.and_then(|f| noul_cosine(&seed_critique, &f.critique(id))).unwrap_or(0.0);
            let cov =
                facets.and_then(|f| critique_coverage(&seed_defining, &f.critique_raw(id))).unwrap_or(0.0);
            let score = base
                + spread
                    * (p.w_tone * t
                        + p.w_noul * nc
                        + p.w_critique * cr
                        + p.w_coverage * cov
                        + p.w_maker * maker
                        + p.w_home * home
                        + p.w_facet * fa
                        - p.w_world * world
                        // Last, so at production's `w_year = 0` and `w_popularity = 0` each adds an exact 0.0
                        // to the sum above; `w_character` and `w_region` likewise at 0.
                        + p.w_year * year
                        + p.w_popularity * popularity
                        + p.w_character * ch
                        + p.w_region * region);
            Scored {
                media_type: id.0,
                tmdb_id: id.1,
                score,
                base,
                premise: pc,
                plot: l,
                spread,
                tone: t,
                noul: nc,
                critique: cr,
                coverage: cov,
                maker,
                character: ch,
                region,
                home,
                facet: fa,
                world,
                year,
                popularity,
                subgenre: dominant,
                held: false,
            }
        })
        .collect();
    final_scored.sort_by(|a, b| {
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.tmdb_id.cmp(&b.tmdb_id))
    });

    // Each type is picked on its own — the cap keyed by (type, subgenre), its window counted in that type's
    // titles — and the two are then merged by score. So the seed type's titles come out in exactly the order
    // a row of that type alone would put them in, and a mixed row only adds the other type between them.
    let (own, other): (Vec<Scored>, Vec<Scored>) = final_scored.into_iter().partition(|s| own_type(s.key()));
    let (own, other) = (capped(own, p), capped(other, p));
    let mut row = Vec::with_capacity(p.max_row.min(own.len() + other.len()));
    let (mut own, mut other) = (own.into_iter().peekable(), other.into_iter().peekable());
    while row.len() < p.max_row {
        let take_own = match (own.peek(), other.peek()) {
            (Some(a), Some(b)) => a.score >= b.score,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        row.extend(if take_own { own.next() } else { other.next() });
    }
    row
}

/// The titles of one type in score order, under the per-subgenre cap, at most `max_row` of them.
///
/// Greedy pick under the cap, then a second pass to fill from what the cap held back rather than reaching
/// further down a worse tail. The cap counts against the first twenty — a row of two hundred should not be
/// three police procedurals and then nothing else from the genre.
fn capped(mut sorted: Vec<Scored>, p: &SimilarParams) -> Vec<Scored> {
    // Positions into `sorted`, not ids, so the answer can carry each title's signals.
    let mut taken: HashMap<&str, usize> = HashMap::new();
    let mut out: Vec<usize> = Vec::new();
    let mut held: Vec<usize> = Vec::new();
    for (at, s) in sorted.iter().enumerate() {
        if out.len() == p.max_row {
            break;
        }
        let count = taken.entry(s.subgenre.as_str()).or_insert(0);
        // Past the first screenful the cap stops applying: it exists to keep the visible row varied, and
        // beyond that it would start excluding good answers for being the same kind of thing.
        if s.subgenre.is_empty() || out.len() >= p.cap_window || *count < p.subgenre_cap {
            *count += 1;
            out.push(at);
        } else {
            held.push(at);
        }
    }
    // The held items go back in right after the visible screenful, in score order — NOT at the end of the
    // row. Appended, they were the last thing added to a 200-long list, so on a dense anchor the main loop
    // filled MAX_ROW first and they were dropped entirely: The Wire's 11th highest-scoring candidate is
    // Homicide: Life on the Street, and a 200-title "more like The Wire" had no Homicide in it.
    //
    // The cap's job is the first twenty. Past that, a held item is simply the next-best answer.
    let tail = out.split_off(out.len().min(p.cap_window));
    for &at in &held {
        sorted[at].held = true;
    }
    out.extend(held);
    out.extend(tail);
    out.truncate(p.max_row);
    out.into_iter().map(|at| sorted[at].clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::fixture;

    fn film(id: u32) -> Key {
        (MediaType::Movie, id)
    }

    fn tv(id: u32) -> Key {
        (MediaType::Tv, id)
    }

    #[test]
    fn plot_neighbours_when_there_is_no_premise_index() {
        let plot = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (2, "movie", "Drama", false, &[], &[], [80, 20, 0]),
            (3, "movie", "Drama", false, &[], &[], [0, 100, 0]),
        ]);
        assert_eq!(more_like_this(Some(&plot), None, 1, MediaType::Movie), vec![2, 3]);
        assert!(more_like_this(Some(&plot), None, 9, MediaType::Movie).is_empty());
    }

    #[test]
    fn the_premise_index_leads_gated_by_animation_genre_and_plot_agreement() {
        let premise = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (2, "movie", "Drama", true, &[], &[], [99, 0, 0]), // animated: never mixed in
            (3, "movie", "Comedy", false, &[], &[], [98, 0, 0]), // other genre: lowered a quarter
            (4, "movie", "Drama", false, &[], &[], [80, 0, 0]), // plot agrees: lifted a quarter
            (5, "movie", "Drama", false, &[], &[], [90, 0, 0]),
        ]);
        let plot = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (4, "movie", "Drama", false, &[], &[], [100, 0, 0]),
        ]);
        // Scores: 3 → 9800 − 2450 = 7350; 4 → 8000 + 2000 = 10000; 5 → 9000.
        assert_eq!(more_like_this(Some(&plot), Some(&premise), 1, MediaType::Movie), vec![4, 5, 3]);
    }

    /// The defect the pooled scorer exists for: a plot neighbour that premise ranks nowhere.
    ///
    /// Measured on the shipped corpus, The Wire's plot top-20 and premise top-40 do not intersect at all, so
    /// the shipped scorer's agreement bonus fires on nobody and Homicide: Life on the Street — plot's 10th
    /// nearest — is discarded. Title 9 below stands in for it.
    #[test]
    fn a_plot_neighbour_can_enter_the_row_which_the_shipped_scorer_cannot_do() {
        let premise = fixture(&[
            (1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [100, 0, 0]),
            (2, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [99, 0, 0]),
        ]);
        let plot = fixture(&[
            (1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [100, 0, 0]),
            (9, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [98, 0, 0]),
        ]);
        // 9 is absent from the premise index entirely, so the shipped scorer can never return it.
        assert!(!more_like_this(Some(&plot), Some(&premise), 1, MediaType::Tv).contains(&9));
        assert!(more_like_this_pooled(
            Some(&plot),
            Some(&premise),
            1,
            MediaType::Tv,
            None::<&dyn Authorship>,
            None::<&dyn Facets>
        )
        .contains(&tv(9)));
    }

    /// The Bates Motel case: same primary genre, so the cross-genre penalty never fires on it, but it shares
    /// almost none of the seed's labels.
    #[test]
    fn a_tonal_mismatch_is_dropped_even_when_the_primary_genre_matches() {
        let seed_subs: &[(&str, f64)] = &[("Police Procedural", 0.9), ("Political", 0.8)];
        let seed_moods: &[(&str, f64)] = &[("Dark & Gritty", 0.95), ("Thought-provoking", 0.9)];
        let premise = fixture(&[
            (1, "tv", "Crime", false, seed_subs, seed_moods, [100, 0, 0]),
            // Shares the seed's labels: a real neighbour.
            (2, "tv", "Drama", false, &[("Political", 0.8)], seed_moods, [70, 0, 0]),
            // Same genre, shares one generic mood: the miss.
            (3, "tv", "Crime", false, &[("Serial Killer", 0.9)], &[("Dark & Gritty", 0.9)], [95, 0, 0]),
        ]);
        let plot = fixture(&[(1, "tv", "Crime", false, seed_subs, seed_moods, [100, 0, 0])]);
        let out = more_like_this_pooled(
            Some(&plot),
            Some(&premise),
            1,
            MediaType::Tv,
            None::<&dyn Authorship>,
            None::<&dyn Facets>,
        );
        assert!(out.contains(&tv(2)), "the title sharing the seed's labels must survive");
        assert!(!out.contains(&tv(3)), "a same-genre title sharing only a generic mood must not");
        // The shipped scorer keeps the miss and ranks it ABOVE the real neighbour.
        let shipped = more_like_this(Some(&plot), Some(&premise), 1, MediaType::Tv);
        assert_eq!(shipped, vec![3, 2]);
    }

    /// The Once case. Begin Again, Sing Street and Flora and Son are the same director's films about the
    /// same thing, and none reaches Once's row on vectors alone — their premise ranks are 726, 2,725 and
    /// 2,237. Authorship is a fact about a title where a label is a guess, so it both lifts and exempts from
    /// the tonal floor: Flora and Son carries no subgenres at all.
    #[test]
    fn a_shared_maker_lifts_a_sibling_the_vectors_rank_nowhere() {
        let seed_subs: &[(&str, f64)] = &[("Romantic Drama", 0.75), ("Musical", 0.7)];
        let seed_moods: &[(&str, f64)] = &[("Tearjerker", 0.6), ("Feel-good", 0.6)];
        let premise = fixture(&[
            (1, "movie", "Romance", false, seed_subs, seed_moods, [100, 0, 0]),
            // Closer on vectors and tonally fine, but by another hand.
            (
                2,
                "movie",
                "Romance",
                false,
                &[("Romantic Drama", 0.8), ("Musical", 0.75)],
                &[("Feel-good", 0.9)],
                [95, 0, 0],
            ),
            // The sibling: same hand, but the vectors put it well down the pool and it carries no subgenres.
            (3, "movie", "Drama", false, &[], &[("Feel-good", 0.7)], [60, 0, 0]),
            // Filler, so the pool's score spread is a real range rather than the gap between two titles.
            (4, "movie", "Romance", false, &[("Romantic Drama", 0.8)], &[("Feel-good", 0.8)], [88, 0, 0]),
            (5, "movie", "Romance", false, &[("Musical", 0.8)], &[("Tearjerker", 0.8)], [80, 0, 0]),
            (6, "movie", "Romance", false, &[("Romantic Drama", 0.7)], &[("Tearjerker", 0.7)], [72, 0, 0]),
            (7, "movie", "Romance", false, &[("Musical", 0.7)], &[("Feel-good", 0.7)], [55, 0, 0]),
            (8, "movie", "Romance", false, &[("Romantic Drama", 0.6)], &[("Feel-good", 0.6)], [40, 0, 0]),
        ]);
        let plot = fixture(&[(1, "movie", "Romance", false, seed_subs, seed_moods, [100, 0, 0])]);
        let none = more_like_this_pooled(
            Some(&plot),
            Some(&premise),
            1,
            MediaType::Movie,
            None::<&dyn Authorship>,
            None::<&dyn Facets>,
        );
        assert_eq!(none.first(), Some(&film(2)), "on vectors alone the closer, unrelated title leads");
        assert!(
            none.iter().position(|x| *x == film(3)).is_some_and(|p| p > 2),
            "and the sibling sits down the row"
        );
        struct SameHand;
        impl Authorship for SameHand {
            fn nominate(&self) -> Vec<Key> {
                vec![film(3)]
            }
            fn makers(&self, id: Key) -> f64 {
                if id == film(3) {
                    1.0
                } else {
                    0.0
                }
            }
        }
        let with = more_like_this_pooled(
            Some(&plot),
            Some(&premise),
            1,
            MediaType::Movie,
            Some(&SameHand),
            None::<&dyn Facets>,
        );
        assert_eq!(with.first(), Some(&film(3)), "the same hand outranks a closer but unrelated title");
    }

    /// The Cheers case. Frasier shares a character with the seed and its kind, but the vectors put it outside
    /// the pool. At `w_character = 0` it is as absent as before characters were read; weighed, it is
    /// nominated and leads. The Holiday Special case: a linked title of another kind is nominated too, and
    /// the tonal floor still drops it.
    #[test]
    fn a_shared_character_nominates_and_lifts_a_spin_off_only_while_weighed() {
        let seed_subs: &[(&str, f64)] = &[("Sitcom", 0.9)];
        let seed_moods: &[(&str, f64)] = &[("Feel-good", 0.9)];
        let premise = fixture(&[
            (1, "tv", "Comedy", false, seed_subs, seed_moods, [100, 0, 0]),
            (2, "tv", "Comedy", false, seed_subs, seed_moods, [95, 0, 0]),
            (4, "tv", "Comedy", false, seed_subs, seed_moods, [88, 0, 0]),
            (5, "tv", "Comedy", false, seed_subs, seed_moods, [70, 0, 0]),
            (6, "tv", "Comedy", false, seed_subs, seed_moods, [50, 0, 0]),
            // The spin-off: far on the vectors, the same kind of show.
            (3, "tv", "Comedy", false, seed_subs, seed_moods, [0, 100, 0]),
            // The variety special: the same characters, labelled as something else.
            (7, "tv", "Drama", false, &[("Legal Drama", 0.9)], &[("Tense", 0.9)], [0, 100, 0]),
        ]);
        struct Spinoff;
        impl Authorship for Spinoff {
            fn nominate(&self) -> Vec<Key> {
                Vec::new()
            }
            fn makers(&self, _: Key) -> f64 {
                0.0
            }
            fn characters(&self) -> &[(Key, f64)] {
                &[((MediaType::Tv, 3), 1.0), ((MediaType::Tv, 7), 1.0)]
            }
        }
        let mut p = SimilarParams::default();
        p.set("pool_k", 4.0).unwrap();
        let row = |p: &SimilarParams| -> Vec<Scored> {
            more_like_this_scored(None, Some(&premise), 1, MediaType::Tv, Some(&Spinoff), None, p)
        };
        p.set("w_character", 0.0).unwrap();
        let off = row(&p);
        let none = more_like_this_scored(None, Some(&premise), 1, MediaType::Tv, None, None, &p);
        assert_eq!(off, none, "unweighed, the links change nothing");
        assert!(off.iter().all(|s| s.tmdb_id != 3 && s.tmdb_id != 7));
        p.set("w_character", 3.0).unwrap();
        let on = row(&p);
        assert_eq!(on[0].tmdb_id, 3, "{on:?}");
        assert_eq!(on[0].character, 1.0);
        assert!(on.iter().all(|s| s.tmdb_id != 7), "the tonal floor still applies: {on:?}");
    }

    /// Breaking Bad and El Camino: a film the seed's premise and plot put first joins a series seed's row
    /// only while `mix_types` is on, and the series in it keep exactly the order a series-only row gives
    /// them — the other type is merged in between, never reorders the seed's own.
    #[test]
    fn a_mixed_row_adds_the_other_type_and_keeps_the_seeds_own_order() {
        let subs: &[(&str, f64)] = &[("Crime Drama", 0.9)];
        let moods: &[(&str, f64)] = &[("Dark & Gritty", 0.9)];
        let titles: &[crate::index::tests::Row<'_>] = &[
            (1, "tv", "Crime", false, subs, moods, [100, 0, 0]),
            (2, "tv", "Crime", false, subs, moods, [90, 40, 0]),
            (3, "tv", "Crime", false, subs, moods, [80, 60, 0]),
            (4, "tv", "Crime", false, subs, moods, [60, 80, 0]),
            (5, "tv", "Crime", false, subs, moods, [30, 95, 0]),
            (10, "movie", "Crime", false, subs, moods, [99, 10, 0]),
            (11, "movie", "Crime", false, subs, moods, [20, 95, 30]),
            (12, "movie", "Crime", false, subs, moods, [0, 50, 90]),
        ];
        let (premise, plot) = (fixture(titles), fixture(titles));
        let row = |mix: bool| -> Vec<Key> {
            let p = SimilarParams { mix_types: mix, ..SimilarParams::default() };
            more_like_this_scored(Some(&plot), Some(&premise), 1, MediaType::Tv, None, None, &p)
                .iter()
                .map(Scored::key)
                .collect()
        };
        let (single, mixed) = (row(false), row(true));
        assert!(single.iter().all(|k| k.0 == MediaType::Tv), "{single:?}");
        assert!(mixed.contains(&film(10)), "{mixed:?}");
        let own: Vec<Key> = mixed.iter().copied().filter(|k| k.0 == MediaType::Tv).collect();
        assert_eq!(own, single, "the seed's own type, in the order a single-type row gives it");
    }

    /// The map puts a cosine as far from the other type's mean, in its deviations, as it is from the seed
    /// type's; a type with no spread is left as it is.
    #[test]
    fn a_cross_type_cosine_is_read_on_the_seed_types_scale() {
        let own = ScanStats { mean: 0.55, sd: 0.05 };
        let other = ScanStats { mean: 0.51, sd: 0.04 };
        assert!((z_map(0.51, own, other) - 0.55).abs() < 1e-12, "the other type's mean is the seed type's");
        assert!((z_map(0.59, own, other) - 0.65).abs() < 1e-12, "two deviations up is two deviations up");
        assert_eq!(z_map(0.7, own, ScanStats::default()), 0.7);
    }

    /// Beck's tiers: Swedish in full, Nordic (not Britain, which UN M49 would call Northern Europe) at the
    /// region's share, Europe at the continent's, anything else nothing; a co-production's second country
    /// counts, its third does not.
    #[test]
    fn origin_affinity_is_country_then_region_then_continent() {
        let sweden: &[[u8; 2]] = &[*b"SE"];
        assert_eq!(origin_affinity(sweden, &[*b"SE"]), 1.0);
        assert_eq!(origin_affinity(sweden, &[*b"DK"]), REGION_TIER);
        assert_eq!(origin_affinity(sweden, &[*b"IS"]), REGION_TIER, "Nordic, not only Scandinavian");
        assert_eq!(origin_affinity(sweden, &[*b"GB"]), CONTINENT_TIER, "Britain is not Nordic");
        assert_eq!(origin_affinity(sweden, &[*b"US"]), 0.0);
        assert_eq!(origin_affinity(sweden, &[*b"US", *b"SE"]), 1.0, "a co-production's second country");
        assert_eq!(origin_affinity(sweden, &[*b"US", *b"CA", *b"SE"]), 0.0, "but not its third");
        assert_eq!(origin_affinity(&[], &[*b"SE"]), 0.0);
    }

    /// Only a seed from outside the English-language mainstream is regional.
    #[test]
    fn a_seed_is_regional_outside_the_us_and_britain_and_english() {
        assert!(regional(&[*b"SE"], Some(*b"sv")));
        assert!(regional(&[*b"KR"], None));
        assert!(!regional(&[*b"US"], Some(*b"es")), "American");
        assert!(!regional(&[*b"GB"], None), "British");
        assert!(!regional(&[*b"IE"], Some(*b"en")), "in English");
        assert!(!regional(&[], Some(*b"sv")), "no known country");
    }

    /// A candidate with no confident labels is not filtered out: unknown is not none.
    #[test]
    fn an_unlabelled_candidate_is_not_gated_by_the_tonal_floor() {
        let premise = fixture(&[
            (1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [100, 0, 0]),
            (2, "tv", "Crime", false, &[], &[], [90, 0, 0]),
        ]);
        let plot = fixture(&[(1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[], [100, 0, 0])]);
        assert!(more_like_this_pooled(
            Some(&plot),
            Some(&premise),
            1,
            MediaType::Tv,
            None::<&dyn Authorship>,
            None::<&dyn Facets>
        )
        .contains(&tv(2)));
    }

    /// Every knob `KNOBS` names is one `get` and `set` answer, and setting a knob to its own default
    /// leaves the parameters exactly as they were — so a form built from `KNOBS` and pre-filled from
    /// `get` sends production back unchanged.
    #[test]
    fn every_knob_round_trips_through_get_and_set() {
        let defaults = SimilarParams::default();
        for knob in SimilarParams::KNOBS {
            let value = defaults.get(knob.name).unwrap_or_else(|| panic!("{} has no getter", knob.name));
            assert!(
                value >= knob.min && value <= knob.max,
                "{}'s default {value} is outside its range",
                knob.name
            );
            let mut p = defaults;
            p.set(knob.name, value).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(p, defaults, "{} did not round-trip", knob.name);
        }
        assert_eq!(SimilarParams::KNOBS.len(), 46, "a field was added without a knob, or the reverse");
        for knob in SimilarParams::KNOBS {
            assert!(KNOB_GROUPS.contains(&knob.group), "{} is in no known group", knob.name);
        }
        // The per-axis knobs name the store's axes, in its order.
        for (knob, axis) in FACET_AXIS_KNOBS.iter().zip(den_store::FACET_AXES) {
            assert_eq!(*knob, format!("w_facet_{axis}"));
            assert!(SimilarParams::KNOBS.iter().any(|k| k.name == *knob), "{knob} is not a knob");
        }
    }

    #[test]
    fn a_bad_value_is_refused_by_name() {
        let mut p = SimilarParams::default();
        assert!(p.set("w_maker", 11.0).unwrap_err().contains("w_maker"));
        assert!(p.set("w_maker", f64::NAN).unwrap_err().contains("w_maker"));
        assert!(p.set("pool_k", 2.5).unwrap_err().contains("whole number"));
        assert!(p.set("pool_k", 0.0).unwrap_err().contains("pool_k"));
        // The size knobs are bounded, because they are what a request's cost scales with.
        assert!(p.set("pool_k", 1001.0).unwrap_err().contains("pool_k"));
        assert!(p.set("max_row", 401.0).unwrap_err().contains("max_row"));
        assert!(p.set("cap_window", 401.0).unwrap_err().contains("cap_window"));
        assert!(p.set("w_nope", 1.0).unwrap_err().contains("w_nope"));
        assert_eq!(p, SimilarParams::default(), "a refused value must change nothing");
    }

    /// The overrides reach the ranking: the Once case again, with the maker weight at zero, puts the
    /// closer-but-unrelated title back in front — and each title reports the signals it was ranked on.
    #[test]
    fn the_parameters_drive_the_ranking() {
        let seed_subs: &[(&str, f64)] = &[("Romantic Drama", 0.75)];
        let seed_moods: &[(&str, f64)] = &[("Feel-good", 0.6)];
        let premise = fixture(&[
            (1, "movie", "Romance", false, seed_subs, seed_moods, [100, 0, 0]),
            (2, "movie", "Romance", false, seed_subs, seed_moods, [95, 0, 0]),
            (3, "movie", "Drama", false, &[], &[("Feel-good", 0.7)], [60, 0, 0]),
            (4, "movie", "Romance", false, seed_subs, seed_moods, [88, 0, 0]),
            (5, "movie", "Romance", false, seed_subs, seed_moods, [40, 0, 0]),
        ]);
        struct SameHand;
        impl Authorship for SameHand {
            fn nominate(&self) -> Vec<Key> {
                vec![film(3)]
            }
            fn makers(&self, id: Key) -> f64 {
                f64::from(u8::from(id == film(3)))
            }
        }
        let rank = |p: &SimilarParams| {
            more_like_this_scored(None, Some(&premise), 1, MediaType::Movie, Some(&SameHand), None, p)
        };
        let shipped = rank(&SimilarParams::default());
        assert_eq!(shipped[0].tmdb_id, 3);
        assert_eq!(shipped[0].maker, 1.0);
        let ids: Vec<Key> = shipped.iter().map(Scored::key).collect();
        assert_eq!(
            ids,
            more_like_this_pooled(None, Some(&premise), 1, MediaType::Movie, Some(&SameHand), None),
            "the default parameters are what the production entry point ranks with"
        );
        let mut off = SimilarParams::default();
        off.set("w_maker", 0.0).unwrap();
        assert_eq!(rank(&off)[0].tmdb_id, 2, "without the maker weight the closest vector leads");
        off.set("max_row", 2.0).unwrap();
        assert_eq!(rank(&off).len(), 2);
    }

    /// `rank_pool` is everything after retrieval: handed the pool `more_like_this_scored` draws, with its
    /// cosines and in its order, it ranks it identically — so a caller holding no vectors can reproduce a
    /// served row from a pool retrieved elsewhere.
    #[test]
    fn a_retrieved_pool_ranks_as_serving_ranks_it() {
        let seed_subs: &[(&str, f64)] = &[("Romantic Drama", 0.75)];
        let seed_moods: &[(&str, f64)] = &[("Feel-good", 0.6)];
        let premise = fixture(&[
            (1, "movie", "Romance", false, seed_subs, seed_moods, [100, 0, 0]),
            (2, "movie", "Romance", false, seed_subs, seed_moods, [95, 0, 0]),
            (3, "movie", "Drama", false, &[], &[("Feel-good", 0.7)], [60, 0, 0]),
            (4, "movie", "Romance", false, seed_subs, seed_moods, [88, 0, 0]),
            (5, "movie", "Romance", false, seed_subs, seed_moods, [40, 0, 0]),
            (6, "movie", "Romance", false, seed_subs, seed_moods, [0, 100, 0]),
        ]);
        struct SameHand;
        impl Authorship for SameHand {
            fn nominate(&self) -> Vec<Key> {
                vec![film(1), film(6)]
            }
            fn makers(&self, id: Key) -> f64 {
                f64::from(u8::from(id == film(6)))
            }
        }
        let mut p = SimilarParams::default();
        p.set("pool_k", 3.0).unwrap();
        let served =
            more_like_this_scored(None, Some(&premise), 1, MediaType::Movie, Some(&SameHand), None, &p);

        // Retrieval by hand: the premise index's nearest `pool_k`, then the nominations the pool lacks.
        let mut ids: Vec<u32> =
            premise.nearest(1, MediaType::Movie, 3).into_iter().map(|n| n.tmdb_id).collect();
        ids.extend(SameHand.nominate().into_iter().map(|(_, id)| id).filter(|&id| id != 1));
        let seed_row = premise.row_of(1, MediaType::Movie).unwrap();
        let pool: Vec<Candidate> = ids
            .iter()
            .map(|&id| Candidate {
                key: film(id),
                premise: premise.row_of(id, MediaType::Movie).map(|row| premise.similarity(seed_row, row)),
                plot: None,
                plot_rank: None,
            })
            .collect();
        let labels = |(kind, id): Key| premise.labels(id, kind);
        let ranked = rank_pool(&pool, film(1), &labels, Some(&SameHand), None, Extras::default(), &p);

        assert_eq!(ranked, served);
        assert_eq!(served.len(), 4, "three retrieved and one nominated: {served:?}");
        assert!(served.iter().any(|s| s.tmdb_id == 6), "the nomination the vectors missed is ranked");
    }

    /// Facets for the era knobs: the seed (1) holds value 1 on every axis and was released in 2000. Title 2
    /// agrees with it on one axis only, `axis`, and shares its year; title 3 agrees on nothing and is ten
    /// years off. On vectors 3 is well ahead of 2. (An axis a candidate does not answer is skipped, not
    /// counted as a disagreement, so title 2 answers all twelve.)
    struct Era {
        axis: Axis,
    }

    impl Facets for Era {
        fn facets(&self, (_, id): Key) -> Vec<(Axis, ValueId, f64)> {
            match id {
                1 => (0..12).map(|axis| (axis, 1, 1.0)).collect(),
                // Answers every axis, so the ones it disagrees on count against it.
                2 => (0..12).map(|axis| (axis, if axis == self.axis { 1 } else { 2 }, 1.0)).collect(),
                _ => Vec::new(),
            }
        }
        fn year(&self, (_, id): Key) -> Option<f64> {
            Some(if id == 3 { 1990.0 } else { 2000.0 })
        }
        fn prevalence(&self, _: Axis, _: ValueId) -> f64 {
            0.1
        }
    }

    /// Whether title 2 ranks ahead of title 3 for the seed, with these knobs.
    fn two_before_three(axis: Axis, p: &SimilarParams) -> bool {
        let premise = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (3, "movie", "Drama", false, &[], &[], [99, 0, 0]),
            (2, "movie", "Drama", false, &[], &[], [80, 0, 0]),
            (4, "movie", "Drama", false, &[], &[], [70, 0, 0]),
            (5, "movie", "Drama", false, &[], &[], [60, 0, 0]),
            (6, "movie", "Drama", false, &[], &[], [50, 0, 0]),
            (7, "movie", "Drama", false, &[], &[], [40, 0, 0]),
            (8, "movie", "Drama", false, &[], &[], [30, 0, 0]),
        ]);
        let row: Vec<u32> =
            more_like_this_scored(None, Some(&premise), 1, MediaType::Movie, None, Some(&Era { axis }), p)
                .iter()
                .map(|s| s.tmdb_id)
                .collect();
        let at = |id: u32| row.iter().position(|&x| x == id).expect("both titles are ranked");
        at(2) < at(3)
    }

    /// Each per-axis facet weight moves the row on its own, and at production's 1.0 it does not.
    #[test]
    fn each_facet_axis_weight_moves_a_row() {
        for (axis, name) in FACET_AXIS_KNOBS.iter().enumerate() {
            let axis = axis as Axis;
            assert!(!two_before_three(axis, &SimilarParams::default()), "{name}: production");
            let mut p = SimilarParams::default();
            p.set(name, 10.0).unwrap();
            assert!(two_before_three(axis, &p), "{name} = 10 lifts the title agreeing on that axis alone");
        }
    }

    /// The year term: off in production, a lift for the same year once weighted, and the half-life decides
    /// how far ten years apart is from the same year.
    #[test]
    fn the_year_knobs_move_a_row() {
        assert!(!two_before_three(0, &SimilarParams::default()), "production does not weigh the year");
        let mut p = SimilarParams::default();
        p.set("w_year", 0.5).unwrap();
        assert!(two_before_three(0, &p), "the same year beats ten years apart");
        p.set("year_halflife", 100.0).unwrap();
        assert!(!two_before_three(0, &p), "with a century's half-life ten years is nearly the same year");
        assert!((year_proximity(2000.0, 1990.0, 10.0) - 0.5).abs() < 1e-15);
        assert_eq!(year_proximity(2000.0, 2000.0, 10.0), 1.0);
    }

    /// Title 2 is rated 5.0 on 100 votes with popularity 1; title 3 has no rating and popularity 1,000;
    /// every other title has popularity 1.
    struct Viewers;

    impl Audience for Viewers {
        fn rating(&self, (_, id): Key) -> Option<(f64, f64)> {
            (id == 2).then_some((5.0, 100.0))
        }
        fn popularity(&self, (_, id): Key) -> Option<f64> {
            Some(if id == 3 { 1000.0 } else { 1.0 })
        }
    }

    /// The seed (1) and seven candidates, closest first on the premise vectors: 2, 3, then 4..=8.
    fn audience_row(p: &SimilarParams, keep: Option<&dyn Fn(Key) -> bool>) -> Vec<u32> {
        let premise = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (2, "movie", "Drama", false, &[], &[], [95, 0, 0]),
            (3, "movie", "Drama", false, &[], &[], [90, 0, 0]),
            (4, "movie", "Drama", false, &[], &[], [70, 0, 0]),
            (5, "movie", "Drama", false, &[], &[], [60, 0, 0]),
            (6, "movie", "Drama", false, &[], &[], [50, 0, 0]),
            (7, "movie", "Drama", false, &[], &[], [40, 0, 0]),
            (8, "movie", "Drama", false, &[], &[], [30, 0, 0]),
        ]);
        let extras = Extras { audience: Some(&Viewers), keep };
        more_like_this_with(None, Some(&premise), 1, MediaType::Movie, None, None, extras, p)
            .iter()
            .map(|s| s.tmdb_id)
            .collect()
    }

    /// Each audience knob and the `keep` filter moves the row, and at production's values none does.
    #[test]
    fn the_audience_knobs_and_the_keep_filter_move_a_row() {
        let production = audience_row(&SimilarParams::default(), None);
        assert_eq!(&production[..2], [2, 3], "on vectors alone 2 leads 3");
        let with = |knob: &str, value: f64| {
            let mut p = SimilarParams::default();
            p.set(knob, value).unwrap();
            audience_row(&p, None)
        };
        assert!(!with("min_rating", 6.0).contains(&2), "rated 5.0: dropped");
        assert!(with("min_rating", 6.0).contains(&3), "unrated: kept");
        assert!(!with("min_votes", 101.0).contains(&2), "100 votes: dropped");
        assert_eq!(with("min_popularity", 5.0), [3], "only the popular title clears the floor");
        assert_eq!(with("w_popularity", 10.0)[0], 3, "popularity lifts 3 over 2");
        let not_two = |id: Key| id != film(2);
        assert!(!audience_row(&SimilarParams::default(), Some(&not_two)).contains(&2));
    }

    /// The pool floor and the spread percentiles move a row: a candidate the premise index lacks is scored
    /// at the pool floor, and the spread scales what a shared maker adds.
    #[test]
    fn the_pool_percentiles_move_a_row() {
        let premise = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (2, "movie", "Drama", false, &[], &[], [95, 0, 0]),
            (3, "movie", "Drama", false, &[], &[], [90, 0, 0]),
            (4, "movie", "Drama", false, &[], &[], [80, 0, 0]),
            (5, "movie", "Drama", false, &[], &[], [70, 0, 0]),
            (6, "movie", "Drama", false, &[], &[], [40, 0, 0]),
            (7, "movie", "Drama", false, &[], &[], [20, 0, 0]),
            (8, "movie", "Drama", false, &[], &[], [10, 0, 0]),
        ]);
        let plot = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (9, "movie", "Drama", false, &[], &[], [60, 0, 0]),
        ]);
        struct Eight;
        impl Authorship for Eight {
            fn nominate(&self) -> Vec<Key> {
                vec![film(8)]
            }
            fn makers(&self, id: Key) -> f64 {
                f64::from(u8::from(id == film(8)))
            }
        }
        let rank = |knob: &str, value: f64, of: u32| {
            let mut p = SimilarParams::default();
            p.set(knob, value).unwrap();
            let row: Vec<u32> = more_like_this_scored(
                Some(&plot),
                Some(&premise),
                1,
                MediaType::Movie,
                Some(&Eight),
                None,
                &p,
            )
            .iter()
            .map(|s| s.tmdb_id)
            .collect();
            row.iter().position(|&x| x == of).expect("ranked")
        };
        // 9 has no premise vector, so its premise cosine is the pool's floor.
        assert!(rank("pool_floor_pct", 50.0, 9) < rank("pool_floor_pct", 10.0, 9), "a higher floor lifts 9");
        // A narrower spread shrinks what the shared maker adds, and 8 falls.
        assert!(rank("spread_high_pct", 51.0, 8) > rank("spread_high_pct", 90.0, 8), "8 falls");
        assert!(rank("spread_low_pct", 49.0, 8) > rank("spread_low_pct", 10.0, 8), "8 falls");
    }

    /// Production's percentile positions are the integer divisions the scorer always used.
    #[test]
    fn production_percentiles_are_the_old_integer_divisions() {
        for len in 1..2000 {
            assert_eq!(percentile_at(len, POOL_FLOOR_PCT), len / 10);
            assert_eq!(percentile_at(len, SPREAD_HIGH_PCT), len * 9 / 10);
        }
        assert_eq!(percentile_at(10, 100), 9, "held inside the list");
    }

    #[test]
    fn a_title_the_premise_index_lacks_falls_back_to_the_plot() {
        let premise = fixture(&[(7, "movie", "Drama", false, &[], &[], [1, 0, 0])]);
        let plot = fixture(&[
            (1, "movie", "Drama", false, &[], &[], [100, 0, 0]),
            (2, "movie", "Drama", false, &[], &[], [90, 0, 0]),
        ]);
        assert_eq!(more_like_this(Some(&plot), Some(&premise), 1, MediaType::Movie), vec![2]);
    }
}
