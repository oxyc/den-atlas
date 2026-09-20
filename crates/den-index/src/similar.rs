//! More Like This — the index half of the tvOS app's `refineMoreLikeThis`. Merging with TMDB's own
//! recommendations and the theme rerank stay with the client, which holds those.

use crate::{Index, MediaType};
use std::collections::HashSet;

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
fn seed_labels(labels: &crate::Labels<'_>) -> Seed {
    let keep = |pairs: &[(&str, f64)]| -> Vec<(String, f64)> {
        pairs.iter().filter(|(_, c)| *c >= MIN_CONFIDENCE).map(|(n, c)| ((*n).to_string(), *c)).collect()
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
fn tone(seed: &Seed, theirs: &crate::Labels<'_>) -> f64 {
    // A family the candidate says nothing in asks nothing of it, and neither does one the seed is silent on.
    let covered = |family: &[(String, f64)], theirs: &[(&str, f64)]| -> Option<f64> {
        let total: f64 = family.iter().map(|(_, c)| c).sum();
        if total <= 0.0 || !theirs.iter().any(|(_, c)| *c >= MIN_CONFIDENCE) {
            return None;
        }
        let has = |name: &str| theirs.iter().any(|(n, c)| *n == name && *c >= MIN_CONFIDENCE);
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
const W_MAKER: f64 = 1.20;
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
fn critique_coverage(defining: &[(String, f64)], theirs: &[(String, f64)]) -> Option<f64> {
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
fn noul_cosine(seed: &[(String, f64)], theirs: &[(String, f64)]) -> Option<f64> {
    if seed.is_empty() || theirs.is_empty() {
        return None;
    }
    let mut dot = 0.0;
    for (name, p) in seed {
        if let Some((_, q)) = theirs.iter().find(|(n, _)| n == name) {
            dot += p * q;
        }
    }
    let norm = |v: &[(String, f64)]| v.iter().map(|(_, p)| p * p).sum::<f64>().sqrt();
    let d = norm(seed) * norm(theirs);
    (d > 0.0).then(|| dot / d)
}

/// One title's facet choices: axis -> (value, confidence). Supplied by the caller for the same reason as
/// `Authorship` — `den-index` does not know where a facet comes from.
pub trait Facets {
    fn facets(&self, tmdb_id: u32) -> Vec<(String, String, f64)>;
    /// The seed's DEFINING arguments — axes it reads >= 0.8 on — each with its idf weight, and a
    /// candidate's raw probability on them. Coverage of these, not cosine over all seventeen.
    fn critique_defining(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        let _ = tmdb_id;
        Vec::new()
    }
    /// A candidate's raw (uncentered) critique probabilities, for coverage.
    fn critique_raw(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        let _ = tmdb_id;
        Vec::new()
    }
    /// Whether this candidate is among the seed's `n` best by critique coverage, corpus-wide.
    fn critique_top(&self, tmdb_id: u32, other: u32, n: usize) -> bool {
        let _ = (tmdb_id, other, n);
        false
    }
    /// The critique profile — what the work argues about — CENTERED on the corpus mean per axis, so the
    /// caller does the centering once rather than every comparison.
    fn critique(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        let _ = tmdb_id;
        Vec::new()
    }
    /// The 75 taxonomy nouls with their probabilities, for the cosine term.
    fn nouls(&self, tmdb_id: u32) -> Vec<(String, f64)> {
        let _ = tmdb_id;
        Vec::new()
    }
    /// How far this title is from a realist world: vampires, superheroes, time travel, the apocalypse.
    /// 0 for The Wire, 0.97 for Angel.
    fn world(&self, tmdb_id: u32) -> f64 {
        let _ = tmdb_id;
        0.0
    }
    /// Share of the corpus carrying this axis value, for rarity weighting. A shared `chronology = linear`
    /// is worth almost nothing (76% of titles) where a shared `conflict = person-vs-system` is worth a lot.
    fn prevalence(&self, axis: &str, value: &str) -> f64;
}

/// Agreement between two titles' facets, confidence-weighted and rarity-weighted, in 0..=1.
fn facet_agreement(f: &dyn Facets, seed: &[(String, String, f64)], other: u32) -> Option<f64> {
    let theirs = f.facets(other);
    if seed.is_empty() || theirs.is_empty() {
        return None; // Unknown is not none.
    }
    let mut num = 0.0;
    let mut den = 0.0;
    for (axis, value, conf) in seed {
        let Some((_, their_value, their_conf)) = theirs.iter().find(|(a, _, _)| a == axis) else { continue };
        // ln(1/prevalence): a value the whole corpus shares carries almost no evidence.
        let weight = conf * (1.0 / f.prevalence(axis, value).max(1e-6)).ln().max(0.0);
        den += weight;
        if their_value == value {
            num += weight * their_conf;
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
    /// Ids that share a maker or a home with the seed, whatever the vectors think of them.
    fn nominate(&self) -> Vec<u32>;
    /// Share of the seed's makers this candidate shares, 0..=1.
    fn makers(&self, tmdb_id: u32) -> f64;
    /// Share of the seed's broadcasters/production companies this candidate shares, 0..=1.
    fn home(&self, tmdb_id: u32) -> f64 {
        let _ = tmdb_id;
        0.0
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
    // Facts nominate too. Without this a shared maker can only re-order what the vectors already found, and
    // the vectors do not find a seed's own siblings.
    for id in authorship.map(Authorship::nominate).unwrap_or_default() {
        if id != tmdb_id && seen.insert(id) {
            pool.push(id);
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
    let seed_facets: Vec<(String, String, f64)> = facets.map(|f| f.facets(tmdb_id)).unwrap_or_default();
    let seed_world = facets.map_or(0.0, |f| f.world(tmdb_id));
    let seed_nouls: Vec<(String, f64)> = facets.map(|f| f.nouls(tmdb_id)).unwrap_or_default();
    let seed_critique: Vec<(String, f64)> = facets.map(|f| f.critique(tmdb_id)).unwrap_or_default();
    let seed_defining: Vec<(String, f64)> = facets.map(|f| f.critique_defining(tmdb_id)).unwrap_or_default();

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
        let maker = authorship.map_or(0.0, |a| a.makers(id));
        // The floor is skipped when the candidate carries no confident labels at all — unknown is not none,
        // and filtering on it would silently drop every thinly-labelled title. A shared maker also exempts
        // it: labels are a guess about a title, authorship is a fact about it, and the fact wins.
        let unlabelled = theirs.subgenres.iter().chain(theirs.moods.iter()).all(|(_, c)| *c < MIN_CONFIDENCE);
        if !unlabelled && maker <= 0.0 && t < TONE_FLOOR {
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
            let maker = authorship.map_or(0.0, |a| a.makers(id));
            let home = authorship.map_or(0.0, |a| a.home(id));
            // A candidate with no facets scores the term at 0 rather than being penalised or exempted: it
            // simply brings no facet evidence, which is different from bringing disagreeing evidence.
            let fa = facets.and_then(|f| facet_agreement(f, &seed_facets, id)).unwrap_or(0.0);
            let world = facets.map_or(0.0, |f| (f.world(id) - seed_world).abs());
            let nc = facets
                .and_then(|f| noul_cosine(&seed_nouls, &f.nouls(id)))
                .unwrap_or(0.0);
            // Already centered, so this can be negative — arguing about different things is evidence
            // against a pair, not merely absence of evidence for it.
            let cr = facets
                .and_then(|f| noul_cosine(&seed_critique, &f.critique(id)))
                .unwrap_or(0.0);
            let cov = facets
                .and_then(|f| critique_coverage(&seed_defining, &f.critique_raw(id)))
                .unwrap_or(0.0);
            let score = base
                + spread
                    * (W_TONE * t + W_NOUL * nc + W_CRITIQUE * cr + W_COVERAGE * cov + W_MAKER * maker
                        + W_HOME * home
                        + W_FACET * fa
                        - W_WORLD * world);
            (id, score, dominant)
        })
        .collect();
    final_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));

    // Greedy pick under the per-subgenre cap, then a second pass to fill from what the cap held back rather
    // than reaching further down a worse tail. The cap counts against the first twenty — a row of two
    // hundred should not be three police procedurals and then nothing else from the genre.
    let mut taken: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<u32> = Vec::new();
    let mut held: Vec<u32> = Vec::new();
    for (id, _, dominant) in &final_scored {
        if out.len() == MAX_ROW {
            break;
        }
        let count = taken.entry(dominant.clone()).or_insert(0);
        // Past the first screenful the cap stops applying: it exists to keep the visible row varied, and
        // beyond that it would start excluding good answers for being the same kind of thing.
        if dominant.is_empty() || out.len() >= KEEP || *count < SUBGENRE_CAP {
            *count += 1;
            out.push(*id);
        } else {
            held.push(*id);
        }
    }
    // The held items go back in right after the visible screenful, in score order — NOT at the end of the
    // row. Appended, they were the last thing added to a 200-long list, so on a dense anchor the main loop
    // filled MAX_ROW first and they were dropped entirely: The Wire's 11th highest-scoring candidate is
    // Homicide: Life on the Street, and a 200-title "more like The Wire" had no Homicide in it.
    //
    // The cap's job is the first twenty. Past that, a held item is simply the next-best answer.
    let tail = out.split_off(out.len().min(KEEP));
    out.extend(held);
    out.extend(tail);
    out.truncate(MAX_ROW);
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
        assert!(more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Tv, None::<&dyn Authorship>, None::<&dyn Facets>).contains(&9));
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
        let out = more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Tv, None::<&dyn Authorship>, None::<&dyn Facets>);
        assert!(out.contains(&2), "the title sharing the seed's labels must survive");
        assert!(!out.contains(&3), "a same-genre title sharing only a generic mood must not");
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
            (2, "movie", "Romance", false, &[("Romantic Drama", 0.8), ("Musical", 0.75)], &[("Feel-good", 0.9)], [95, 0, 0]),
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
        let none = more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Movie, None::<&dyn Authorship>, None::<&dyn Facets>);
        assert_eq!(none.first(), Some(&2), "on vectors alone the closer, unrelated title leads");
        assert!(none.iter().position(|x| *x == 3).is_some_and(|p| p > 2), "and the sibling sits down the row");
        struct SameHand;
        impl Authorship for SameHand {
            fn nominate(&self) -> Vec<u32> {
                vec![3]
            }
            fn makers(&self, id: u32) -> f64 {
                if id == 3 { 1.0 } else { 0.0 }
            }
        }
        let with = more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Movie, Some(&SameHand), None::<&dyn Facets>);
        assert_eq!(with.first(), Some(&3), "the same hand outranks a closer but unrelated title");
    }

    /// A candidate with no confident labels is not filtered out: unknown is not none.
    #[test]
    fn an_unlabelled_candidate_is_not_gated_by_the_tonal_floor() {
        let premise = fixture(&[
            (1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[("Dark & Gritty", 0.95)], [100, 0, 0]),
            (2, "tv", "Crime", false, &[], &[], [90, 0, 0]),
        ]);
        let plot = fixture(&[(1, "tv", "Crime", false, &[("Police Procedural", 0.9)], &[], [100, 0, 0])]);
        assert!(more_like_this_pooled(Some(&plot), Some(&premise), 1, MediaType::Tv, None::<&dyn Authorship>, None::<&dyn Facets>).contains(&2));
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
