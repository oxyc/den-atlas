//! TMDB's vote count and score for each store row (oxyc/den#118), out of what `tmdb.rs` keeps.
//!
//! Browse row order is a vote count, `/recommend` rates a title with a score, and the store no longer carries
//! either: the dataset is public and TMDB's numbers may not be published. `tmdb.rs` fetches them on the box and
//! keeps them in `CACHE_DIR`; this joins what it keeps onto the store's rows by the store's own `keys` column,
//! so what stays resident is one `u32` and one `f32` per STORE row (~190 KB each). The rules on what these
//! numbers may be used for are at the top of `tmdb.rs`.
//!
//! The index is rebuilt whenever `tmdb.rs` changes what it keeps, and a fetched count past TMDB's six months
//! has already been dropped there, so nothing here is older than that.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// TMDB's vote count and score for the store rows it has one for, indexed BY STORE ROW so a lookup is an
/// array read beside the row lookup `votes_of` already does.
pub struct RatingsIndex {
    /// `vote_count` per store row; 0 where nothing is kept for that row.
    votes: Vec<u32>,
    /// `vote_average` per store row, on TMDB's 0-10 scale; 0.0 where absent.
    ratings: Vec<f32>,
    /// How many rows have a count. Never 0 — `build` refuses an index that matched nothing.
    matched: usize,
}

/// Its coverage, never its contents: two vectors the length of the corpus help nobody in a test failure —
/// the same reason `MappedStore` names the store rather than printing it.
impl std::fmt::Debug for RatingsIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RatingsIndex").field("matched", &self.matched).field("rows", &self.rows()).finish()
    }
}

impl RatingsIndex {
    /// TMDB's vote count for a store row, `None` where nothing is kept.
    pub fn votes(&self, row: usize) -> Option<u32> {
        self.votes.get(row).copied().filter(|&votes| votes > 0)
    }

    /// TMDB's score for a store row, `None` where nothing is kept.
    pub fn rating(&self, row: usize) -> Option<f32> {
        self.ratings.get(row).copied().filter(|&rating| rating > 0.0)
    }

    /// Both at once, for a caller that wants a rating only when a count stands behind it.
    pub fn of(&self, row: usize) -> Option<(u32, f32)> {
        Some((self.votes(row)?, self.rating(row)?))
    }

    /// How many store rows have a count.
    pub fn matched(&self) -> usize {
        self.matched
    }

    /// The store rows this was built over — the denominator for `matched`.
    pub fn rows(&self) -> usize {
        self.votes.len()
    }
}

/// The live holder: `tmdb.rs` swaps a new index in, and the query indexes read whichever is current.
#[derive(Default)]
pub struct Ratings {
    index: RwLock<Option<Arc<RatingsIndex>>>,
}

impl Ratings {
    #[cfg(test)]
    pub fn with_index(index: RatingsIndex) -> Self {
        Ratings { index: RwLock::new(Some(Arc::new(index))) }
    }

    /// The current index; `None` until the first build, and while nothing is kept.
    pub fn index(&self) -> Option<Arc<RatingsIndex>> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Swap an index in, as a rebuild does.
    pub fn set(&self, index: Option<RatingsIndex>) {
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = index.map(Arc::new);
    }
}

/// One title's kept numbers, by the store's key halves: `(media, tmdb id)`, media 0 a film and 1 a series.
pub type Key = (u8, u32);

/// TMDB's numbers joined onto a store's rows through its `keys` column.
///
/// An index that names no row is an error, not an index of zeros: that would order every browse row by
/// nothing while every request still answered 200.
pub fn build(view: &den_store::Store<'_>, kept: &HashMap<Key, (f32, u32)>) -> Result<RatingsIndex, String> {
    let keys = view.per_row::<u64>("keys").map_err(|e| e.to_string())?;
    let mut votes = vec![0u32; keys.len()];
    let mut ratings = vec![0f32; keys.len()];
    let mut matched = 0usize;
    for (row, &packed) in keys.iter().enumerate() {
        let key = (u8::from(packed >> 32 == 1), packed as u32);
        let Some(&(average, count)) = kept.get(&key) else { continue };
        if count == 0 || !average.is_finite() || average <= 0.0 {
            continue;
        }
        votes[row] = count;
        ratings[row] = average;
        matched += 1;
    }
    if matched == 0 {
        return Err(format!("TMDB's kept numbers named none of the store's {} rows", keys.len()));
    }
    Ok(RatingsIndex { votes, ratings, matched })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Over the route fixture: rows are ordered by packed key, so movie 1 is row 0.
    #[test]
    fn joins_kept_numbers_onto_the_stores_rows_by_key() {
        let dir = std::env::temp_dir().join(format!("den-atlas-ratings-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        let kept = HashMap::from([
            ((0, 1), (8.4, 9000)),
            // A title the store does not hold, and one with no votes.
            ((0, 999_999), (9.3, 29_000)),
            ((1, 1), (0.0, 0)),
        ]);
        let index = build(&mapped.view(), &kept).expect("movie 1 is in the store");
        assert_eq!(index.rows(), 12, "one slot per fixture row");
        assert_eq!(index.matched(), 1);
        assert_eq!(index.of(0), Some((9000, 8.4)));
        assert!(!index.votes.contains(&29_000), "a title the store lacks is never allocated");
        assert!((1..12).all(|row| index.of(row).is_none()), "a zero-vote title is absent, not zero");

        let e = build(&mapped.view(), &HashMap::from([((0, 999_999), (9.3, 29_000))]))
            .expect_err("nothing matched");
        assert!(e.contains("named none of the store's 12 rows"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
