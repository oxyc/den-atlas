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

use crate::dataset::Dataset;
use crate::facts::Facts;
use crate::fit::Corpus;
use crate::plotrows::{cards_from_store, Card, PlotFacets};
use crate::util::lock;
use den_index::{FacetIndex, Index};
use den_titlesearch::{TitleIndex, TitleRecord};
use std::collections::HashMap;
use std::path::PathBuf;
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
    /// The plot facets, and the cards their rows are drawn with; without both, `/index/plot` rows are empty.
    pub plot_facets: Option<PlotFacets>,
    /// The mapped store and its corpus-wide aggregates: the artifact everything above was read out of,
    /// kept because the rail addresses its columns per candidate rather than copying them.
    pub store: crate::store::LoadedStore,
    pub cards: Option<HashMap<(den_index::MediaType, u32), Card>>,
    /// The cards' display titles as a fuzzy title index, for search: TMDB's export names a title by its original
    /// title, so "parasite" finds only what is displayed as "Parasite" here.
    pub display: Option<TitleIndex>,
    /// More Like This answers already worked out, by title (`Indexes::more_like_this`).
    similar: Mutex<HashMap<Key, Arc<[u32]>>>,
    /// Row orders already worked out, by type and constraints (`Indexes::row_order`).
    rows: Mutex<HashMap<String, Arc<[Key]>>>,
    /// What a billboard's fit reads off the index as a whole (`Indexes::corpus`).
    corpus: OnceLock<Corpus>,
}

type Key = (den_index::MediaType, u32);

/// More Like This answers and row orders kept at most: bounded, and simply started over when full. Both are small
/// (a row order is at most a few thousand titles, an answer at most `den_index::MAX_ROW` ids).
const SIMILAR_MEMO: usize = 4096;
const ROW_MEMO: usize = 128;

impl Indexes {
    /// More Like This for a title, worked out once while the indexes are loaded: it is deterministic for
    /// the dataset, and asked again and again — a billboard's seeds on every Home load, the title a search
    /// names, a detail page.
    ///
    /// The pooled scorer when the dataset ships the signals it needs, and the original otherwise. They are
    /// not small variations of each other: the original draws candidates from the premise index alone, so a
    /// plot neighbour can never enter the row — measured on The Wire, its plot top-20 and premise top-40 do
    /// not intersect at all, and Homicide: Life on the Street sits at plot rank 10 and is discarded.
    pub fn more_like_this(&self, tmdb_id: u32, media_type: den_index::MediaType) -> Arc<[u32]> {
        memoised(&self.similar, (media_type, tmdb_id), SIMILAR_MEMO, || {
            // `LoadedStore::open` already proved this builds — `check` calls the same constructor — and
            // the load fails without a store, so there is no arm here that answers without one.
            let facets = crate::rail::SeedFacets::new(&self.store.view(), &self.store.aggregates, media_type)
                .expect("MappedStore::check builds this at load, so it cannot fail per request");
            let authorship =
                self.facts.as_ref().map(|f| crate::rail::SeedAuthorship::of(f, media_type, tmdb_id));
            den_index::more_like_this_pooled(
                Some(&self.plot),
                self.premise.as_ref(),
                tmdb_id,
                media_type,
                authorship.as_ref().map(|a| a as &dyn den_index::Authorship),
                Some(&facets),
            )
            .into()
        })
    }

    /// What a billboard's fit reads off the index as a whole (`fit::Corpus`), worked out once: the first time it is
    /// asked for, which the load does, so no billboard waits on it.
    pub fn corpus(&self) -> &Corpus {
        self.corpus.get_or_init(|| Corpus::of(self))
    }

    /// A title's TMDB vote count — what every browse row is ORDERED by.
    ///
    /// From the store's `votes` column. It used to come from `facets.bin`, which fell 9,007 titles behind
    /// the corpus because nothing rebuilt it, and a title with no row there sorts by tmdbId — which is how
    /// *La Job* (tv:5) came to sit next to *Game of Thrones*. 0 for a row the store has no count for, which
    /// is what the blob answered for a title it did not describe.
    pub fn votes(&self, media_type: den_index::MediaType, tmdb_id: u32) -> u32 {
        votes_of(&self.store, media_type, tmdb_id)
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

/// A title's vote count, out of the store. Shared by `Indexes::votes` and the display index's ranking,
/// which used to read the column with two copies of the same four lines.
fn votes_of(loaded: &crate::store::LoadedStore, media_type: den_index::MediaType, tmdb_id: u32) -> u32 {
    let view = loaded.view();
    let media = u8::from(media_type == den_index::MediaType::Tv);
    let row = view.row_of(media, tmdb_id).ok().flatten();
    row.and_then(|row| view.per_row::<u32>("votes").ok()?.get(row.0).copied()).unwrap_or(0)
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
fn facet_index_from(
    facts: &Facts,
    store: &den_store::Store<'_>,
) -> Result<FacetIndex, den_store::StoreError> {
    let keys = store.per_row::<u64>("keys")?;
    let votes = store.per_row::<u32>("votes")?;
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
            votes.get(i).copied().unwrap_or(0),
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
    store: PathBuf,
    loaded: Mutex<Option<(Arc<Indexes>, Instant)>>,
    /// Held while loading, so concurrent first queries wait for one load instead of each starting their own.
    loading: tokio::sync::Mutex<()>,
    /// Whether the last load couldn't read the store's facts sections (`/health`).
    facts_unusable: AtomicBool,
    /// Whether the last load failed outright: the store would not open, so every index route answers 503.
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
}

impl IndexQueries {
    pub fn new(ds: &Dataset) -> Self {
        IndexQueries {
            dataset_version: ds.meta.dataset_version.clone(),
            taxonomy_version: ds.meta.taxonomy_version.clone(),
            store: ds.store.clone(),
            loaded: Mutex::new(None),
            loading: tokio::sync::Mutex::new(()),
            facts_unusable: AtomicBool::new(false),
            store_unusable: AtomicBool::new(false),
            rows_unusable: AtomicBool::new(false),
        }
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
            store: self.store.clone(),
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
    store: PathBuf,
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
        store: ds.store.clone(),
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
/// The store is opened FIRST and a failure ends the load: it is the only input, so there is nothing to
/// build the other parts out of and nothing to serve if it will not open. Everything after it is a read
/// of a section of that one mapping, so the parallel phase is what can genuinely run side by side — both
/// vector indexes and the cards — while the facts, the facet rows, the facet index and the display index
/// follow in the order they depend on each other.
fn load(sources: &Sources) -> Result<(Indexes, String), String> {
    // Name the file and the reason. "could not load the dataset" is the message that cost nineteen
    // minutes of quiet degradation the last time a blob went bad.
    let (store, store_took) = timed(|| {
        crate::store::LoadedStore::open(&sources.store)
            .map_err(|e| format!("store {} is unusable: {e}", sources.store.display()))
    });
    let store = store?;
    let population = store.store.rows();

    let (plot, premise, cards) = std::thread::scope(|scope| {
        let view = || store.view();
        let plot = scope.spawn(move || timed(|| Index::from_store_plot(&view())));
        // A premise index that will not read costs premise-led More Like This, not the whole feature.
        let premise = scope.spawn(move || {
            timed(|| {
                Index::from_store_premise(&view())
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
        facet_index_from(facts, &store.view())
            .map_err(|e| eprintln!("facet index unusable ({e}) — attribute search is off"))
            .ok()
    });

    // The facts hand their titles' other names to the display index, which is then the only one holding them.
    let (display, display_took) = timed(|| {
        let other_names = facts.as_mut().map(Facts::take_titles).unwrap_or_default();
        cards.as_ref().map(|cards| {
            let votes = |kind, id| f64::from(votes_of(&store, kind, id));
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
        "store {}, plot {}, premise {}, cards {}, facts {}, facet rows {}, facets {}, display {}",
        seconds(store_took),
        seconds(plot_took),
        seconds(premise_took),
        seconds(cards_took),
        seconds(facts_took),
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
        plot_facets,
        store,
        cards,
        display,
        similar: Mutex::new(HashMap::new()),
        rows: Mutex::new(HashMap::new()),
        corpus: OnceLock::new(),
    };
    let (_, fit_took) = timed(|| {
        indexes.corpus();
    });
    Ok((indexes, format!("{phases}, fit {}", seconds(fit_took))))
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
    write_fixture_as(dir, "One", true)
}

/// The same fixture with a different display title for movie 1, for the tests about a word that is both a
/// facet and a title ("brazil").
#[cfg(test)]
pub fn write_fixture_titled(dir: &std::path::Path, movie_one: &str) -> Dataset {
    write_fixture_as(dir, movie_one, true)
}

/// The same fixture with no premise vectors, for the tests about a dataset whose store carries only the
/// plot space. Setting `premise_labels = None` on the dataset used to do this; the premise index is a
/// section of the store now, so the store is what has to lack it.
#[cfg(test)]
pub fn write_fixture_plot_only(dir: &std::path::Path) -> Dataset {
    write_fixture_as(dir, "One", false)
}

#[cfg(test)]
fn write_fixture_as(dir: &std::path::Path, movie_one: &str, premise: bool) -> Dataset {
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
            franchise: Some(50),
            based_kind: vec!["book", "play"],
            alias_titles: vec!["One", "하나", "Uno"],
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
    let entities = [
        Entity { qid: 1, name: "A Director", tmdb: Some(11), aliases: Vec::new() },
        // People search indexes the aliases as well as the name.
        Entity { qid: 2, name: "Lead Actor", tmdb: None, aliases: vec!["Bong Joon-ho", "기생충 배우"] },
        Entity { qid: 50, name: "A Franchise", tmdb: None, aliases: Vec::new() },
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

    /// A store swapped for something that is not one fails the load and SAYS SO through `/health`, rather
    /// than panicking or answering a degraded row. It is the only input, so there is nothing else to serve
    /// from — which is the difference from every optional blob this used to fall back through.
    #[tokio::test]
    async fn a_store_that_does_not_read_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-bad-{}", std::process::id()));
        let ds = write_fixture(&dir);
        std::fs::write(&ds.store, b"not a store").unwrap();
        let queries = IndexQueries::new(&ds);
        assert!(queries.get(|| ()).await.is_err());
        assert!(queries.store_unusable(), "a failed load must reach /health");
    }
}
