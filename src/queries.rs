//! Index queries (`den-index`) over the dataset's plot and premise indexes: the label taxonomy, label rows
//! and More Like This — and the Wikidata facts `/recommend` reads beside them. The indexes load on the first
//! query and are released after a few idle minutes, so an atlas nobody is asking holds none of their ~80 MB
//! of vectors; the first query after an idle spell pays the load from disk.
//!
//! # One artifact
//!
//! Everything below is read out of the mmap'd store (den-spec `wire/store-v1.md`) and nothing else. There
//! used to be seven inputs — `labels-t02.json`, `vectors-bge-m3.bin`, `labels-premise.json`,
//! `vectors-premise.bin`, `metadata-*.json`, `facets.bin`, a facts sidecar and a plot-facets sidecar —
//! each with its own reader, its own parse and its own idea of how many titles the corpus has. They
//! disagreed: `facets.bin` covered 9,086 fewer titles than the store, the plot-facets sidecar 42,282
//! fewer, and a title missing from one of them lost its row order or its attribute search with nothing
//! failing. One artifact cannot disagree with itself, which is the whole point of the cut.

use crate::characters::Characters;
use crate::dataset::Dataset;
use crate::facts::Facts;
use crate::filter::FilterIndex;
use crate::fit::Corpus;
use crate::plotrows::{cards_from_store, Card, PlotFacets};
use crate::ratings::{Ratings, RatingsIndex};
use crate::store::MappedStore;
use crate::util::lock;
use den_index::{FacetIndex, Index};
use den_titlesearch::{TitleIndex, TitleRecord};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

/// How long the indexes stay in memory after the last query.
const IDLE_RELEASE: Duration = Duration::from_secs(10 * 60);
const SWEEP_EVERY: Duration = Duration::from_secs(60);

pub struct Indexes {
    /// Every title in the corpus — the store's own row count. The indexes below may be partial (the plot
    /// index covers the rows that have a plot vector); this is their denominator.
    pub population: usize,
    pub dataset_version: String,
    pub plot: Index,
    /// The premise index, when the store's premise vector sections read; without it More Like This is
    /// plot-only.
    pub premise: Option<Index>,
    /// The facet index — country, original language, year, votes — filled from the store's own columns.
    /// Without it the facet lane answers nothing.
    pub facets: Option<FacetIndex>,
    /// The Wikidata facts; without them `/recommend` reads labels and facets alone.
    pub facts: Option<Facts>,
    /// Each franchise series' strength (`series::SeriesStrength`), out of the facts and the plot index;
    /// empty without the facts.
    pub series: crate::series::SeriesStrength,
    /// The plot facets, and the cards their rows are drawn with; without both, `/index/plot` rows are empty.
    pub plot_facets: Option<PlotFacets>,
    /// The mapped store and its corpus-wide aggregates: the artifact everything above was read out of,
    /// kept because the rail addresses its columns per candidate rather than copying them.
    pub store: crate::store::LoadedStore,
    /// TMDB's vote counts and scores, joined onto the store's rows (`ratings`, kept by `tmdb`). The LIVE
    /// holder, not a snapshot: the indexes outlive a refresh, and a vote count that reached the process should
    /// reach the next row it orders rather than waiting for an idle release. `None` without the index routes'
    /// TMDB half, and empty while nothing is kept — see `votes_of`.
    pub ratings: Option<Arc<Ratings>>,
    /// Titles that share a character, from TMDB's credits (`characters`). The live holder, like `ratings`.
    pub characters: Option<Arc<Characters>>,
    pub cards: Option<HashMap<(den_index::MediaType, u32), Card>>,
    /// The iconic studios (`studios.rs`); empty for a store without their sections.
    pub studios: crate::studios::Studios,
    /// The cards' display titles as a fuzzy title index, for search: TMDB's export names a title by its original
    /// title, so "parasite" finds only what is displayed as "Parasite" here.
    pub display: Option<TitleIndex>,
    /// More Like This answers already worked out, by title (`Indexes::more_like_this`).
    /// Keyed by title and whether the row mixes types.
    similar: Mutex<HashMap<SimilarKey, Arc<[Key]>>>,
    /// Row orders already worked out, by type and constraints (`Indexes::row_order`).
    rows: Mutex<HashMap<String, Arc<[Key]>>>,
    /// What a billboard's fit reads off the index as a whole (`Indexes::corpus`).
    corpus: OnceLock<Corpus>,
    /// Corpus aggregates for a critique floor and holds share other than production's, by their bits
    /// (`Indexes::aggregates_for`).
    aggregates: Mutex<HashMap<(u64, u64), Arc<den_index::RailAggregates>>>,
    /// Every filter kind's values as the titles carrying them (`Indexes::filter`).
    filter: OnceLock<FilterIndex>,
    /// `/index/schema.json`, serialised (`Indexes::schema_json`).
    schema: OnceLock<String>,
}

type Key = (den_index::MediaType, u32);
/// A More Like This row in the memo: the title, and whether the row mixes films and series.
type SimilarKey = (Key, bool);

/// More Like This answers and row orders kept at most: bounded, and simply started over when full. Both are small
/// (a row order is at most a few thousand titles, an answer at most `den_index::MAX_ROW` ids).
///
/// `ROW_MEMO` covers each row twice over: once untilted, and once per (household taste x weights) asking for
/// it. 128 was a row per key; a tilt forks that key space, so a household browsing thirty rows would clear
/// the memo on its own and pay every other household's order again.
///
/// The bound is on ENTRIES, not bytes, and the entries vary enormously: on the shipped corpus `tone=bleak`
/// is 1,599 titles and `chronology=linear` is 31,627, at 8 bytes each. 256 of the largest would be 65 MB,
/// but a handful of axis values are that broad and the ordinary row is a few hundred titles — and the whole
/// map is dropped anyway when the indexes go idle, ten minutes after the last query.
const SIMILAR_MEMO: usize = 4096;
const ROW_MEMO: usize = 256;
/// Tuned aggregates kept at most: each is a few thousand floats, and a tuner tries a handful of values.
const AGGREGATES_MEMO: usize = 8;

impl Indexes {
    /// More Like This for a title, worked out once while the indexes are loaded: it is deterministic for
    /// the dataset, and asked again and again — a billboard's seeds on every Home load, the title a search
    /// names, a detail page.
    ///
    /// The pooled scorer when the dataset ships the signals it needs, and the original otherwise. They are
    /// not small variations of each other: the original draws candidates from the premise index alone, so a
    /// plot neighbour can never enter the row — measured on The Wire, its plot top-20 and premise top-40 do
    /// not intersect at all, and Homicide: Life on the Street sits at plot rank 10 and is discarded.
    ///
    /// The seed's own type alone, as ids — the row `/index/similar`'s `ids` and `/index/suggest`'s have always
    /// carried, which every client reads. `more_like_this_mixed` is the row with the other type in it.
    pub fn more_like_this(&self, tmdb_id: u32, media_type: den_index::MediaType) -> Arc<[u32]> {
        let row = memoised(&self.similar, ((media_type, tmdb_id), false), SIMILAR_MEMO, || {
            let one_type =
                den_index::SimilarParams { mix_types: false, ..den_index::SimilarParams::default() };
            self.more_like_this_scored(tmdb_id, media_type, &one_type).iter().map(|s| s.key()).collect()
        });
        row.iter().map(|&(_, id)| id).collect()
    }

    /// More Like This mixing films and series (`SimilarParams::mix_types`), typed: the `mixed` lists of
    /// `/index/similar` and `/index/suggest`. Its titles of the seed's type are `more_like_this`'s first ones,
    /// in its order.
    pub fn more_like_this_mixed(&self, tmdb_id: u32, media_type: den_index::MediaType) -> Arc<[Key]> {
        memoised(&self.similar, ((media_type, tmdb_id), true), SIMILAR_MEMO, || {
            let production = den_index::SimilarParams::default();
            self.more_like_this_scored(tmdb_id, media_type, &production).iter().map(|s| s.key()).collect()
        })
    }

    /// More Like This ranked with `params`, every title's signals kept, and never memoised — the tuning
    /// playground's question. `more_like_this` is this with the default parameters, so the two cannot
    /// build the scorer's inputs differently.
    pub fn more_like_this_scored(
        &self,
        tmdb_id: u32,
        media_type: den_index::MediaType,
        params: &den_index::SimilarParams,
    ) -> Vec<den_index::Scored> {
        self.more_like_this_with(tmdb_id, media_type, params, den_index::Extras::default())
    }

    /// `more_like_this_scored` with a request's `Extras`: who is watching, and which candidates to consider.
    pub fn more_like_this_with(
        &self,
        tmdb_id: u32,
        media_type: den_index::MediaType,
        params: &den_index::SimilarParams,
        extras: den_index::Extras<'_>,
    ) -> Vec<den_index::Scored> {
        self.with_seed(tmdb_id, media_type, params, |authorship, facets| {
            den_index::more_like_this_with(
                Some(&self.plot),
                self.premise.as_ref(),
                tmdb_id,
                media_type,
                authorship,
                Some(facets),
                extras,
                params,
            )
        })
    }

    /// Both of `/index/similar`'s rows: `more_like_this` and `more_like_this_mixed`, from the memo where it
    /// holds them.
    ///
    /// Where it holds neither, the two are ranked over ONE scan of each index (`den_index::more_like_this_both`)
    /// instead of one scan per row: the seed type's row draws its pool from each index's nearest of that type,
    /// which the mixed row's scan finds anyway. Where it holds one, the other is ranked alone, as it would be.
    pub fn more_like_this_rows(
        &self,
        tmdb_id: u32,
        media_type: den_index::MediaType,
    ) -> (Arc<[u32]>, Arc<[Key]>) {
        let seed = (media_type, tmdb_id);
        let held = {
            let memo = lock(&self.similar);
            memo.contains_key(&(seed, false)) || memo.contains_key(&(seed, true))
        };
        if held {
            let one = self.more_like_this(tmdb_id, media_type);
            return (one, self.more_like_this_mixed(tmdb_id, media_type));
        }
        let production = den_index::SimilarParams::default();
        let keys = |row: Vec<den_index::Scored>| -> Arc<[Key]> { row.iter().map(|s| s.key()).collect() };
        let (one, mixed) = self.with_seed(tmdb_id, media_type, &production, |authorship, facets| {
            den_index::more_like_this_both(
                Some(&self.plot),
                self.premise.as_ref(),
                tmdb_id,
                media_type,
                authorship,
                Some(facets),
                den_index::Extras::default(),
                &production,
            )
        });
        let (one, mixed) = (keys(one), keys(mixed));
        let mut memo = lock(&self.similar);
        if memo.len() + 2 > SIMILAR_MEMO {
            memo.clear();
        }
        memo.insert((seed, false), Arc::clone(&one));
        memo.insert((seed, true), Arc::clone(&mixed));
        (one.iter().map(|&(_, id)| id).collect(), mixed)
    }

    /// `work` given what the rail reads about the seed beyond the vectors: its facets, tuned to `params`, and
    /// its authorship with its character links. One place, so every More Like This builds them alike.
    fn with_seed<R>(
        &self,
        tmdb_id: u32,
        media_type: den_index::MediaType,
        params: &den_index::SimilarParams,
        work: impl FnOnce(Option<&dyn den_index::Authorship>, &dyn den_index::Facets) -> R,
    ) -> R {
        let aggregates = self.aggregates_for(params);
        let aggregates = aggregates.as_deref().unwrap_or(&self.store.aggregates);
        // `LoadedStore::open` already proved this builds — `check` calls the same constructor — and
        // the load fails without a store, so there is no arm here that answers without one.
        let view = self.store.view();
        let facets = den_index::SeedFacets::new(&view, aggregates, media_type)
            .expect("MappedStore::check builds this at load, so it cannot fail per request")
            .tuned(params);
        // Without the credit lists the rail ranks without authorship. The facts read the same lists, so a
        // store missing them also reaches `/health` as `facts_unusable`.
        let characters = self.character_links(media_type, tmdb_id);
        let authorship = den_index::SeedAuthorship::of(&view, media_type, tmdb_id)
            .ok()
            .map(|authorship| authorship.with_characters(characters));
        work(authorship.as_ref().map(|a| a as &dyn den_index::Authorship), &facets)
    }

    /// The corpus aggregates for `params`' critique floor and holds share, or `None` for production's, which
    /// were built at load. Other values are built on first use — one pass over the store, ~0.1 s — and kept,
    /// a few at a time, so a tuner moving other knobs does not pay it again.
    fn aggregates_for(&self, params: &den_index::SimilarParams) -> Option<Arc<den_index::RailAggregates>> {
        let production = den_index::SimilarParams::default();
        if (params.critique_floor, params.holds) == (production.critique_floor, production.holds) {
            return None;
        }
        let key = (params.critique_floor.to_bits(), params.holds.to_bits());
        let built = memoised(&self.aggregates, key, AGGREGATES_MEMO, || {
            let built = den_index::RailAggregates::build_with(
                &self.store.view(),
                params.critique_floor,
                params.holds,
            )
            // The same pass production's aggregates passed at load, over the same store.
            .expect("the store's critique and facet sections were checked at load");
            Arc::new(built)
        });
        Some(built)
    }

    /// What a billboard's fit reads off the index as a whole (`fit::Corpus`), worked out once: the first time it is
    /// asked for, which the load does, so no billboard waits on it.
    pub fn corpus(&self) -> &Corpus {
        self.corpus.get_or_init(|| Corpus::of(self))
    }

    /// What `/index/filter/…` counts and selects over, built once: the first time it is asked for, which the
    /// load does, so no request waits on it.
    pub fn filter(&self) -> &FilterIndex {
        self.filter.get_or_init(|| FilterIndex::build(self))
    }

    /// `/index/schema.json`'s body, built the first time it is asked for and kept for as long as these indexes
    /// are: ~70 KB of counts over the whole corpus, ~150 ms on the box, and the same bytes on every call.
    ///
    /// Kept HERE, on the indexes of one dataset, because everything `schema::document` reads is fixed when
    /// they load — the counts, the facet index's snapshot of the vote counts, the filter index — and nothing
    /// live (the ratings and character holders) reaches it. A dataset swap restarts the process and a reload
    /// after an idle release builds new indexes, so a schema can never outlive the dataset it describes.
    pub fn schema_json(&self) -> &str {
        self.schema.get_or_init(|| crate::schema::document(self).to_string())
    }

    /// A title's vote count — what every browse row is ORDERED by.
    ///
    /// TMDB's `vote_count` where the ratings index names the row, else the store's own `votes` column,
    /// which older stores carry. It used to come from `facets.bin`, which fell 9,007 titles behind the corpus
    /// because nothing rebuilt it, and a title with no row there sorts by tmdbId — which is how *La Job*
    /// (tv:5) came to sit next to *Game of Thrones*. 0 only when NEITHER source has a count, which
    /// `IndexQueries::votes_unusable` reports to `/health` rather than leaving to be noticed on a screen.
    pub fn votes(&self, media_type: den_index::MediaType, tmdb_id: u32) -> u32 {
        let ratings = self.ratings.as_ref().and_then(|r| r.index());
        votes_of(&self.store, ratings.as_deref(), media_type, tmdb_id)
    }

    /// TMDB's own vote count and score for a title, where one is kept. `/recommend` reads this where it used
    /// to substitute a prior for a rating nobody supplied.
    pub fn rating(&self, media_type: den_index::MediaType, tmdb_id: u32) -> Option<(u32, f32)> {
        let ratings = self.ratings.as_ref()?.index()?;
        ratings.of(row_of(&self.store, media_type, tmdb_id)?)
    }

    /// The titles sharing a character with this one, of either type, strongest evidence first, each with how
    /// strongly the link says the two are one franchise (`CharacterLink::strength`: a shared series in the
    /// facts confirms a recast link). Empty when the store does not hold the title or no credits are kept.
    pub fn character_links(&self, media_type: den_index::MediaType, tmdb_id: u32) -> Vec<(Key, f64)> {
        let Some(index) = self.characters.as_ref().and_then(|c| c.index()) else { return Vec::new() };
        let Some(row) = row_of(&self.store, media_type, tmdb_id) else { return Vec::new() };
        let Ok(keys) = self.store.view().per_row::<u64>("keys") else { return Vec::new() };
        let series = |(media, id): Key| {
            self.facts.as_ref().and_then(|f| f.get(id, media)).map_or(&[][..], |r| r.franchise.as_slice())
        };
        let mine = series((media_type, tmdb_id));
        index
            .of(row)
            .iter()
            .filter_map(|link| {
                let packed = *keys.get(link.row as usize)?;
                let media =
                    if packed >> 32 == 1 { den_index::MediaType::Tv } else { den_index::MediaType::Movie };
                let key = (media, packed as u32);
                let shares_series = series(key).iter().any(|s| mine.contains(s));
                Some((key, link.strength(shares_series)))
            })
            .collect()
    }

    /// Whether the store's own `votes` column holds a count for any row — the fallback source for row
    /// order, and the half of `votes_unusable` that only a load can answer. Scanned rather than
    /// remembered: one pass over the corpus's `u32`s, at load only.
    pub(crate) fn store_has_votes(&self) -> bool {
        self.store.view().per_row::<u32>("votes").is_ok_and(|votes| votes.iter().any(|&v| v > 0))
    }

    /// Which source is ordering browse rows, for the load line.
    ///
    /// Stated at EVERY load, including the ordinary one. With no TMDB counts kept the order comes from the
    /// store's `votes` column where an older store has one; without either it means "ordered by nothing",
    /// and a state that is only ever visible by its absence from a log is one nobody sees.
    fn row_order_source(&self) -> String {
        let ratings = self.ratings.as_ref().and_then(|r| r.index());
        match (ratings, self.store_has_votes()) {
            (Some(index), store_votes) => format!(
                "row order: TMDB vote_count for {} of {} rows, the store's `votes` for the rest ({})",
                index.matched(),
                self.population,
                if store_votes { "which it has" } else { "which it has NOT" }
            ),
            (None, true) => "row order: the store's `votes` column — no TMDB counts kept".to_owned(),
            (None, false) => "row order: NOTHING — no TMDB counts kept and no `votes` column in the store; \
                              every browse row falls back to tmdb-id order"
                .to_owned(),
        }
    }

    /// A browse row's order (`plotrows::row`), worked out once per type and constraints: every page of a row, and
    /// every visit to a screen, asks for the same one.
    pub(crate) fn row_order(
        &self,
        key: String,
        work: impl FnOnce() -> Vec<(den_index::MediaType, u32)>,
    ) -> Arc<[(den_index::MediaType, u32)]> {
        memoised(&self.rows, key, ROW_MEMO, || work().into())
    }
}

/// A title's store row, `None` when the store does not hold it.
fn row_of(
    loaded: &crate::store::LoadedStore,
    media_type: den_index::MediaType,
    tmdb_id: u32,
) -> Option<usize> {
    let media = u8::from(media_type == den_index::MediaType::Tv);
    loaded.view().row_of(media, tmdb_id).ok().flatten().map(|row| row.0)
}

/// A title's vote count: TMDB's kept `vote_count` first, the store's `votes` column after it. Shared by
/// `Indexes::votes` and the display index's ranking, which used to read the column with two copies of the
/// same four lines.
///
/// The store's column is read with `.ok()`, so a store that no longer carries it still orders rows off
/// TMDB's kept counts rather than failing the load. That tolerance is exactly what used to make the failure silent when
/// there was only ONE source — hence `Indexes::store_has_votes` and `IndexQueries::votes_unusable`, which
/// ask once, at load, whether either source has anything at all.
fn votes_of(
    loaded: &crate::store::LoadedStore,
    ratings: Option<&RatingsIndex>,
    media_type: den_index::MediaType,
    tmdb_id: u32,
) -> u32 {
    let Some(row) = row_of(loaded, media_type, tmdb_id) else { return 0 };
    if let Some(votes) = ratings.and_then(|index| index.votes(row)) {
        return votes;
    }
    loaded.view().per_row::<u32>("votes").ok().and_then(|votes| votes.get(row).copied()).unwrap_or(0)
}

/// The facet index from the store's own columns, so attribute search covers the whole corpus.
///
/// `facets.bin` was a separate producer that fell 9,086 titles behind the corpus because nothing rebuilt
/// it, and the four facts it held — country, original language, year, vote count — are all in the store.
/// This reads them from there instead. `den-index` cannot depend on `den-store` (it must keep building for
/// wasm32 and aarch64-apple-tvos), so the index is FILLED here rather than read there.
///
/// The country and language taken are the FIRST each title lists, which is what the blob held: one code per
/// title. A title with several origins is findable by the one Wikidata lists first, exactly as it was.
///
/// The vote count is TMDB's where the ratings index names the row, the store's column otherwise — the same
/// order `votes_of` uses, so attribute search and a browse row rank on one number rather than two. A
/// SNAPSHOT of the ratings index, unlike `votes_of`: this builds a table, and the table is rebuilt on the
/// next index load. The store's column is optional here (`.unwrap_or(&[])`) so a store that has dropped it
/// still yields a facet index off TMDB's counts instead of turning every browse row empty.
fn facet_index_from(
    facts: &Facts,
    store: &den_store::Store<'_>,
    ratings: Option<&RatingsIndex>,
) -> Result<FacetIndex, den_store::StoreError> {
    let keys = store.per_row::<u64>("keys")?;
    let votes = store.per_row::<u32>("votes").unwrap_or(&[]);
    let mut index = FacetIndex::empty();
    for (i, &packed) in keys.iter().enumerate() {
        let media_type =
            if (packed >> 32) == 1 { den_index::MediaType::Tv } else { den_index::MediaType::Movie };
        let id = (packed & 0xffff_ffff) as u32;
        let Some(record) = facts.get(id, media_type) else { continue };
        // A year, not a date: the blob's unit, and the only granularity a decade bucket or a year window
        // needs. 0 is "unknown" on both sides, and `insert` treats anything under 1870 as unknown.
        let year = record.released.map(|r| r.year_of()).and_then(|y| u16::try_from(y).ok()).unwrap_or(0);
        index.insert(
            id,
            media_type,
            record.countries.first().copied().unwrap_or([0, 0]),
            record.languages.first().copied().unwrap_or([0, 0]),
            year,
            ratings.and_then(|index| index.votes(i)).unwrap_or_else(|| votes.get(i).copied().unwrap_or(0)),
        );
    }
    Ok(index)
}

/// `memo`'s value for `key`, else what `work` gives, kept. The work runs outside the lock: two requests for one key
/// may both do it, and get the same answer.
fn memoised<K: Eq + std::hash::Hash, V: ?Sized>(
    memo: &Mutex<HashMap<K, Arc<V>>>,
    key: K,
    cap: usize,
    work: impl FnOnce() -> Arc<V>,
) -> Arc<V> {
    if let Some(value) = lock(memo).get(&key) {
        return Arc::clone(value);
    }
    let value = work();
    let mut memo = lock(memo);
    if memo.len() >= cap {
        memo.clear();
    }
    memo.insert(key, Arc::clone(&value));
    value
}

pub struct IndexQueries {
    dataset_version: String,
    taxonomy_version: String,
    /// The process's one verified mapping (`Dataset::mapped`): every load reads it, none hashes it again.
    store: Arc<MappedStore>,
    loaded: Mutex<Option<(Arc<Indexes>, Instant)>>,
    /// Held while loading, so concurrent first queries wait for one load instead of each starting their own.
    loading: tokio::sync::Mutex<()>,
    /// Whether the last load couldn't read the store's facts sections (`/health`).
    facts_unusable: AtomicBool,
    /// Whether the last load failed outright: the rail could not read the store, so every index route
    /// answers 503.
    ///
    /// It used to mean something softer — "no store, so More Like This falls back to the pre-pooled
    /// scorer" — because the store was one input among several and the rest could carry a degraded
    /// service. It is now the only input, so there is no degraded service behind it: this is an outage of
    /// every query route, and `/health` says so.
    store_unusable: AtomicBool,
    /// Whether the last load ended with no facet rows. Its own flag because it is its own feature: the
    /// store can be perfectly readable and its twelve `facet_*` sections missing or mis-typed, and then
    /// every browse row (`/index/row?ending=bittersweet`) answers `{"titles":[],"total":0}` with nothing
    /// else wrong. That had no health reason at all, so a whole screen could go blank on a green addon.
    rows_unusable: AtomicBool,
    /// Whether the store's own `votes` column held a count for no row at the last load. Half of
    /// `votes_unusable`; the other half is whether TMDB counts are kept, which is live.
    store_votes_absent: AtomicBool,
    /// TMDB's counts and scores, joined onto this store's rows (`tmdb`).
    ratings: Option<Arc<Ratings>>,
    /// The character neighbour list from TMDB's credits over this store's rows (`tmdb`).
    characters: Option<Arc<Characters>>,
}

impl IndexQueries {
    pub fn new(ds: &Dataset) -> Self {
        IndexQueries {
            dataset_version: ds.meta.dataset_version.clone(),
            taxonomy_version: ds.meta.taxonomy_version.clone(),
            store: Arc::clone(&ds.mapped),
            loaded: Mutex::new(None),
            loading: tokio::sync::Mutex::new(()),
            facts_unusable: AtomicBool::new(false),
            store_unusable: AtomicBool::new(false),
            rows_unusable: AtomicBool::new(false),
            store_votes_absent: AtomicBool::new(false),
            ratings: None,
            characters: None,
        }
    }

    /// The TMDB counts these indexes order rows by. Separate from `new` because they live in `tmdb`, and
    /// every test that only wants a dataset should not have to name them.
    pub fn with_ratings(mut self, ratings: Option<Arc<Ratings>>) -> Self {
        self.ratings = ratings;
        self
    }

    /// The character neighbour list, for the same reason as `with_ratings`.
    pub fn with_characters(mut self, characters: Option<Arc<Characters>>) -> Self {
        self.characters = characters;
        self
    }

    /// Whether NEITHER vote source can order a browse row: the store's `votes` column held nothing at the
    /// last load, and no TMDB counts are kept.
    ///
    /// This is the signal the silent-zero failure never had. `votes_of` answered 0 for every row when the
    /// column could not be read, so every browse row collapsed into tmdb-id order — *La Job* beside *Game
    /// of Thrones* — while every request answered 200 and nothing was logged. It is deliberately LIVE on
    /// the ratings side: a refresh that lands clears it without waiting for an idle release. An index that
    /// exists always names at least one row, since `ratings::build` refuses one that matched nothing.
    pub fn votes_unusable(&self) -> bool {
        self.store_votes_absent.load(Ordering::Relaxed)
            && self.ratings.as_ref().and_then(|r| r.index()).is_none()
    }

    /// Whether the last index load failed on the store: every `/index/…` route then answers 503.
    pub fn store_unusable(&self) -> bool {
        self.store_unusable.load(Ordering::Relaxed)
    }

    /// Whether the last index load couldn't read the store's facts sections: `/recommend` and search
    /// then run without facts, which only a log line said before.
    pub fn facts_unusable(&self) -> bool {
        self.facts_unusable.load(Ordering::Relaxed)
    }

    /// Whether the last load ended with no facet rows: every browse row is then empty.
    pub fn rows_unusable(&self) -> bool {
        self.rows_unusable.load(Ordering::Relaxed)
    }

    /// The indexes — loaded first if they aren't in memory — and how long that load took (`None` when they
    /// already were). `on_load` runs once, as a load starts, and not when they were in memory.
    pub async fn get(&self, on_load: impl FnOnce()) -> Result<(Arc<Indexes>, Option<Duration>), String> {
        if let Some(indexes) = self.touch() {
            return Ok((indexes, None));
        }
        let _loading = self.loading.lock().await;
        if let Some(indexes) = self.touch() {
            return Ok((indexes, None));
        }
        on_load();
        let started = Instant::now();
        let sources = Sources {
            dataset_version: self.dataset_version.clone(),
            taxonomy_version: self.taxonomy_version.clone(),
            store: Arc::clone(&self.store),
            ratings: self.ratings.clone(),
            characters: self.characters.clone(),
        };
        let loaded = tokio::task::spawn_blocking(move || load(&sources))
            .await
            .map_err(|e| format!("load task: {e}"))?;
        // A failed load is now an OUTAGE of the query routes, not a degradation of one of them, because
        // the store is the only input left. Recorded before the `?` so `/health` reports it rather than
        // only the request that happened to trigger the load.
        self.store_unusable.store(loaded.is_err(), Ordering::Relaxed);
        let (indexes, phases) = loaded?;
        let took = started.elapsed();
        let count = |n: Option<usize>| n.map_or("none".to_owned(), |n| format!("{n} titles"));
        let premise = count(indexes.premise.as_ref().map(Index::len));
        let facets = count(indexes.facets.as_ref().map(FacetIndex::len));
        let facts = count(indexes.facts.as_ref().map(Facts::len));
        let plot_facets = count(indexes.plot_facets.as_ref().map(PlotFacets::len));
        eprintln!(
            "index loaded: {} of {} store rows, premise {premise}, facets {facets}, facts {facts}, plot facets {plot_facets}, in {:.1}s ({phases})",
            indexes.plot.len(),
            indexes.population,
            took.as_secs_f64()
        );
        // The facts are sections of the store, so a store that opened is a promise of them: there is no
        // longer a "the dataset declares no facts file" case in which their absence is by design. A store
        // whose facts sections are missing or the wrong width leaves `/recommend`, `imdbId` and people
        // search off while every request answers 200 — the nineteen-minute silent degradation this flag
        // exists to make impossible.
        self.facts_unusable.store(indexes.facts.is_none(), Ordering::Relaxed);
        // Same reasoning for the twelve `facet_*` sections and the browse rows they fill.
        self.rows_unusable.store(indexes.plot_facets.is_none(), Ordering::Relaxed);
        // And for the vote counts every browse row is ordered by, which had no signal at all: a store
        // whose `votes` column will not read ordered every row by nothing and said so nowhere.
        self.store_votes_absent.store(!indexes.store_has_votes(), Ordering::Relaxed);
        let indexes = Arc::new(indexes);
        *lock(&self.loaded) = Some((Arc::clone(&indexes), Instant::now()));
        Ok((indexes, Some(took)))
    }

    fn touch(&self) -> Option<Arc<Indexes>> {
        let mut slot = lock(&self.loaded);
        let (indexes, last_used) = slot.as_mut()?;
        *last_used = Instant::now();
        Some(Arc::clone(indexes))
    }

    /// Drop the indexes if nobody has queried them for `IDLE_RELEASE`; `true` when it did. A query already
    /// running holds its own `Arc`, so a release never pulls an index out from under one.
    fn release_if_idle(&self) -> bool {
        let mut slot = lock(&self.loaded);
        if slot.as_ref().is_some_and(|(_, used)| used.elapsed() >= IDLE_RELEASE) {
            *slot = None;
            true
        } else {
            false
        }
    }
}

/// Look for an idle index once a minute, for as long as atlas runs.
pub async fn release_when_idle(queries: Arc<IndexQueries>) {
    loop {
        tokio::time::sleep(SWEEP_EVERY).await;
        if queries.release_if_idle() {
            eprintln!("index released after {} idle minutes", IDLE_RELEASE.as_secs() / 60);
        }
    }
}

/// Where the indexes are read from: one file, and the manifest field that is not in it.
struct Sources {
    /// The generation, as the manifest names it. Not read from the store even though the store stamps its
    /// own — `den-atlas check` compares the two to catch a mixed generation, and a value that agreed with
    /// itself by construction could not.
    dataset_version: String,
    /// The labelling pass's version. It is NOT in the store — the store is the corpus, and the taxonomy
    /// is a property of the pass that labelled it — so both indexes are stamped with it here.
    taxonomy_version: String,
    store: Arc<MappedStore>,
    /// TMDB's kept counts and scores (`tmdb`). The indexes keep the holder — not the index it currently
    /// has — so a rebuild that lands between two loads reaches the rows in between.
    ratings: Option<Arc<Ratings>>,
    /// The character neighbour list from TMDB's credits; the holder, like `ratings`.
    characters: Option<Arc<Characters>>,
}

/// Load the indexes ONCE, synchronously, for a command-line tool.
///
/// The same `load` serving uses, so a tool measures the corpus as atlas reads it rather than as the tool
/// re-reads it — which is the mistake `rail-ab` was built on for its whole life: it parsed its own facts
/// and its own facet sidecar out of files serving does not open, and every number it produced was about
/// that parse. Nothing here is cached or released; the process exits when it is done.
pub fn load_for_tools(ds: &Dataset) -> Result<Indexes, String> {
    let sources = Sources {
        dataset_version: ds.meta.dataset_version.clone(),
        taxonomy_version: ds.meta.taxonomy_version.clone(),
        store: Arc::clone(&ds.mapped),
        // No ratings fetch for a one-shot tool: it would download 8 MB to rank the run it then exits
        // from. A tool measures row order off the store's own `votes` column, and says so here.
        ratings: None,
        characters: None,
    };
    load(&sources).map(|(indexes, _phases)| indexes)
}

/// `work`, and how long it took.
fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let started = std::time::Instant::now();
    let value = work();
    (value, started.elapsed())
}

/// A load thread's answer; a panic in one is re-raised here, as it would have been had the part loaded inline.
fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The indexes, and how long each part took.
///
/// The store's rail aggregates are built FIRST and a failure ends the load: it is the only input, so there
/// is nothing to build the other parts out of and nothing to serve if the rail cannot read it. The mapping
/// itself was verified once, when the process loaded the dataset. Everything after it is a read of a
/// section of that one mapping, so the parallel phase is what can genuinely run side by side — both vector
/// indexes and the cards — while the facts, the facet rows, the facet index and the display index follow in
/// the order they depend on each other.
fn load(sources: &Sources) -> Result<(Indexes, String), String> {
    // Name the file and the reason. "could not load the dataset" is the message that cost nineteen
    // minutes of quiet degradation the last time a blob went bad.
    let (store, store_took) = timed(|| {
        crate::store::LoadedStore::of(Arc::clone(&sources.store))
            .map_err(|e| format!("store {} is unusable: {e}", sources.store.path().display()))
    });
    let store = store?;
    let population = store.store.rows();
    // One snapshot of the ratings index for everything this load BUILDS out of it — the facet index and
    // the display index's ranking — so the two cannot disagree about a title's vote count within a load.
    // `Indexes::votes` reads the live holder instead; a table is rebuilt at the next load, a lookup is not.
    let ratings = sources.ratings.as_ref().and_then(|r| r.index());

    let (plot, premise, cards) = std::thread::scope(|scope| {
        let view = || store.view();
        // Both indexes read their vectors in place, in the mapping, rather than copying ~95 MB of them.
        let mapped = || Arc::clone(&store.store) as Arc<dyn den_index::StoreBytes>;
        let plot = scope.spawn(move || timed(|| Index::from_store_plot(&view(), mapped())));
        // A premise index that will not read costs premise-led More Like This, not the whole feature.
        let premise = scope.spawn(move || {
            timed(|| {
                Index::from_store_premise(&view(), mapped())
                    .map_err(|e| eprintln!("premise index unusable ({e}) — More Like This is plot-only"))
                    .ok()
            })
        });
        let cards = scope.spawn(move || {
            timed(|| {
                cards_from_store(&view())
                    .map_err(|e| {
                        eprintln!("cards unusable ({e}) — plot rows are empty, search has no display titles")
                    })
                    .ok()
            })
        });
        (joined(plot), joined(premise), joined(cards))
    });
    let ((plot, plot_took), (premise, premise_took), (cards, cards_took)) = (plot, premise, cards);
    // The taxonomy version is a property of the LABELLING PASS, not of the corpus, so the store does not
    // carry it and both indexes are stamped from the manifest. Miss this and `/index/schema.json`,
    // `/index/taxonomy.json` and the `atlas_dataset_info` metric all report an empty version.
    let plot = plot.map_err(|e| e.to_string())?.with_taxonomy_version(&sources.taxonomy_version);
    if !plot.has_length_direction() {
        eprintln!(
            "plot vectors are {} dimensions, not the length direction's 1024 — plot_length_off does nothing",
            plot.dimension()
        );
    }
    let premise = premise.map(|index| index.with_taxonomy_version(&sources.taxonomy_version));
    // `factsFile` was a 43 MB JSON blob that atlas alone read — nothing served it and no client fetched
    // it — and parsing it was 1.04 s of a 1.6 s load, against 0.38 s off the store.
    //
    // Switched over only once `Facts::from_store` answered IDENTICALLY to the JSON reader on the real
    // corpus: 0 of 47,618 records differ. Getting there found four real losses in the store, three of
    // which would have changed what people see — series genres kept as TMDB composites (which dropped
    // Horror from Chilling Adventures of Sabrina), genres sorted out of the genreMap's order, the
    // franchise interned against a table that holds almost no franchises, and 1,236 entity references
    // the table did not describe being dropped. The test that found them is `facts::tests::
    // the_store_answers_what_the_json_did`, and it is opt-in because it needs the real artifacts.
    let (mut facts, facts_took) = timed(|| {
        Facts::from_store(&store.view())
            .map_err(|e| eprintln!("facts unusable ({e}) — /recommend and search run without them"))
            .ok()
    });
    let (series, series_took) = timed(|| {
        facts
            .as_ref()
            .map_or_else(Default::default, |facts| crate::series::SeriesStrength::from_facts(&plot, facts))
    });
    // The facet rows used to come from `plotFacetsFile`, a 5,336-title sidecar frozen at a dead
    // datasetVersion; the store answers the same axes for all 47,618 titles, and three more besides.
    let (plot_facets, plot_facets_took) = timed(|| {
        PlotFacets::from_store(&store.view())
            .map_err(|e| eprintln!("facet rows unusable ({e}) — every browse row is empty"))
            .ok()
    });
    // The facet index needs the facts, so it follows them: `facets.bin` covered 38,532 titles where the
    // store covers 47,618, so attribute search ("spanish series", "80s korean horror") was asking a table
    // 9,086 titles behind the corpus it was searching.
    let (facets, facets_took) = timed(|| {
        let facts = facts.as_ref()?;
        facet_index_from(facts, &store.view(), ratings.as_deref())
            .map_err(|e| eprintln!("facet index unusable ({e}) — attribute search is off"))
            .ok()
    });

    // Optional sections: a store without them has no iconic studios, and says nothing. Present but malformed
    // costs the studio links and rows, not the load.
    let studios = crate::studios::Studios::from_store(&store.view()).unwrap_or_else(|e| {
        eprintln!("iconic studios unusable ({e}) — no studio links, rows or list");
        crate::studios::Studios::default()
    });

    // The facts hand their titles' other names to the display index, which is then the only one holding them.
    let (display, display_took) = timed(|| {
        let other_names = facts.as_mut().map(Facts::take_titles).unwrap_or_default();
        cards.as_ref().map(|cards| {
            let votes = |kind, id| f64::from(votes_of(&store, ratings.as_deref(), kind, id));
            // Each title under its display name, and every other name the facts give it: its original title and
            // aliases ("기생충", "Gisaengchung").
            TitleIndex::build(
                cards
                    .iter()
                    .flat_map(|(&(kind, id), card)| {
                        let also = other_names.get(&(kind, id)).map_or(&[][..], Vec::as_slice);
                        let names = std::iter::once(card.title.as_str())
                            .chain(also.iter().map(|name| &**name).filter(|name| *name != card.title));
                        names.map(move |title| TitleRecord {
                            tmdb_id: id,
                            media_type: match kind {
                                den_index::MediaType::Movie => den_titlesearch::MediaType::Movie,
                                den_index::MediaType::Tv => den_titlesearch::MediaType::Tv,
                            },
                            title: title.to_owned(),
                            popularity: votes(kind, id),
                        })
                    })
                    .collect(),
            )
        })
    });
    let seconds = |took: Duration| format!("{:.2}s", took.as_secs_f64());
    let phases = format!(
        "store {}, plot {}, premise {}, cards {}, facts {}, series {} ({}), facet rows {}, facets {}, display {}",
        seconds(store_took),
        seconds(plot_took),
        seconds(premise_took),
        seconds(cards_took),
        seconds(facts_took),
        seconds(series_took),
        series.len(),
        seconds(plot_facets_took),
        seconds(facets_took),
        seconds(display_took)
    );
    let indexes = Indexes {
        population,
        dataset_version: sources.dataset_version.clone(),
        plot,
        premise,
        facets,
        facts,
        series,
        plot_facets,
        store,
        ratings: sources.ratings.clone(),
        characters: sources.characters.clone(),
        cards,
        studios,
        display,
        similar: Mutex::new(HashMap::new()),
        rows: Mutex::new(HashMap::new()),
        corpus: OnceLock::new(),
        aggregates: Mutex::new(HashMap::new()),
        filter: OnceLock::new(),
        schema: OnceLock::new(),
    };
    eprintln!("{}", indexes.row_order_source());
    let (_, fit_took) = timed(|| {
        indexes.corpus();
    });
    let (filter_mb, filter_took) = timed(|| indexes.filter().bytes() as f64 / 1_000_000.0);
    Ok((
        indexes,
        format!("{phases}, fit {}, filters {} ({filter_mb:.1} MB)", seconds(fit_took), seconds(filter_took)),
    ))
}

/// A small, real dataset — twelve titles in one store — written to `dir` and loaded, for route tests.
/// Heist is the biggest subgenre; "Campy/Cult" has a slash to encode.
///
/// It used to be seven files: two labels blobs, two vector blobs, `facets.bin`, a facts sidecar, a
/// plot-facets sidecar and a metadata sidecar, each covering a different subset of the same twelve titles.
/// The corpus is unchanged — same ids, labels, vectors, cards, votes, facets and credits — but it is now
/// one artifact, which is why the facts describe every title the facet index needs rather than four of
/// them living in a separate blob.
#[cfg(test)]
pub fn write_fixture(dir: &std::path::Path) -> Dataset {
    write_fixture_as(dir, "One", true, true)
}

/// The same fixture with every `votes` count zeroed, for the tests about a corpus no source can order:
/// what the store will look like once the producer stops writing the column.
#[cfg(test)]
pub fn write_fixture_voteless(dir: &std::path::Path) -> Dataset {
    write_fixture_as(dir, "One", true, false)
}

/// The same fixture with a different display title for movie 1, for the tests about a word that is both a
/// facet and a title ("brazil").
#[cfg(test)]
pub fn write_fixture_titled(dir: &std::path::Path, movie_one: &str) -> Dataset {
    write_fixture_as(dir, movie_one, true, true)
}

/// The same fixture with no premise vectors, for the tests about a dataset whose store carries only the
/// plot space. Setting `premise_labels = None` on the dataset used to do this; the premise index is a
/// section of the store now, so the store is what has to lack it.
#[cfg(test)]
pub fn write_fixture_plot_only(dir: &std::path::Path) -> Dataset {
    write_fixture_as(dir, "One", false, true)
}

#[cfg(test)]
fn write_fixture_as(dir: &std::path::Path, movie_one: &str, premise: bool, votes: bool) -> Dataset {
    use crate::store::fixture::{Entity, Title};
    // Days since 1970-01-01, the store's unit for a release date.
    const D1985: i32 = 5479;
    const D1995: i32 = 9131;
    const D2010: i32 = 14610;
    const D2026_09_01: i32 = 20697;

    let vector = |v: [i8; 3], on: bool| if on { v.to_vec() } else { Vec::new() };
    // Movies 1-3 and series 4 are the drawable titles. Eight zero-vector series make the fixture's
    // semantic-score distribution large enough for one clear movie match to cross search.rs's z=2.5 floor;
    // they deliberately have no card, so a route test can prove which of the four drawable titles each
    // semantic index proposes without expanding every other fixture.
    let mut titles = vec![
        Title {
            media: 0,
            tmdb_id: 1,
            primary_genre: "Drama",
            subgenres: vec![("Heist", 90)],
            moods: vec![("Tense", 80)],
            plot: vector([100, 0, 0], true),
            premise: vector([100, 0, 0], premise),
            facets: vec![("ending", "bittersweet", 70), ("tone", "bleak", 90), ("pacing", "slow-burn", 90)],
            card: Some((movie_one, Some("/1.jpg"), Some(1985))),
            votes: 100,
            imdb: Some("tt0000001"),
            released: Some((D2026_09_01, 0)),
            genres: vec![80, 18],
            countries: vec!["KR", "DK"],
            makers: vec![1, 9],
            cast: vec![2, 3],
            franchise: vec![50, 51],
            based_kind: vec!["book", "play"],
            alias_titles: vec!["One", "하나", "Uno"],
            runtime: 95,
            companies: vec![60],
            locations: vec![80],
            subjects: vec![70],
            instance_of: vec![90],
            technique: vec![("live_action", 95)],
            audience: vec![("made_for_adults", 80)],
            depicts: vec![("violence", 85)],
            ..Title::default()
        },
        Title {
            media: 0,
            tmdb_id: 2,
            primary_genre: "Drama",
            subgenres: vec![("Heist", 80)],
            plot: vector([90, 10, 0], true),
            premise: vector([0, 100, 0], premise),
            facets: vec![("ending", "bittersweet", 90), ("tone", "bleak", 60)],
            card: Some(("Two", Some("/2.jpg"), Some(1995))),
            votes: 500,
            released: Some((D1995, 0)),
            countries: vec!["KR"],
            runtime: 130,
            instance_of: vec![90],
            depicts: vec![("violence", 20)],
            ..Title::default()
        },
        Title {
            media: 0,
            tmdb_id: 3,
            primary_genre: "Comedy",
            subgenres: vec![("Heist", 60), ("Campy/Cult", 90)],
            plot: vector([0, 100, 0], true),
            premise: vector([95, 0, 0], premise),
            facets: vec![("ending", "bittersweet", 90), ("tone", "comic", 90)],
            card: Some(("Three", None, Some(1985))),
            votes: 50,
            released: Some((D1985, 0)),
            countries: vec!["ES"],
            runtime: 160,
            audience: vec![("made_for_children", 90)],
            ..Title::default()
        },
        Title {
            media: 1,
            tmdb_id: 4,
            primary_genre: "Drama",
            subgenres: vec![("Heist", 95)],
            plot: vector([100, 0, 0], true),
            premise: vector([100, 0, 0], premise),
            facets: vec![("ending", "bittersweet", 90)],
            card: Some(("Four", Some("/4.jpg"), Some(2010))),
            votes: 300,
            released: Some((D2010, 0)),
            countries: vec!["KR"],
            ..Title::default()
        },
    ];
    titles.extend((101..=108).map(|tmdb_id| Title {
        media: 1,
        tmdb_id,
        primary_genre: "Drama",
        plot: vector([0, 0, 0], true),
        premise: vector([0, 0, 0], premise),
        ..Title::default()
    }));
    if !votes {
        for title in &mut titles {
            title.votes = 0;
        }
    }
    let entities = [
        Entity { qid: 1, name: "A Director", tmdb: Some(11), ..Entity::default() },
        // People search indexes the aliases as well as the name.
        Entity {
            qid: 2,
            name: "Lead Actor",
            aliases: vec!["Bong Joon-ho", "기생충 배우"],
            ..Entity::default()
        },
        Entity { qid: 50, name: "A Franchise", ..Entity::default() },
        Entity { qid: 60, name: "A Studio", ..Entity::default() },
        Entity { qid: 70, name: "Revenge", ..Entity::default() },
        Entity { qid: 80, name: "Seoul", ..Entity::default() },
        Entity { qid: 90, name: "feature film", ..Entity::default() },
    ];

    std::fs::create_dir_all(dir).unwrap();
    crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &titles, &entities);
    // The pruned manifest: the store, and the few facts about it that are not in it.
    let meta = serde_json::json!({
        "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
        "quantization": "int8", "storeFile": "den-v1.store",
    });
    std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
    Dataset::load(dir).expect("fixture dataset must load")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_idle_index_is_released_and_reloads_on_the_next_query() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-{}", std::process::id()));
        let queries = IndexQueries::new(&write_fixture(&dir));

        let loads = std::cell::Cell::new(0);
        let counted = || loads.set(loads.get() + 1);
        let (indexes, first) = queries.get(counted).await.unwrap();
        assert!(first.is_some(), "the first query loads");
        assert!(indexes.premise.is_some());
        // One facts record per store row: the facts are sections of the corpus now, not a sidecar that
        // happened to describe some of it.
        assert_eq!(indexes.facts.as_ref().map(|f| f.len()), Some(12), "the facts load with the indexes");
        assert_eq!(indexes.plot.taxonomy_version(), "t02", "stamped from the manifest, not the store");
        assert_eq!(indexes.premise.as_ref().map(Index::taxonomy_version), Some("t02"));
        let (_, again) = queries.get(counted).await.unwrap();
        assert!(again.is_none(), "a warm query doesn't");
        assert_eq!(loads.get(), 1, "on_load runs for the load alone");

        tokio::time::advance(IDLE_RELEASE - Duration::from_secs(1)).await;
        assert!(!queries.release_if_idle(), "released before it was idle");
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(queries.release_if_idle());

        // A query that held the index across the release still has it; the next query reloads.
        assert_eq!(indexes.plot.len(), 12);
        let (_, reload) = queries.get(counted).await.unwrap();
        assert!(reload.is_some(), "the query after a release loads again");
        assert_eq!(loads.get(), 2);
    }

    /// A store that verifies but that the rail cannot read — here, one without its `world` column — fails
    /// the load and SAYS SO through `/health`, rather than panicking or answering a degraded row. It is the
    /// only input, so there is nothing else to serve from — which is the difference from every optional
    /// blob this used to fall back through.
    #[tokio::test]
    async fn a_store_that_does_not_read_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let title =
            crate::store::fixture::Title { media: 0, tmdb_id: 1, plot: vec![1, 0, 0], ..Default::default() };
        crate::store::fixture::write_omitting(&dir.join("den-v1.store"), "v1", 3, &[title], &[], &["world"]);
        let meta = serde_json::json!({
            "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
            "quantization": "int8", "storeFile": "den-v1.store",
        });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = Dataset::load(&dir).expect("the store itself verifies");
        let queries = IndexQueries::new(&ds);
        assert!(queries.get(|| ()).await.is_err());
        assert!(queries.store_unusable(), "a failed load must reach /health");
    }

    /// Every load reads the mapping the process verified when it loaded the dataset, never the path again:
    /// a file renamed over the store afterwards — what `atlas-dataset-sync` does just before it restarts the
    /// process — is not read, so a reload in that window serves the generation its manifest describes.
    #[tokio::test]
    async fn a_load_reads_the_verified_mapping_not_the_path() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-renamed-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let other = dir.join("other.store");
        std::fs::write(&other, b"not a store").unwrap();
        std::fs::rename(&other, &ds.store).unwrap();
        let queries = IndexQueries::new(&ds);
        let (indexes, _) = queries.get(|| ()).await.expect("the verified mapping still loads");
        assert_eq!(indexes.plot.len(), 12);
        assert!(!queries.store_unusable());
    }

    /// TMDB's kept counts, joined onto a real store. Only movie 1 has one kept, and every other row must
    /// still be ordered by the store's own column.
    #[tokio::test]
    async fn tmdb_vote_counts_win_and_the_store_s_column_is_the_fallback() {
        use den_index::MediaType::{Movie, Tv};
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-tmdb-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let queries = IndexQueries::new(&ds).with_ratings(Some(Arc::new(Ratings::with_index(kept(&ds)))));
        let (indexes, _) = queries.get(|| ()).await.unwrap();

        assert_eq!(indexes.votes(Movie, 1), 9000, "TMDB's kept vote_count, not the store's 100");
        assert_eq!(indexes.votes(Movie, 2), 500, "nothing kept for this row: the store's column");
        assert_eq!(indexes.votes(Tv, 4), 300);
        assert_eq!(indexes.votes(Movie, 999), 0, "a title the store does not hold");
        assert_eq!(indexes.rating(Movie, 1), Some((9000, 8.4)));
        assert_eq!(indexes.rating(Movie, 2), None, "no score where nothing is kept");

        // Attribute search ranks on the same number a browse row does, so the facet index takes the join
        // too rather than reading the store's column on its own.
        let facets = indexes.facets.as_ref().expect("the fixture builds a facet index");
        assert_eq!(facets.title(1, Movie).map(|t| t.votes), Some(9000));
        assert_eq!(facets.title(2, Movie).map(|t| t.votes), Some(500));
        assert!(!queries.votes_unusable(), "both sources have counts");
    }

    /// The silent-zero failure, as `/health` now sees it: a store with no usable `votes` column and no
    /// TMDB counts kept orders every browse row by tmdb id, and must SAY so. The same store with counts
    /// kept is ordered again — so the flag is live on the ratings side, not frozen at load.
    #[tokio::test]
    async fn no_vote_counts_from_either_source_is_reported() {
        use den_index::MediaType::Movie;
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-novotes-{}", std::process::id()));
        let ds = write_fixture_voteless(&dir);

        let queries = IndexQueries::new(&ds);
        assert!(!queries.votes_unusable(), "nothing is claimed before a load");
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        assert!(!indexes.store_has_votes());
        assert_eq!(indexes.votes(Movie, 1), 0);
        assert!(queries.votes_unusable(), "no source at all must reach /health");

        let queries = IndexQueries::new(&ds).with_ratings(Some(Arc::new(Ratings::with_index(kept(&ds)))));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        assert_eq!(indexes.votes(Movie, 1), 9000, "TMDB's counts alone order the rows they name");
        assert!(!queries.votes_unusable(), "one source is enough");
    }

    /// `/recommend` used to rate a title no upstream list had scored with `RATING_PRIOR` — the same 6.6
    /// for every one of them. TMDB's kept numbers give a real score for 99.9% of the corpus, and a real
    /// count to stand it on, so neither number is a guess any more and `estimated_votes` stays truthful.
    #[tokio::test]
    async fn recommend_rates_a_title_no_list_scored_with_tmdb_s_own_score() {
        use den_index::MediaType::Movie;
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-rate-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let queries = IndexQueries::new(&ds).with_ratings(Some(Arc::new(Ratings::with_index(kept(&ds)))));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let known = crate::recommend::Knowledge { indexes: &indexes };

        let title = known.title((Movie, 1), None, None);
        assert_eq!(title.rating, Some(f64::from(8.4f32)), "TMDB's score, where there was none");
        assert_eq!(title.votes, Some(9000.0));
        assert!(!title.estimated_votes, "a real count is not an estimate");
        assert!(crate::recommend::quality(&title) > 0.3, "a real 8.4 beats the 6.6 prior's 0.3");

        // A title nothing is kept for is unchanged: no score, and `quality` still reads the prior.
        let title = known.title((Movie, 2), None, None);
        assert_eq!((title.rating, title.votes, title.estimated_votes), (None, None, false));
        assert!((crate::recommend::quality(&title) - 0.3).abs() < 1e-9, "still the prior's 0.3");

        // And where JustWatch does supply an IMDb score, the count under it is TMDB's kept one rather than
        // the store's — the facet index carries the same join.
        let listed =
            crate::recommend::Listed { key: (Movie, 1), imdb_id: None, rating: Some(7.0), year: None };
        let title = known.title(listed.key, None, Some(&listed));
        assert_eq!((title.rating, title.votes, title.estimated_votes), (Some(7.0), Some(9000.0), false));
    }

    /// A title in two series ranks on both, in the store's order.
    #[tokio::test]
    async fn recommend_ranks_on_every_series() {
        use den_index::MediaType::Movie;
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-franchise-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let (indexes, _) = IndexQueries::new(&ds).get(|| ()).await.unwrap();
        let record = indexes.facts.as_ref().and_then(|facts| facts.get(1, Movie)).expect("movie 1's facts");
        assert_eq!(record.franchise, vec![50, 51], "the store's list, in order");
        let known = crate::recommend::Knowledge { indexes: &indexes };
        assert_eq!(known.title((Movie, 1), None, None).franchise, [50, 51]);
        assert!(known.title((Movie, 2), None, None).franchise.is_empty(), "no series is none");
    }

    /// `/recommend` read popularity from client hints alone, so every title from atlas's own lists scored no buzz.
    /// TMDB's export fills it where it holds the title, TMDB's kept count (capped, and marked) where only that does,
    /// and a hint is never overwritten.
    #[tokio::test]
    async fn recommend_reads_popularity_from_the_export_then_the_vote_count() {
        use crate::recommend::{attend, Candidate, Title};
        use den_index::MediaType::Movie;
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-attend-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let queries = IndexQueries::new(&ds).with_ratings(Some(Arc::new(Ratings::with_index(kept(&ds)))));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let export = TitleIndex::build(vec![TitleRecord {
            tmdb_id: 2,
            media_type: den_titlesearch::MediaType::Movie,
            title: "Two".to_owned(),
            popularity: 42.0,
        }]);
        let attended = |id, popularity| {
            let title = Title { popularity, ..Title::default() };
            let mut candidate = Candidate { key: (Movie, id), title, rank: None, arrival: None };
            attend(&indexes, Some(&export), &mut candidate);
            (candidate.title.popularity, candidate.title.popularity_from_votes)
        };
        assert_eq!(attended(2, None), (Some(42.0), false), "the export's own score");
        let capped = crate::search::POPULAR_VOTES / crate::search::VOTES_PER_POPULARITY;
        assert_eq!(attended(1, None), (Some(capped), true), "9,000 votes, capped at fully popular");
        assert_eq!(attended(2, Some(7.0)), (Some(7.0), false), "a hint stands");
        assert_eq!(attended(3, None), (None, false), "neither source knows it");
    }

    /// A title's character neighbours through the indexes: read by its store row (movie 1 is row 0) and
    /// named by key, empty for a title the store lacks and when there is no list. A recast link counts
    /// half; the same link between titles sharing a series counts in full.
    #[tokio::test]
    async fn character_links_are_read_by_store_row() {
        use crate::characters::{Characters, RECAST};
        use den_index::MediaType::Movie;
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-chars-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let credits = || crate::tmdb::Credits {
            fetched: 0,
            roles: vec![crate::tmdb::Role { order: 0, person: 100, character: "Walter White".into() }],
        };
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        let list = crate::characters::build(
            &mapped.view(),
            &HashMap::from([((0, 1), credits()), ((0, 2), credits())]),
        )
        .unwrap();
        let queries = IndexQueries::new(&ds).with_characters(Some(Arc::new(Characters::with_index(list))));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        assert_eq!(indexes.character_links(Movie, 1), [((Movie, 2), 1.0)], "one actor: in full");
        assert!(indexes.character_links(Movie, 999).is_empty(), "a title the store does not hold");

        let (bare, _) = IndexQueries::new(&ds).get(|| ()).await.unwrap();
        assert!(bare.character_links(Movie, 1).is_empty(), "no list, no links");

        // Two shared names, recast: movie 2 shares no series with movie 1.
        let recast = |person: u32| crate::tmdb::Credits {
            fetched: 0,
            roles: vec![
                crate::tmdb::Role { order: 0, person, character: "Walter White".into() },
                crate::tmdb::Role { order: 1, person: person + 1, character: "Jesse Pinkman".into() },
            ],
        };
        let list = crate::characters::build(
            &mapped.view(),
            &HashMap::from([((0, 1), recast(100)), ((0, 2), recast(200))]),
        )
        .unwrap();
        let queries = IndexQueries::new(&ds).with_characters(Some(Arc::new(Characters::with_index(list))));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        assert_eq!(indexes.character_links(Movie, 1), [((Movie, 2), RECAST)]);
    }

    /// The rail's authorship is read from the store's credit lists (`den_index::SeedAuthorship`); it used
    /// to be read from the facts, which are built from the same lists. This holds the two to one answer on
    /// the real corpus: for every 100th title, the siblings it nominates are exactly the titles whose facts
    /// credit one of its makers, and every share it gives a sibling or one of its plot neighbours is the
    /// share the facts give.
    ///
    /// Opt-in, like every test that needs the real corpus: `DEN_STORE` names a store whose directory holds
    /// its `dataset.meta.json`.
    #[test]
    fn authorship_from_the_credit_lists_answers_what_the_facts_did() {
        use den_index::Authorship as _;
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to exercise this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let indexes = load_for_tools(&Dataset::load(dir).expect("the dataset loads")).expect("indexes");
        let facts = indexes.facts.as_ref().expect("the real store carries facts");
        let view = indexes.store.view();
        let share = |mine: &[u32], theirs: &[u32]| -> f64 {
            if mine.is_empty() || theirs.is_empty() {
                return 0.0;
            }
            mine.iter().filter(|m| theirs.contains(m)).count() as f64 / mine.len() as f64
        };
        let mut records: Vec<(den_index::MediaType, u32, &crate::facts::Record)> =
            facts.keys().map(|(m, id)| (m, id, facts.get(id, m).expect("a key the facts listed"))).collect();
        records.sort_unstable_by_key(|&(m, id, _)| (m, id));
        let (mut seeds, mut shares, mut nominated) = (0, 0, 0);
        for &(media, id, seed) in records.iter().step_by(100) {
            let columns = den_index::SeedAuthorship::of(&view, media, id).expect("the credit lists read");
            // Siblings of either type: a mixed row nominates both.
            let siblings: Vec<Key> = records
                .iter()
                .filter(|&&(_, _, r)| r.makers.iter().any(|q| seed.makers.contains(q)))
                .map(|&(m, other, _)| (m, other))
                .collect();
            assert_eq!(columns.nominate(), siblings, "{media:?} {id}: nominations");
            let neighbours =
                indexes.plot.nearest(id, media, 50).into_iter().map(|n| (n.media_type, n.tmdb_id));
            for other in siblings.iter().copied().chain(neighbours) {
                let theirs = facts.get(other.1, other.0).cloned().unwrap_or_default();
                assert_eq!(
                    columns.makers(other),
                    share(&seed.makers, &theirs.makers),
                    "{id} -> {other:?}: makers"
                );
                assert_eq!(
                    columns.home(other),
                    share(&seed.broadcasters, &theirs.broadcasters),
                    "{id} -> {other:?}: home"
                );
                shares += 2;
            }
            nominated += siblings.len();
            seeds += 1;
        }
        eprintln!("{seeds} seeds, {nominated} nominations and {shares} shares agree with the facts");
        assert!(seeds > 400, "a real corpus, got {seeds} seeds");
    }

    /// `/index/schema.json` is built once per loaded dataset: the second call is the first call's bytes, not
    /// a rebuild of them, and those bytes are exactly what the document serialises to.
    #[tokio::test]
    async fn the_schema_is_built_once_per_load() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-schema-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let (indexes, _) = IndexQueries::new(&ds).get(|| ()).await.unwrap();
        let first = indexes.schema_json();
        let second = indexes.schema_json();
        assert!(std::ptr::eq(first, second), "the second call rebuilt the schema");
        assert_eq!(first, crate::schema::document(&indexes).to_string());
        assert!(first.contains(r#""datasetVersion":"v1""#), "{first}");

        // A new load — which is what a dataset swap or an idle release ends in — builds its own.
        let (reloaded, _) = IndexQueries::new(&ds).get(|| ()).await.unwrap();
        assert!(!std::ptr::eq(first, reloaded.schema_json()), "a new load kept the old schema");
        assert_eq!(first, reloaded.schema_json());
    }

    /// `/index/similar`'s two rows ranked over one scan of each index are the rows ranked apart, over the fixture
    /// and — with `DEN_STORE` naming a store beside its `dataset.meta.json` — every 40th title of the real
    /// corpus. Each seed on fresh indexes' memo would be one load per seed, so the rows ranked apart are
    /// asked of `more_like_this_scored`, which is never memoised.
    #[tokio::test]
    async fn both_rows_over_one_scan_are_the_rows_ranked_apart() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-both-{}", std::process::id()));
        let fixture = write_fixture(&dir);
        let real = std::env::var("DEN_STORE").ok().map(|store| {
            let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
            Dataset::load(dir).expect("the dataset loads")
        });
        for (ds, every) in std::iter::once((&fixture, 1)).chain(real.as_ref().map(|ds| (ds, 40))) {
            let (indexes, _) = IndexQueries::new(ds).get(|| ()).await.expect("indexes");
            let apart = |id, media, mix| -> Vec<Key> {
                let p = den_index::SimilarParams { mix_types: mix, ..den_index::SimilarParams::default() };
                indexes.more_like_this_scored(id, media, &p).iter().map(|s| s.key()).collect()
            };
            let mut seeds = 0;
            for (media, id) in indexes.plot.titles().step_by(every) {
                let (one, mixed) = indexes.more_like_this_rows(id, media);
                let ids: Vec<u32> = apart(id, media, false).iter().map(|&(_, id)| id).collect();
                assert_eq!(&*one, ids.as_slice(), "{media:?} {id}: the seed type's row");
                assert_eq!(&*mixed, apart(id, media, true).as_slice(), "{media:?} {id}: the mixed row");
                // And from the memo, as the next request for the title gets them.
                assert_eq!(indexes.more_like_this_rows(id, media), (one, mixed));
                seeds += 1;
            }
            eprintln!("{seeds} seeds: both rows over one scan are the rows ranked apart");
        }
    }

    /// An index reads its vectors in place, in the bytes it was handed, and answers what an index over a copy
    /// of those bytes answers. A store view over one copy handed another copy's bytes is refused: its rows
    /// would be read from somewhere the view never checked.
    #[test]
    fn an_index_reads_its_vectors_from_the_bytes_its_store_was_opened_over() {
        use den_index::MediaType::{Movie, Tv};
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-inplace-{}", std::process::id()));
        let ds = write_fixture(&dir);
        let mapped = Index::from_store_plot(&ds.mapped.view(), ds.mapped.clone()).expect("plot, in place");
        let copy = Arc::new(std::fs::read(&ds.store).expect("read the fixture store"));
        let view = den_store::Store::open(&copy).expect("open the copy");
        let copied = Index::from_store_plot(&view, copy.clone()).expect("plot, over the copy");
        for (id, media) in [(1, Movie), (2, Movie), (4, Tv), (101, Tv)] {
            assert_eq!(mapped.nearest(id, media, 12), copied.nearest(id, media, 12), "{media:?} {id}");
            let by_type = |index: &Index| index.nearest_by_type(id, media, 12);
            assert_eq!(by_type(&mapped), by_type(&copied), "{media:?} {id}");
        }
        assert!(Index::from_store_plot(&view, ds.mapped.clone()).is_err(), "another copy's bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TMDB numbers kept for the fixture's movie 1 alone, joined onto its store.
    fn kept(ds: &Dataset) -> crate::ratings::RatingsIndex {
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        crate::ratings::build(&mapped.view(), &HashMap::from([((0, 1), (8.4, 9000))]))
            .expect("movie 1 is kept")
    }
}
