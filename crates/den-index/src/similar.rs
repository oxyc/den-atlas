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
