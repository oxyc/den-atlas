//! The in-memory fuzzy title index. Candidates are the titles sharing a character trigram with the query;
//! each is scored as the tvOS app's `TitleIndexStore` scores it — the share of the query's trigrams the
//! title contains, blended with popularity, plus an exact/prefix boost scaled by popularity — so a popular
//! near-match beats an obscure literal one. Unlike that store, every title sharing a trigram is scored
//! (it capped candidates at the best few hundred by bm25), so results can only match or improve on it.

use crate::fold::{fold, trigram_keys};
use crate::{MediaType, TitleRecord};
use std::collections::HashMap;

const RELEVANCE_WEIGHT: f64 = 0.6;
const POPULARITY_WEIGHT: f64 = 0.4;
const EXACT_BOOST: f64 = 0.5;
const PREFIX_BOOST: f64 = 0.15;
/// The popularity that already counts as fully popular. Most titles sit at 1–50, so a reference of 50
/// spreads that common range across the score instead of squashing it into a narrow low band.
const POPULARITY_REFERENCE: f64 = 50.0;
/// The share of the query's trigrams a title must contain — 0.6 tolerates a one-character typo.
pub const DEFAULT_MIN_COVERAGE: f64 = 0.6;
/// English leading articles, longest first so "an " wins over "a ". The trailing space keeps "Theatre" and
/// "Avatar" from counting.
const ARTICLES: [&str; 3] = ["the ", "an ", "a "];

/// A search result.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit<'a> {
    pub tmdb_id: u32,
    pub media_type: MediaType,
    pub title: &'a str,
    pub popularity: f64,
}

/// Titles sorted by popularity, plus a trigram → titles map stored flat: sorted keys, and each key's run
/// of title positions in one shared array.
pub struct TitleIndex {
    ids: Vec<u32>,
    kinds: Vec<MediaType>,
    popularity: Vec<f64>,
    titles: Vec<Box<str>>,
    folded: Vec<Box<str>>,
    keys: Vec<u64>,
    key_ends: Vec<u32>,
    postings: Vec<u32>,
}

impl TitleIndex {
    pub fn build(mut records: Vec<TitleRecord>) -> Self {
        records.sort_by(|a, b| {
            b.popularity
                .total_cmp(&a.popularity)
                .then(a.media_type.cmp(&b.media_type))
                .then(a.tmdb_id.cmp(&b.tmdb_id))
        });
        let mut pairs: Vec<(u64, u32)> = Vec::new();
        let mut folded = Vec::with_capacity(records.len());
        for (position, record) in records.iter().enumerate() {
            let f = fold(&record.title);
            let mut keys = trigram_keys(&f);
            keys.sort_unstable();
            keys.dedup();
            pairs.extend(keys.into_iter().map(|k| (k, position as u32)));
            folded.push(f.into_boxed_str());
        }
        pairs.sort_unstable();
        let mut keys = Vec::new();
        let mut key_ends = Vec::new();
        let mut postings = Vec::with_capacity(pairs.len());
        for (key, position) in pairs {
            if keys.last() != Some(&key) {
                if !keys.is_empty() {
                    key_ends.push(postings.len() as u32);
                }
                keys.push(key);
            }
            postings.push(position);
        }
        if !keys.is_empty() {
            key_ends.push(postings.len() as u32);
        }
        TitleIndex {
            ids: records.iter().map(|r| r.tmdb_id).collect(),
            kinds: records.iter().map(|r| r.media_type).collect(),
            popularity: records.iter().map(|r| r.popularity).collect(),
            titles: records.into_iter().map(|r| r.title.into_boxed_str()).collect(),
            folded,
            keys,
            key_ends,
            postings,
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Roughly what the index holds in memory, for logs.
    pub fn approx_bytes(&self) -> usize {
        let strings: usize = self.titles.iter().chain(&self.folded).map(|s| s.len() + 16).sum();
        strings
            + self.ids.len() * (4 + 1 + 8)
            + self.keys.len() * 8
            + self.key_ends.len() * 4
            + self.postings.len() * 4
    }

    /// The best `limit` titles for `query`, optionally of one media type, best first.
    pub fn search(&self, query: &str, media_type: Option<MediaType>, limit: usize) -> Vec<Hit<'_>> {
        self.search_with(query, media_type, limit, DEFAULT_MIN_COVERAGE)
    }

    pub fn search_with(
        &self,
        query: &str,
        media_type: Option<MediaType>,
        limit: usize,
        min_coverage: f64,
    ) -> Vec<Hit<'_>> {
        let q = fold(query);
        if q.chars().count() < 2 || limit == 0 {
            return Vec::new();
        }
        let wanted = |i: usize| media_type.is_none_or(|m| self.kinds[i] == m);
        let mut query_keys = trigram_keys(&q);
        query_keys.sort_unstable();
        query_keys.dedup();
        if query_keys.is_empty() {
            // Two characters make no trigram: the most popular titles that start with them.
            return (0..self.len())
                .filter(|&i| wanted(i) && self.folded[i].starts_with(q.as_str()))
                .take(limit)
                .map(|i| self.hit(i))
                .collect();
        }
        let mut shared: HashMap<u32, u32> = HashMap::new();
        for key in &query_keys {
            for &position in self.postings(*key) {
                *shared.entry(position).or_insert(0) += 1;
            }
        }
        let denominator = query_keys.len() as f64;
        let typed_article = leading_article(&q).is_some();
        let mut scored: Vec<(f64, u32)> = shared
            .into_iter()
            .filter_map(|(position, count)| {
                let coverage = f64::from(count) / denominator;
                let i = position as usize;
                (coverage >= min_coverage && wanted(i))
                    .then(|| (self.score(i, coverage, &q, typed_article), position))
            })
            .collect();
        // Ties go to the more popular title (a lower position), so the order never depends on the map.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(limit).map(|(_, position)| self.hit(position as usize)).collect()
    }

    fn postings(&self, key: u64) -> &[u32] {
        let Ok(i) = self.keys.binary_search(&key) else { return &[] };
        let start = if i == 0 { 0 } else { self.key_ends[i - 1] as usize };
        &self.postings[start..self.key_ends[i] as usize]
    }

    /// The exact/prefix boost is scaled by popularity, so a typed title earns a big lift only if it's also a
    /// plausible target: "matrix" lifts the iconic "The Matrix", while an obscure "Matrix" gets a sliver.
    /// A leading article is dropped from the title unless the query typed one itself.
    fn score(&self, i: usize, coverage: f64, query: &str, typed_article: bool) -> f64 {
        let popularity = (self.popularity[i].max(0.0).ln_1p() / POPULARITY_REFERENCE.ln_1p()).min(1.0);
        let title: &str = if typed_article { &self.folded[i] } else { drop_article(&self.folded[i]) };
        let boost = if title == query {
            EXACT_BOOST
        } else if title.starts_with(query) {
            PREFIX_BOOST
        } else {
            0.0
        };
        RELEVANCE_WEIGHT * coverage + POPULARITY_WEIGHT * popularity + boost * popularity
    }

    fn hit(&self, i: usize) -> Hit<'_> {
        Hit {
            tmdb_id: self.ids[i],
            media_type: self.kinds[i],
            title: &self.titles[i],
            popularity: self.popularity[i],
        }
    }
}

fn leading_article(s: &str) -> Option<&'static str> {
    ARTICLES.iter().copied().find(|a| s.starts_with(a))
}

fn drop_article(s: &str) -> &str {
    leading_article(s).map_or(s, |a| &s[a.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u32, media_type: MediaType, title: &str, popularity: f64) -> TitleRecord {
        TitleRecord { tmdb_id: id, media_type, title: title.to_owned(), popularity }
    }

    fn index() -> TitleIndex {
        TitleIndex::build(vec![
            record(1, MediaType::Movie, "The Avengers", 90.0),
            record(2, MediaType::Movie, "The Cave", 40.0),
            record(3, MediaType::Movie, "The Matrix", 80.0),
            record(4, MediaType::Movie, "Matrix", 2.0),
            record(5, MediaType::Tv, "Pokémon", 60.0),
            record(6, MediaType::Movie, "Matrix Reloaded", 45.0),
            record(7, MediaType::Tv, "Mad Men", 30.0),
        ])
    }

    fn ids(hits: &[Hit<'_>]) -> Vec<u32> {
        hits.iter().map(|h| h.tmdb_id).collect()
    }

    #[test]
    fn a_typo_still_finds_the_title() {
        let idx = index();
        assert_eq!(ids(&idx.search("avengrs", None, 10)), vec![1]);
    }

    #[test]
    fn the_popular_title_beats_an_obscure_exact_one() {
        let idx = index();
        assert_eq!(ids(&idx.search("matrix", None, 3)), vec![3, 6, 4]);
    }

    #[test]
    fn a_typed_article_is_taken_literally() {
        let idx = index();
        assert_eq!(idx.search("the matrix", None, 1)[0].tmdb_id, 3);
    }

    #[test]
    fn diacritics_and_case_are_folded() {
        let idx = index();
        assert_eq!(ids(&idx.search("POKEMON", None, 5)), vec![5]);
    }

    #[test]
    fn two_characters_prefix_match_in_popularity_order() {
        let idx = index();
        assert_eq!(ids(&idx.search("ma", None, 5)), vec![6, 7, 4]);
    }

    #[test]
    fn filters_by_media_type() {
        let idx = index();
        assert_eq!(ids(&idx.search("ma", Some(MediaType::Tv), 5)), vec![7]);
        assert!(idx.search("pokemon", Some(MediaType::Movie), 5).is_empty());
    }

    #[test]
    fn an_unrelated_query_finds_nothing() {
        let idx = index();
        assert!(idx.search("zzzzzz", None, 5).is_empty());
        assert!(idx.search("a", None, 5).is_empty());
    }

    #[test]
    fn the_same_records_in_any_order_give_the_same_results() {
        let mut records = vec![
            record(1, MediaType::Movie, "Alien", 10.0),
            record(2, MediaType::Tv, "Alien", 10.0),
            record(3, MediaType::Movie, "Aliens", 10.0),
        ];
        let forward = ids(&TitleIndex::build(records.clone()).search("alien", None, 5));
        records.reverse();
        assert_eq!(ids(&TitleIndex::build(records).search("alien", None, 5)), forward);
    }
}
