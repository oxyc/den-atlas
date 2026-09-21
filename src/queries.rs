//! Index queries (`den-index`) over the dataset's plot and premise indexes: the label taxonomy, label rows
//! and More Like This — and the Wikidata facts `/recommend` reads beside them. The indexes load on the first
//! query and are released after a few idle minutes, so an atlas nobody is asking holds none of their ~80 MB
//! of vectors; the first query after an idle spell pays the load from disk.

use crate::dataset::Dataset;
use crate::facts::Facts;
use crate::fit::Corpus;
use crate::plotrows::{read_cards, Card, PlotFacets};
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
    /// Every title in the published corpus. The indexes below may be partial; this is their denominator.
    pub population: usize,
    pub dataset_version: String,
    pub plot: Index,
    /// The premise index, when the dataset ships one that loads; without it More Like This is plot-only.
    pub premise: Option<Index>,
    /// The facet index — country, original language, year, votes — built from the store's own columns
    /// where there is one, else from `facets.bin`. Without either the facet lane answers nothing.
    pub facets: Option<FacetIndex>,
    /// Which of the two built it. Reported on the load line, because the two differ by 9,086 titles and a
    /// silent fall back to the blob is a narrower attribute search that nothing else would show.
    pub facets_from_store: bool,
    /// The Wikidata facts, when the dataset ships a file that reads; without them `/recommend` reads labels and
    /// facets alone.
    pub facts: Option<Facts>,
    /// The plot facets, and the cards their rows are drawn with; without both, `/index/plot` rows are empty.
    pub plot_facets: Option<PlotFacets>,
    /// The per-title signals More Like This ranks on; without them the rail falls back to vectors and
    /// labels, which is how it worked before they existed.
    /// The mapped store and its corpus-wide aggregates. Absent ⇒ the rail falls back to vectors
    /// and labels alone, which is how it worked before the store existed.
    pub store: Option<crate::store::LoadedStore>,
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
            // `LoadedStore::open` already proved this builds — `check` calls the same constructor — so
            // the `else` below is the no-store case, which `store_unusable` reports.
            let facets = self
                .store
                .as_ref()
                .and_then(|s| crate::rail::SeedFacets::new(&s.view(), &s.aggregates, media_type).ok());
            let Some(facets) = facets else {
                return den_index::more_like_this(
                    Some(&self.plot),
                    self.premise.as_ref(),
                    tmdb_id,
                    media_type,
                )
                .into();
            };
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
    /// From the STORE first. It used to come only from `facets.bin`, which fell 9,007 titles behind the
    /// corpus because nothing rebuilt it, and a title with no row there sorts by tmdbId — which is how
    /// *La Job* (tv:5) came to sit next to *Game of Thrones*. The store carries one for 47,551 of 47,618
    /// rows, 9,019 more than the blob.
    ///
    /// `facets.bin` remains the fallback while it is still shipped, so a dataset published before the
    /// `votes` section existed keeps ordering its rows the way it always did.
    pub fn votes(&self, media_type: den_index::MediaType, tmdb_id: u32) -> u32 {
        let from_store = self.store.as_ref().and_then(|loaded| {
            let view = loaded.view();
            let media = u8::from(media_type == den_index::MediaType::Tv);
            let row = view.row_of(media, tmdb_id).ok().flatten()?;
            view.per_row::<u32>("votes").ok()?.get(row.0).copied()
        });
        from_store.unwrap_or_else(|| {
            self.facets.as_ref().and_then(|f| f.title(tmdb_id, media_type)).map_or(0, |t| t.votes)
        })
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

/// The facet index from the store's own columns, so attribute search covers the whole corpus.
///
/// `facets.bin` is a separate producer that fell 9,086 titles behind the corpus because nothing rebuilt it,
/// and the four facts it holds — country, original language, year, vote count — are all in the store. This
/// reads them from there instead. `den-index` cannot depend on `den-store` (it must keep building for
/// wasm32 and aarch64-apple-tvos), so the index is FILLED here rather than read there.
///
/// The country and language taken are the FIRST each title lists, which is what the blob held: one code per
/// title. A title with several origins is findable by the one Wikidata lists first, exactly as before.
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

/// A labels blob and its vectors blob.
type BlobPair = (PathBuf, PathBuf);

pub struct IndexQueries {
    population: usize,
    dataset_version: String,
    plot: BlobPair,
    premise: Option<BlobPair>,
    facets: Option<PathBuf>,
    facts: Vec<PathBuf>,
    plot_facets: Option<PathBuf>,
    store: Option<PathBuf>,
    metadata: Option<PathBuf>,
    loaded: Mutex<Option<(Arc<Indexes>, Instant)>>,
    /// Held while loading, so concurrent first queries wait for one load instead of each starting their own.
    loading: tokio::sync::Mutex<()>,
    /// Whether the dataset declares a facts file the last load couldn't read (`/health`).
    facts_unusable: AtomicBool,
    /// Whether the last load ended without a usable store — declared and unreadable, or not declared
    /// at all. Both answer More Like This the same way, so `/health` reports them the same way.
    store_unusable: AtomicBool,
}

impl IndexQueries {
    pub fn new(ds: &Dataset) -> Self {
        let premise = ds
            .premise_labels
            .as_ref()
            .zip(ds.premise_vectors.as_ref())
            .map(|(labels, vectors)| (labels.path.clone(), vectors.path.clone()));
        IndexQueries {
            population: usize::try_from(ds.meta.count).unwrap_or(usize::MAX),
            dataset_version: ds.meta.dataset_version.clone(),
            plot: (ds.labels.path.clone(), ds.vectors.path.clone()),
            premise,
            facets: ds.facets.as_ref().map(|f| f.path.clone()),
            facts: ds.facts.clone(),
            plot_facets: ds.plot_facets.clone(),
            store: ds.store.clone(),
            metadata: ds.metadata.as_ref().map(|m| m.path.clone()),
            loaded: Mutex::new(None),
            loading: tokio::sync::Mutex::new(()),
            facts_unusable: AtomicBool::new(false),
            store_unusable: AtomicBool::new(false),
        }
    }

    /// Whether the last index load ended without a store: More Like This then falls back to the
    /// pre-pooled scorer, which draws candidates from the premise index alone.
    pub fn store_unusable(&self) -> bool {
        self.store_unusable.load(Ordering::Relaxed)
    }

    /// Whether the dataset declares a facts file that the last index load couldn't read: `/recommend` and search
    /// then run without facts, which only a log line said before.
    pub fn facts_unusable(&self) -> bool {
        self.facts_unusable.load(Ordering::Relaxed)
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
            population: self.population,
            dataset_version: self.dataset_version.clone(),
            plot: self.plot.clone(),
            premise: self.premise.clone(),
            facets: self.facets.clone(),
            facts: self.facts.clone(),
            plot_facets: self.plot_facets.clone(),
            store: self.store.clone(),
            metadata: self.metadata.clone(),
        };
        let (indexes, phases) = tokio::task::spawn_blocking(move || load(&sources))
            .await
            .map_err(|e| format!("load task: {e}"))??;
        let took = started.elapsed();
        let count = |n: Option<usize>| n.map_or("none".to_owned(), |n| format!("{n} titles"));
        let premise = count(indexes.premise.as_ref().map(Index::len));
        let facets = format!(
            "{} ({})",
            count(indexes.facets.as_ref().map(FacetIndex::len)),
            if indexes.facets_from_store { "store" } else { "facets.bin" }
        );
        let facts = count(indexes.facts.as_ref().map(Facts::len));
        let plot_facets = count(indexes.plot_facets.as_ref().map(PlotFacets::len));
        // The store gets counted like everything else. It was the one part of the load that reported no
        // number, in a log line whose whole job is to say what arrived — so the artifact More Like This
        // ranks on was the one you could not confirm had loaded without reading /health.
        let store = count(indexes.store.as_ref().map(|s| s.store.rows()));
        eprintln!(
            "index loaded: {} titles, premise {premise}, facets {facets}, facts {facts}, plot facets {plot_facets}, store {store}, in {:.1}s ({phases})",
            indexes.plot.len(),
            took.as_secs_f64()
        );
        self.facts_unusable.store(!self.facts.is_empty() && indexes.facts.is_none(), Ordering::Relaxed);
        // Absent for ANY reason, including never declared. This used to require `self.store.is_some()`,
        // on the reasoning that a dataset which never promised a store cannot have broken one — but the
        // rail ranks on the store now, and the two cases are indistinguishable from the outside: both
        // answer More Like This with the pre-pooled scorer. A manifest published without `storeFile`
        // would have degraded every row silently, with `/health` green. `check` refuses such a manifest
        // outright; this is the second half, for a generation that got past it.
        self.store_unusable.store(indexes.store.is_none(), Ordering::Relaxed);
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

/// Where each part of the indexes is read from.
struct Sources {
    population: usize,
    dataset_version: String,
    plot: BlobPair,
    premise: Option<BlobPair>,
    facets: Option<PathBuf>,
    facts: Vec<PathBuf>,
    plot_facets: Option<PathBuf>,
    store: Option<PathBuf>,
    metadata: Option<PathBuf>,
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

/// The indexes, and how long each part took. Every part is its own file and parse, so they load side by side —
/// the first query after an idle spell waits on this — and then the display title index, which needs the cards,
/// the facts and the facets.
fn load(sources: &Sources) -> Result<(Indexes, String), String> {
    let (plot, premise, facets, store, cards) = std::thread::scope(|scope| {
        let plot = scope.spawn(|| timed(|| read_index(&sources.plot)));
        // A broken premise index costs premise-led More Like This, not the whole feature.
        let premise = scope.spawn(|| {
            timed(|| {
                sources.premise.as_ref().and_then(|pair| {
                    read_index(pair)
                        .map_err(|e| eprintln!("premise index unusable ({e}) — More Like This is plot-only"))
                        .ok()
                })
            })
        });
        // Like the premise index, an unusable facet blob costs only its own feature.
        let facets = scope.spawn(|| {
            timed(|| {
                sources.facets.as_ref().and_then(|path| match std::fs::read(path) {
                    Ok(blob) => FacetIndex::from_blob(&blob).or_else(|| {
                        eprintln!("facet index {} is not a DFI2 blob — facet search is off", path.display());
                        None
                    }),
                    Err(e) => {
                        eprintln!("read {}: {e} — facet search is off", path.display());
                        None
                    }
                })
            })
        });
        // The store: every per-title signal the rail ranks on, mapped rather than parsed. Read on the
        // load threads beside everything else; without it the pooled scorer is unavailable and More Like
        // This falls back to vectors and labels, which is how it worked before the store existed.
        let store = scope.spawn(|| {
            timed(|| {
                sources.store.as_ref().and_then(|path| match crate::store::LoadedStore::open(path) {
                    Ok(loaded) => Some(loaded),
                    Err(e) => {
                        // Name the file and the reason. "could not load the dataset" is the message that
                        // cost nineteen minutes of quiet degradation the last time a blob went bad.
                        eprintln!("store unusable ({e}) — More Like This falls back to vectors and labels");
                        None
                    }
                })
            })
        });
        let cards = scope.spawn(|| {
            timed(|| {
                sources.metadata.as_ref().and_then(|path| {
                    read_cards(path)
                        .map_err(|e| {
                            eprintln!(
                                "metadata unusable ({e}) — plot rows are empty, search has no display titles"
                            )
                        })
                        .ok()
                })
            })
        });
        (joined(plot), joined(premise), joined(facets), joined(store), joined(cards))
    });
    let ((plot, plot_took), (premise, premise_took), (facets, facets_took)) = (plot, premise, facets);
    let plot = plot?;
    let (cards, cards_took) = cards;
    let (store, store_took) = store;
    // The facts come out of the store, so this runs AFTER it rather than beside it. `factsFile` is a
    // 43 MB JSON blob that atlas alone reads — nothing serves it and no client fetches it — and parsing
    // it was 1.04 s of a 1.6 s load, now 0.38 s off the store.
    //
    // Switched on only once `Facts::from_store` answered IDENTICALLY to the JSON reader on the real
    // corpus: 0 of 47,618 records differ. Getting there found four real losses in the store, three of
    // which would have changed what people see — series genres kept as TMDB composites (which dropped
    // Horror from Chilling Adventures of Sabrina), genres sorted out of the genreMap's order, the
    // franchise interned against a table that holds almost no franchises, and 1,236 entity references
    // the table did not describe being dropped. The test that found them is `facts::tests::
    // the_store_answers_what_the_json_did`, and it is opt-in because it needs the real artifacts.
    //
    // The sidecar stays as a fallback for a generation published before the store carried them.
    let (mut facts, facts_took) = timed(|| {
        let from_store = store.as_ref().and_then(|s| {
            Facts::from_store(&s.view())
                .map_err(|e| eprintln!("facts unusable from the store ({e}) — trying factsFile"))
                .ok()
        });
        from_store.or_else(|| {
            sources.facts.iter().find_map(|path| match Facts::read(path) {
                Ok(facts) => Some(facts),
                Err(e) => {
                    // Name the file: with several candidates, "facts unusable" alone does not say which
                    // one, and the next line may be a success from a different file.
                    eprintln!("facts unusable ({}: {e}) — trying the next candidate", path.display());
                    None
                }
            })
        })
    });
    // The facet rows come out of the store, so this runs AFTER it rather than beside it. It used to read
    // `plotFacetsFile`, a 5,336-title sidecar frozen at a dead datasetVersion; the store answers the same
    // axes for all 47,618 titles, and three more besides.
    let (plot_facets, plot_facets_took) = timed(|| {
        let from_store = store.as_ref().and_then(|s| {
            PlotFacets::from_store(&s.view())
                .map_err(|e| eprintln!("facet rows unusable ({e}) — falling back to plotFacetsFile"))
                .ok()
        });
        // The sidecar only when there is no store to read them from: a dataset published before the
        // `facet_v` sections existed still gets its rows, at the 5,336 titles it described.
        from_store.or_else(|| {
            sources.plot_facets.as_ref().and_then(|path| {
                PlotFacets::read(path)
                    .map_err(|e| eprintln!("plot facets unusable ({e}) — plot rows are empty"))
                    .ok()
            })
        })
    });
    // The facet index comes out of the store too, for the same reason: `facets.bin` covers 38,532 titles
    // and the store covers 47,618, so attribute search ("spanish series", "80s korean horror") was asking
    // a table 9,086 titles behind the corpus it is searching. The blob stays as the fallback for a dataset
    // published before the store carried these columns.
    let (facets, facets_from_store) = match (store.as_ref(), facts.as_ref()) {
        (Some(loaded), Some(facts)) => match facet_index_from(facts, &loaded.view()) {
            Ok(built) => (Some(built), true),
            Err(e) => {
                eprintln!("facet index unusable from the store ({e}) — falling back to facets.bin");
                (facets, false)
            }
        },
        _ => (facets, false),
    };

    // The facts hand their titles' other names to the display index, which is then the only one holding them.
    let (display, display_took) = timed(|| {
        let other_names = facts.as_mut().map(Facts::take_titles).unwrap_or_default();
        cards.as_ref().map(|cards| {
            let votes = |kind, id| {
                store
                    .as_ref()
                    .and_then(|loaded| {
                        let view = loaded.view();
                        let media = u8::from(kind == den_index::MediaType::Tv);
                        let row = view.row_of(media, id).ok().flatten()?;
                        view.per_row::<u32>("votes").ok()?.get(row.0).copied()
                    })
                    .map(f64::from)
                    .unwrap_or_else(|| {
                        facets.as_ref().and_then(|f| f.title(id, kind)).map_or(0.0, |t| f64::from(t.votes))
                    })
            };
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
        "plot {}, premise {}, facts {}, metadata {}, facets {}, plot facets {}, store {}, display {}",
        seconds(plot_took),
        seconds(premise_took),
        seconds(facts_took),
        seconds(cards_took),
        seconds(facets_took),
        seconds(plot_facets_took),
        seconds(store_took),
        seconds(display_took)
    );
    let indexes = Indexes {
        population: sources.population,
        dataset_version: sources.dataset_version.clone(),
        plot,
        premise,
        facets,
        facets_from_store,
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

fn read_index((labels, vectors): &BlobPair) -> Result<Index, String> {
    let labels = std::fs::read(labels).map_err(|e| format!("read {}: {e}", labels.display()))?;
    let vector_bytes = std::fs::read(vectors).map_err(|e| format!("read {}: {e}", vectors.display()))?;
    Index::from_blobs(&labels, vector_bytes).map_err(|e| e.to_string())
}

/// A small, real dataset — plot and premise indexes over three movies and a series — written to `dir` and
/// loaded, for route tests. Heist is the biggest subgenre; "Campy/Cult" has a slash to encode.
#[cfg(test)]
pub fn write_fixture(dir: &std::path::Path) -> Dataset {
    type Row<'a> = (u32, &'a str, &'a str, &'a [(&'a str, f64)], &'a [(&'a str, f64)], [i8; 3]);
    fn blobs(rows: &[Row<'_>]) -> (String, Vec<u8>) {
        let records: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, kind, genre, subs, moods, _)| {
                let labels = |ls: &[(&str, f64)]| -> Vec<serde_json::Value> {
                    ls.iter().map(|(l, c)| serde_json::json!({"label": l, "confidence": c})).collect()
                };
                serde_json::json!({"tmdbId": id, "mediaType": kind, "primaryGenre": genre, "animated": false,
                                   "source": "llm", "subgenres": labels(subs), "moods": labels(moods)})
            })
            .collect();
        let labels = serde_json::json!({"taxonomyVersion": "t02", "count": rows.len(), "records": records});
        let mut vectors = (rows.len() as i32).to_le_bytes().to_vec();
        vectors.extend_from_slice(&3i32.to_le_bytes());
        for row in rows {
            vectors.extend(row.5.iter().map(|&v| v as u8));
        }
        (labels.to_string(), vectors)
    }
    // Eight zero-vector series make the fixture's semantic-score distribution large enough for one clear
    // movie match to cross search.rs's z=2.5 floor. They deliberately have no cards: route tests can prove
    // which of the four drawable titles each semantic index proposes without expanding every other fixture.
    let plot = blobs(&[
        (1, "movie", "Drama", &[("Heist", 0.9)], &[("Tense", 0.8)], [100, 0, 0]),
        (2, "movie", "Drama", &[("Heist", 0.8)], &[], [90, 10, 0]),
        (3, "movie", "Comedy", &[("Heist", 0.6), ("Campy/Cult", 0.9)], &[], [0, 100, 0]),
        (4, "tv", "Drama", &[("Heist", 0.95)], &[], [100, 0, 0]),
        (101, "tv", "Drama", &[], &[], [0, 0, 0]),
        (102, "tv", "Drama", &[], &[], [0, 0, 0]),
        (103, "tv", "Drama", &[], &[], [0, 0, 0]),
        (104, "tv", "Drama", &[], &[], [0, 0, 0]),
        (105, "tv", "Drama", &[], &[], [0, 0, 0]),
        (106, "tv", "Drama", &[], &[], [0, 0, 0]),
        (107, "tv", "Drama", &[], &[], [0, 0, 0]),
        (108, "tv", "Drama", &[], &[], [0, 0, 0]),
    ]);
    let premise = blobs(&[
        (1, "movie", "Drama", &[("Heist", 0.9)], &[("Tense", 0.8)], [100, 0, 0]),
        (2, "movie", "Drama", &[("Heist", 0.8)], &[], [0, 100, 0]),
        (3, "movie", "Comedy", &[("Heist", 0.6), ("Campy/Cult", 0.9)], &[], [95, 0, 0]),
        (4, "tv", "Drama", &[("Heist", 0.95)], &[], [100, 0, 0]),
        (101, "tv", "Drama", &[], &[], [0, 0, 0]),
        (102, "tv", "Drama", &[], &[], [0, 0, 0]),
        (103, "tv", "Drama", &[], &[], [0, 0, 0]),
        (104, "tv", "Drama", &[], &[], [0, 0, 0]),
        (105, "tv", "Drama", &[], &[], [0, 0, 0]),
        (106, "tv", "Drama", &[], &[], [0, 0, 0]),
        (107, "tv", "Drama", &[], &[], [0, 0, 0]),
        (108, "tv", "Drama", &[], &[], [0, 0, 0]),
    ]);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("labels.json"), &plot.0).unwrap();
    std::fs::write(dir.join("vectors.bin"), &plot.1).unwrap();
    std::fs::write(dir.join("premise-labels.json"), &premise.0).unwrap();
    std::fs::write(dir.join("premise-vectors.bin"), &premise.1).unwrap();
    // Korean movies 1 (1985, 100 votes) and 2 (1995, 500), Spanish movie 3 (1985), Korean series 4 (2010, 300).
    let mut facets = b"DFI2".to_vec();
    facets.extend_from_slice(&4u32.to_le_bytes());
    for (id, tv, country, year, votes) in [
        (1i32, 0u8, b"KR", 1985u16, 100u32),
        (2, 0, b"KR", 1995, 500),
        (3, 0, b"ES", 1985, 50),
        (4, 1, b"KR", 2010, 300),
    ] {
        facets.extend_from_slice(&id.to_le_bytes());
        facets.push(tv);
        facets.extend_from_slice(b"xx");
        facets.extend_from_slice(country);
        facets.extend_from_slice(&year.to_le_bytes());
        facets.extend_from_slice(&votes.to_le_bytes());
    }
    std::fs::write(dir.join("facets.bin"), &facets).unwrap();
    std::fs::write(dir.join("facts-slim.json"), crate::facts::tests::SAMPLE).unwrap();
    std::fs::write(dir.join("plot-facets.json"), crate::plotrows::tests::SAMPLE).unwrap();
    let metadata = serde_json::json!([
        {"tmdbId": 1, "mediaType": "movie", "title": "One", "posterPath": "/1.jpg", "year": 1985},
        {"tmdbId": 2, "mediaType": "movie", "title": "Two", "posterPath": "/2.jpg", "year": 1995},
        {"tmdbId": 3, "mediaType": "movie", "title": "Three", "posterPath": null, "year": 1985},
        {"tmdbId": 4, "mediaType": "tv", "title": "Four", "posterPath": "/4.jpg", "year": 2010},
    ])
    .to_string();
    std::fs::write(dir.join("metadata.json"), &metadata).unwrap();
    let meta = serde_json::json!({
        "factsSlimFile": "facts-slim.json",
        "plotFacetsFile": "plot-facets.json",
        "metadataFile": "metadata.json", "metadataBytes": metadata.len(), "metadataSha256": "f",
        "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3, "count": 12,
        "quantization": "int8",
        "labelsFile": "labels.json", "labelsBytes": plot.0.len(), "labelsSha256": "a",
        "vectorsFile": "vectors.bin", "vectorsBytes": plot.1.len(), "vectorsSha256": "b",
        "premiseEmbeddingModel": "pm", "premiseDims": 3, "premiseCount": 12,
        "premiseLabelsFile": "premise-labels.json", "premiseLabelsBytes": premise.0.len(),
        "premiseLabelsSha256": "c",
        "premiseVectorsFile": "premise-vectors.bin", "premiseVectorsBytes": premise.1.len(),
        "premiseVectorsSha256": "d",
        "facetsFile": "facets.bin", "facetsBytes": facets.len(), "facetsSha256": "e",
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
        assert_eq!(indexes.facts.as_ref().map(|f| f.len()), Some(3), "the facts load with the indexes");
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

    #[tokio::test]
    async fn a_dataset_that_does_not_parse_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-bad-{}", std::process::id()));
        let ds = write_fixture(&dir);
        std::fs::write(&ds.labels.path, b"not json").unwrap();
        assert!(IndexQueries::new(&ds).get(|| ()).await.is_err());
    }
}
