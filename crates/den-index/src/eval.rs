//! How good a ranked row is against hand judgements: graded nDCG and its companions.
//!
//! Here rather than beside the judged set so that every place a row is scored — den-atlas's `rail-eval`,
//! its tuning playground, and a build of this crate running in a browser — computes one number the same
//! way. Pure: a row of ids and a map of grades in, numbers out.

use std::collections::HashMap;
use std::hash::Hash;

/// A judgement of one candidate as a recommendation for one seed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grade {
    Good,
    Ok,
    Bad,
}

impl Grade {
    /// The spelling the judged files use: `good`, `ok`, `bad`.
    pub fn parse(s: &str) -> Option<Grade> {
        match s {
            "good" => Some(Grade::Good),
            "ok" => Some(Grade::Ok),
            "bad" => Some(Grade::Bad),
            _ => None,
        }
    }

    pub fn gain(self) -> f64 {
        match self {
            Grade::Good => 2.0,
            Grade::Ok => 1.0,
            Grade::Bad => 0.0,
        }
    }
}

/// One row's quality at `k`, or a mean over several rows.
///
/// - `ndcg` — graded, gain 2 / 1 / 0 for good / ok / bad, an unjudged title scoring 0. The ideal is the
///   case's own judgements sorted best first.
/// - `condensed` — the same over the row with unjudged titles removed first (Sakai's condensed list), so
///   surfacing a good title nobody judged yet is not read as a loss.
/// - `precision` — the share of the first `k` judged good or ok.
/// - `bad` — judged-bad titles in the first `k`.
/// - `judged` — how many of the first `k` carry a judgement at all.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Scores {
    pub ndcg: f64,
    pub condensed: f64,
    pub precision: f64,
    pub bad: usize,
    pub judged: usize,
}

/// Discounted cumulative gain of gains in rank order.
pub fn dcg(gains: impl Iterator<Item = f64>) -> f64 {
    gains.enumerate().map(|(i, g)| g / ((i + 2) as f64).log2()).sum()
}

/// One row against one case's judgements, at `k`. The ideal is over every judgement the case holds, so a
/// row that cannot reach some of them — films for a series seed, in a row of one type — is scored against
/// the same ideal as one that can.
pub fn score<K: Eq + Hash>(row: &[K], grades: &HashMap<K, Grade>, k: usize) -> Scores {
    let mut ideal: Vec<f64> = grades.values().map(|g| g.gain()).collect();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg = dcg(ideal.into_iter().take(k));
    let ratio = |d: f64| if idcg > 0.0 { d / idcg } else { 0.0 };

    let top = &row[..row.len().min(k)];
    let gain = |id: &K| grades.get(id).map_or(0.0, |g| g.gain());
    let condensed: Vec<&K> = row.iter().filter(|id| grades.contains_key(id)).take(k).collect();
    let count =
        |want: &[Grade]| top.iter().filter(|id| grades.get(id).is_some_and(|g| want.contains(g))).count();
    Scores {
        ndcg: ratio(dcg(top.iter().map(gain))),
        condensed: ratio(dcg(condensed.into_iter().map(gain))),
        precision: count(&[Grade::Good, Grade::Ok]) as f64 / k as f64,
        bad: count(&[Grade::Bad]),
        judged: top.iter().filter(|id| grades.contains_key(id)).count(),
    }
}

/// Means over a set of cases; `bad` and `judged` are summed, since a count per case is what they are.
pub fn mean(all: &[Scores]) -> Scores {
    let n = all.len().max(1) as f64;
    Scores {
        ndcg: all.iter().map(|s| s.ndcg).sum::<f64>() / n,
        condensed: all.iter().map(|s| s.condensed).sum::<f64>() / n,
        precision: all.iter().map(|s| s.precision).sum::<f64>() / n,
        bad: all.iter().map(|s| s.bad).sum(),
        judged: all.iter().map(|s| s.judged).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grades(pairs: &[(u32, Grade)]) -> HashMap<u32, Grade> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn the_ideal_order_scores_one_and_its_reverse_does_not() {
        let g = grades(&[(1, Grade::Good), (2, Grade::Ok), (3, Grade::Bad)]);
        let best = score(&[1, 2, 3], &g, 10);
        assert!((best.ndcg - 1.0).abs() < 1e-12 && (best.condensed - 1.0).abs() < 1e-12);
        let worst = score(&[3, 2, 1], &g, 10);
        assert!(worst.ndcg < 0.8, "{worst:?}");
        assert_eq!((worst.bad, worst.judged), (1, 3));
        assert!((worst.precision - 0.2).abs() < 1e-12, "two of ten are good or ok");
    }

    /// An unjudged title costs plain nDCG and nothing on the condensed list: it is unknown, not bad.
    #[test]
    fn an_unjudged_title_costs_plain_ndcg_only() {
        let g = grades(&[(1, Grade::Good)]);
        let s = score(&[99, 1], &g, 10);
        assert!((s.ndcg - 1.0 / 3f64.log2()).abs() < 1e-12, "{s:?}");
        assert!((s.condensed - 1.0).abs() < 1e-12, "{s:?}");
        assert_eq!(s.judged, 1);
    }

    /// Past k counts for nothing, and a case with only bad judgements has no ideal to reach.
    #[test]
    fn only_the_first_k_count() {
        let g = grades(&[(1, Grade::Good)]);
        assert_eq!(score(&[5, 6, 1], &g, 2).ndcg, 0.0);
        assert_eq!(score(&[1], &grades(&[(1, Grade::Bad)]), 10).ndcg, 0.0);
    }

    #[test]
    fn a_grade_is_read_by_its_file_spelling_only() {
        assert_eq!(Grade::parse("good"), Some(Grade::Good));
        assert_eq!(Grade::parse("ok"), Some(Grade::Ok));
        assert_eq!(Grade::parse("bad"), Some(Grade::Bad));
        assert_eq!(Grade::parse("Good"), None);
    }
}
