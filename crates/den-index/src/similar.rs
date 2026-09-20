//! More Like This — the index half of the tvOS app's `refineMoreLikeThis`. Merging with TMDB's own
//! recommendations and the theme rerank stay with the client, which holds those.

use crate::{Index, MediaType};
use std::collections::HashSet;

/// Plot neighbours asked for, and the premise candidates weighed before keeping the best of them.
const PLOT_K: usize = 20;
const PREMISE_K: usize = 40;
const KEEP: usize = 20;

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

/// The seed's labels a candidate is measured against: subgenres and moods it carries confidently.
///
/// Weighted by confidence alone. Weighting each label by its rarity as well — `ln(N / titles carrying it)`,
/// so a mood 1,842 titles share counts for less than a specific subgenre — was tried and measured WORSE on
/// this corpus: mean single-subgenre share rose 15% to 17%, The Corner fell from 10th to 13th in The Wire's
/// row, and it did not remove the miss it was aimed at (Angel, which carries both of The Wire's moods and
/// none of its subgenres, held 7th either way). Recorded here so the next person does not re-derive it.
fn seed_labels(labels: &crate::Labels<'_>) -> Vec<(String, f64)> {
    labels
        .subgenres
        .iter()
        .chain(labels.moods.iter())
        .filter(|(_, c)| *c >= MIN_CONFIDENCE)
        .map(|(n, c)| ((*n).to_string(), *c))
        .collect()
}

/// How much of the SEED the candidate covers, confidence-weighted.
///
/// Coverage of the seed, deliberately not Jaccard: a candidate is not less like The Wire for carrying labels
/// The Wire lacks. Jaccard punishes exactly the broad, many-labelled titles this is meant to surface.
fn tone(seed: &[(String, f64)], theirs: &crate::Labels<'_>) -> f64 {
    let total: f64 = seed.iter().map(|(_, c)| c).sum();
    if total <= 0.0 {
        return 1.0; // Unknown is not none: an unlabelled seed gates nothing.
    }
    let has = |name: &str| {
        theirs.subgenres.iter().chain(theirs.moods.iter()).any(|(n, c)| *n == name && *c >= MIN_CONFIDENCE)
    };
    seed.iter().filter(|(n, _)| has(n)).map(|(_, c)| c).sum::<f64>() / total
}

/// Neighbour ids for More Like This, best first — the pooled scorer.
///
/// Three differences from `more_like_this`, each answering a measured defect:
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
///
/// The cross-genre penalty is gone: it punished every one of the seed's own siblings in another genre while
/// waving through anything that merely shared its genre label.
pub fn more_like_this_pooled(
    plot: Option<&Index>,
    premise: Option<&Index>,
    tmdb_id: u32,
    media_type: MediaType,
) -> Vec<u32> {
    let mut pool: Vec<u32> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    for index in [premise, plot].into_iter().flatten() {
        for n in index.nearest(tmdb_id, media_type, POOL_K) {
            if seen.insert(n.tmdb_id) {
                pool.push(n.tmdb_id);
            }
        }
    }
    if pool.is_empty() {
        return Vec::new();
    }

    // Labels come from whichever index holds the seed; both carry the same label set.
    let Some(mine) = premise
        .and_then(|p| p.labels(tmdb_id, media_type))
        .or_else(|| plot.and_then(|p| p.labels(tmdb_id, media_type)))
    else {
        return Vec::new();
    };
    let seed = seed_labels(&mine);

    // One index's cosine between the seed and a candidate, when that index holds both.
    let sim = |index: Option<&Index>, other: u32| -> Option<f64> {
        let index = index?;
        let a = index.row_of(tmdb_id, media_type)?;
        let b = index.row_of(other, media_type)?;
        Some(index.similarity(a, b))
    };

    let raw: Vec<(u32, Option<f64>, Option<f64>)> =
        pool.iter().map(|&id| (id, sim(premise, id), sim(plot, id))).collect();
    // A candidate one index has never seen is scored at that index's pool floor rather than zero, so a
    // missing vector costs it a little and does not disqualify it.
    let floor = |values: Vec<f64>| -> f64 {
        let mut v = values;
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 10]
    };
    let premise_floor = floor(raw.iter().filter_map(|&(_, p, _)| p).collect());
    let plot_floor = floor(raw.iter().filter_map(|&(_, _, l)| l).collect());

    let mut scored: Vec<(u32, f64, String)> = Vec::new();
    for &(id, p, l) in &raw {
        let Some(theirs) = premise
            .and_then(|x| x.labels(id, media_type))
            .or_else(|| plot.and_then(|x| x.labels(id, media_type)))
        else {
            continue;
        };
        if theirs.animated != mine.animated {
            continue;
        }
        let t = tone(&seed, &theirs);
        // The floor is skipped when the candidate carries no confident labels at all — unknown is not none,
        // and filtering on it would silently drop every thinly-labelled title.
        let unlabelled = theirs.subgenres.iter().chain(theirs.moods.iter()).all(|(_, c)| *c < MIN_CONFIDENCE);
        if !unlabelled && t < TONE_FLOOR {
            continue;
        }
        let base = W_PREMISE * p.unwrap_or(premise_floor) + W_PLOT * l.unwrap_or(plot_floor);
        let dominant = theirs
            .subgenres
            .iter()
            .filter(|(_, c)| *c >= MIN_CONFIDENCE)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(n, _)| (*n).to_string())
            .unwrap_or_default();
        scored.push((id, base, dominant));
    }
    if scored.is_empty() {
        return Vec::new();
    }

    // The tonal term is expressed in the pool's own units so one weight works for every seed.
    let mut bases: Vec<f64> = scored.iter().map(|&(_, b, _)| b).collect();
    bases.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let spread = (bases[bases.len() * 9 / 10] - bases[bases.len() / 10]).max(f64::EPSILON);
    let mut final_scored: Vec<(u32, f64, String)> = scored
        .into_iter()
        .map(|(id, base, dominant)| {
            let theirs = premise
                .and_then(|x| x.labels(id, media_type))
                .or_else(|| plot.and_then(|x| x.labels(id, media_type)));
            let t = theirs.as_ref().map_or(0.0, |th| tone(&seed, th));
            (id, base + W_TONE * spread * t, dominant)
        })
        .collect();
    final_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));

    // Greedy pick under the per-subgenre cap, then a second pass to fill from what the cap held back rather
    // than reaching further down a worse tail.
    let mut taken: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<u32> = Vec::new();
    let mut held: Vec<u32> = Vec::new();
    for (id, _, dominant) in &final_scored {
        if out.len() == KEEP {
            break;
        }
        let count = taken.entry(dominant.clone()).or_insert(0);
        if dominant.is_empty() || *count < SUBGENRE_CAP {
            *count += 1;
            out.push(*id);
        } else {
            held.push(*id);
        }
    }
    for id in held {
        if out.len() == KEEP {
            break;
        }
        out.push(id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::fixture;

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
        assert!(more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Tv).contains(&9));
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
        let out = more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Tv);
        assert!(out.contains(&2), "the title sharing the seed's labels must survive");
        assert!(!out.contains(&3), "a same-genre title sharing only a generic mood must not");
        // The shipped scorer keeps the miss and ranks it ABOVE the real neighbour.
        let shipped = more_like_this(Some(&plot), Some(&premise), 1, MediaType::Tv);
        assert_eq!(shipped, vec![3, 2]);
    }

    /// A candidate with no confident labels is not filtered out: unknown is not none.
    #[test]
    fn an_unlabelled_candidate_is_not_gated_by_the_tonal_floor() {
        let premise = fixture(&[
            (1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [100, 0, 0]),
            (2, "tv", "Crime", false, &[], &[], [90, 0, 0]),
        ]);
        let plot = fixture(&[(1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[], [100, 0, 0])]);
        assert!(more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Tv).contains(&2));
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
