//! Index queries (`den-index`) over the dataset's plot and premise indexes: the label taxonomy, label rows
//! and More Like This. The indexes load on the first query and are released after a few idle minutes, so an
//! atlas nobody is asking holds none of their ~80 MB of vectors; the first query after an idle spell pays
//! the load from disk.

use crate::dataset::Dataset;
use crate::util::lock;
use den_index::{FacetIndex, Index};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// How long the indexes stay in memory after the last query.
const IDLE_RELEASE: Duration = Duration::from_secs(10 * 60);
const SWEEP_EVERY: Duration = Duration::from_secs(60);

pub struct Indexes {
    pub plot: Index,
    /// The premise index, when the dataset ships one that loads; without it More Like This is plot-only.
    pub premise: Option<Index>,
    /// The facet index, when the dataset ships `facets.bin`; without it the facet lane answers nothing.
    pub facets: Option<FacetIndex>,
}

/// A labels blob and its vectors blob.
type BlobPair = (PathBuf, PathBuf);

pub struct IndexQueries {
    plot: BlobPair,
    premise: Option<BlobPair>,
    facets: Option<PathBuf>,
    loaded: Mutex<Option<(Arc<Indexes>, Instant)>>,
    /// Held while loading, so concurrent first queries wait for one load instead of each starting their own.
    loading: tokio::sync::Mutex<()>,
}

impl IndexQueries {
    pub fn new(ds: &Dataset) -> Self {
        let premise = ds
            .premise_labels
            .as_ref()
            .zip(ds.premise_vectors.as_ref())
            .map(|(labels, vectors)| (labels.path.clone(), vectors.path.clone()));
        IndexQueries {
            plot: (ds.labels.path.clone(), ds.vectors.path.clone()),
            premise,
            facets: ds.facets.as_ref().map(|f| f.path.clone()),
            loaded: Mutex::new(None),
            loading: tokio::sync::Mutex::new(()),
        }
    }

    /// The indexes — loaded first if they aren't in memory — and how long that load took (`None` when they
    /// already were).
    pub async fn get(&self) -> Result<(Arc<Indexes>, Option<Duration>), String> {
        if let Some(indexes) = self.touch() {
            return Ok((indexes, None));
        }
        let _loading = self.loading.lock().await;
        if let Some(indexes) = self.touch() {
            return Ok((indexes, None));
        }
        let started = Instant::now();
        let (plot, premise, facets) = (self.plot.clone(), self.premise.clone(), self.facets.clone());
        let indexes = tokio::task::spawn_blocking(move || load(&plot, premise.as_ref(), facets.as_ref()))
            .await
            .map_err(|e| format!("load task: {e}"))??;
        let took = started.elapsed();
        let premise = indexes.premise.as_ref().map_or("none".to_owned(), |p| format!("{} titles", p.len()));
        let facets = indexes.facets.as_ref().map_or("none".to_owned(), |f| format!("{} titles", f.len()));
        eprintln!(
            "index loaded: {} titles, premise {premise}, facets {facets}, in {:.1}s",
            indexes.plot.len(),
            took.as_secs_f64()
        );
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

fn load(plot: &BlobPair, premise: Option<&BlobPair>, facets: Option<&PathBuf>) -> Result<Indexes, String> {
    let plot = read_index(plot)?;
    // Like the premise index, an unusable facet blob costs only its own feature.
    let facets = facets.and_then(|path| match std::fs::read(path) {
        Ok(blob) => FacetIndex::from_blob(&blob).or_else(|| {
            eprintln!("facet index {} is not a DFI2 blob — facet search is off", path.display());
            None
        }),
        Err(e) => {
            eprintln!("read {}: {e} — facet search is off", path.display());
            None
        }
    });
    // A broken premise index costs premise-led More Like This, not the whole feature.
    let premise = premise.and_then(|pair| {
        read_index(pair)
            .map_err(|e| eprintln!("premise index unusable ({e}) — More Like This is plot-only"))
            .ok()
    });
    Ok(Indexes { plot, premise, facets })
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
    let plot = blobs(&[
        (1, "movie", "Drama", &[("Heist", 0.9)], &[("Tense", 0.8)], [100, 0, 0]),
        (2, "movie", "Drama", &[("Heist", 0.8)], &[], [90, 10, 0]),
        (3, "movie", "Comedy", &[("Heist", 0.6), ("Campy/Cult", 0.9)], &[], [0, 100, 0]),
        (4, "tv", "Drama", &[("Heist", 0.95)], &[], [100, 0, 0]),
    ]);
    let premise = blobs(&[
        (1, "movie", "Drama", &[("Heist", 0.9)], &[("Tense", 0.8)], [100, 0, 0]),
        (2, "movie", "Drama", &[("Heist", 0.8)], &[], [0, 100, 0]),
        (3, "movie", "Comedy", &[("Heist", 0.6), ("Campy/Cult", 0.9)], &[], [95, 0, 0]),
        (4, "tv", "Drama", &[("Heist", 0.95)], &[], [100, 0, 0]),
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
    let meta = serde_json::json!({
        "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3, "count": 4,
        "quantization": "int8",
        "labelsFile": "labels.json", "labelsBytes": plot.0.len(), "labelsSha256": "a",
        "vectorsFile": "vectors.bin", "vectorsBytes": plot.1.len(), "vectorsSha256": "b",
        "premiseEmbeddingModel": "pm", "premiseDims": 3, "premiseCount": 4,
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

        let (indexes, first) = queries.get().await.unwrap();
        assert!(first.is_some(), "the first query loads");
        assert!(indexes.premise.is_some());
        let (_, again) = queries.get().await.unwrap();
        assert!(again.is_none(), "a warm query doesn't");

        tokio::time::advance(IDLE_RELEASE - Duration::from_secs(1)).await;
        assert!(!queries.release_if_idle(), "released before it was idle");
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(queries.release_if_idle());

        // A query that held the index across the release still has it; the next query reloads.
        assert_eq!(indexes.plot.len(), 4);
        let (_, reload) = queries.get().await.unwrap();
        assert!(reload.is_some(), "the query after a release loads again");
    }

    #[tokio::test]
    async fn a_dataset_that_does_not_parse_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("den-atlas-queries-bad-{}", std::process::id()));
        let ds = write_fixture(&dir);
        std::fs::write(&ds.labels.path, b"not json").unwrap();
        assert!(IndexQueries::new(&ds).get().await.is_err());
    }
}
