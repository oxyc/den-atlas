//! One index: the labels blob (`labels-tNN.json`) and the vectors blob (`vectors-*.bin`: a little-endian
//! `[i32 count][i32 dim]` header, then `count × dim` int8 rows in label order). The dataset has two — plot
//! and premise — in different embedding spaces; each is an `Index`, and nothing here mixes them.

use crate::MediaType;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// The confidence a label needs to be shown in a row — the tvOS app's `displayConfidenceFloor`. The producer
/// keeps weaker labels as review material, but a row built from them puts the classifier's least confident
/// guesses in front of the viewer.
pub const DISPLAY_CONFIDENCE_FLOOR: f64 = 0.55;
const HEADER_BYTES: usize = 8;

#[derive(Debug)]
pub enum LoadError {
    Labels(String),
    Vectors(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Labels(e) => write!(f, "labels: {e}"),
            LoadError::Vectors(e) => write!(f, "vectors: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

#[derive(Deserialize)]
struct LabelsArtifact {
    #[serde(rename = "taxonomyVersion")]
    taxonomy_version: String,
    records: Vec<RawRecord>,
}

#[derive(Deserialize)]
struct RawRecord {
    #[serde(rename = "tmdbId")]
    tmdb_id: u32,
    #[serde(rename = "mediaType")]
    media_type: String,
    #[serde(rename = "primaryGenre", default)]
    primary_genre: String,
    #[serde(default)]
    subgenres: Vec<RawLabel>,
    #[serde(default)]
    moods: Vec<RawLabel>,
    #[serde(default)]
    animated: bool,
}

#[derive(Deserialize)]
struct RawLabel {
    label: String,
    confidence: f64,
}

/// A record with its label and genre names interned: the same few hundred names repeat across ~37k titles.
struct Record {
    tmdb_id: u32,
    /// `None` for a type the app doesn't know; such a row keeps its place (rows align with vectors) but never
    /// matches a query.
    media_type: Option<MediaType>,
    primary_genre: u32,
    animated: bool,
    subgenres: Box<[(u32, f64)]>,
    moods: Box<[(u32, f64)]>,
}

#[derive(Clone, Copy)]
struct Entry {
    row: u32,
    confidence: f64,
}

/// A neighbour: the title and its int8 dot-product similarity (the vectors were L2-normalised before
/// quantising, so dot ≈ cosine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Neighbor {
    pub tmdb_id: u32,
    pub media_type: MediaType,
    pub score: i32,
}

/// A title's labels, as stored.
#[derive(Debug, PartialEq)]
pub struct Labels<'a> {
    pub primary_genre: &'a str,
    pub animated: bool,
    pub subgenres: Vec<(&'a str, f64)>,
    pub moods: Vec<(&'a str, f64)>,
}

pub struct Index {
    taxonomy_version: String,
    dim: usize,
    records: Vec<Record>,
    names: Vec<Box<str>>,
    /// (type, tmdb id) → row. Ids collide across the movie and tv namespaces (1399 is both a movie and Game
    /// of Thrones), so a title is always looked up with its type. The first row wins a duplicate.
    rows: HashMap<(MediaType, u32), u32>,
    /// The vectors blob as delivered, header included; row r starts at `HEADER_BYTES + r × dim`.
    vectors: Vec<u8>,
    /// Label → titles carrying it, most confident first (stable, so equal confidences keep record order).
    subgenres: HashMap<u32, Vec<Entry>>,
    moods: HashMap<u32, Vec<Entry>>,
}

impl Index {
    /// Build from the two blobs. Fails when either doesn't parse or the vector count disagrees with the labels.
    pub fn from_blobs(labels_json: &[u8], vectors: Vec<u8>) -> Result<Index, LoadError> {
        let artifact: LabelsArtifact =
            serde_json::from_slice(labels_json).map_err(|e| LoadError::Labels(e.to_string()))?;
        let dim = vector_dimension(&vectors, artifact.records.len())?;
        let mut names: Vec<Box<str>> = Vec::new();
        let mut name_ids: HashMap<String, u32> = HashMap::new();
        let mut intern = |name: String| -> u32 {
            *name_ids.entry(name).or_insert_with_key(|name| {
                names.push(name.as_str().into());
                (names.len() - 1) as u32
            })
        };
        let mut records = Vec::with_capacity(artifact.records.len());
        for raw in artifact.records {
            let subgenres = raw.subgenres.into_iter().map(|l| (intern(l.label), l.confidence)).collect();
            let moods = raw.moods.into_iter().map(|l| (intern(l.label), l.confidence)).collect();
            records.push(Record {
                tmdb_id: raw.tmdb_id,
                media_type: MediaType::parse(&raw.media_type),
                primary_genre: intern(raw.primary_genre),
                animated: raw.animated,
                subgenres,
                moods,
            });
        }
        let mut rows = HashMap::with_capacity(records.len());
        for (row, record) in records.iter().enumerate() {
            if let Some(media_type) = record.media_type {
                rows.entry((media_type, record.tmdb_id)).or_insert(row as u32);
            }
        }
        let subgenres = buckets(&records, |r| &r.subgenres);
        let moods = buckets(&records, |r| &r.moods);
        Ok(Index {
            taxonomy_version: artifact.taxonomy_version,
            dim,
            records,
            names,
            rows,
            vectors,
            subgenres,
            moods,
        })
    }

    pub fn taxonomy_version(&self) -> &str {
        &self.taxonomy_version
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Every subgenre label, most-populated first, ties alphabetical — the full taxonomy behind the endless
    /// category rows.
    pub fn subgenre_labels(&self) -> Vec<&str> {
        self.labels_by_size(&self.subgenres)
    }

    /// Every mood label, most-populated first.
    pub fn mood_labels(&self) -> Vec<&str> {
        self.labels_by_size(&self.moods)
    }

    /// Titles carrying a subgenre label, most confident first, at or above `min_confidence`, optionally of one
    /// type; `skip` then `limit` page through them.
    pub fn titles_with_subgenre(
        &self,
        label: &str,
        media_type: Option<MediaType>,
        min_confidence: f64,
        skip: usize,
        limit: usize,
    ) -> Vec<(u32, MediaType)> {
        self.ranked(&self.subgenres, label, media_type, min_confidence, skip, limit)
    }

    /// Titles carrying a mood label; as `titles_with_subgenre`.
    pub fn titles_with_mood(
        &self,
        label: &str,
        media_type: Option<MediaType>,
        min_confidence: f64,
        skip: usize,
        limit: usize,
    ) -> Vec<(u32, MediaType)> {
        self.ranked(&self.moods, label, media_type, min_confidence, skip, limit)
    }

    pub fn labels(&self, tmdb_id: u32, media_type: MediaType) -> Option<Labels<'_>> {
        let record = &self.records[*self.rows.get(&(media_type, tmdb_id))? as usize];
        let named = |pairs: &[(u32, f64)]| pairs.iter().map(|&(n, c)| (self.name(n), c)).collect();
        Some(Labels {
            primary_genre: self.name(record.primary_genre),
            animated: record.animated,
            subgenres: named(&record.subgenres),
            moods: named(&record.moods),
        })
    }

    /// The `k` titles of the same type nearest to an indexed title, best first, excluding the title itself.
    /// Empty when the title isn't indexed. Ties go to the earlier row, so the answer never varies.
    pub fn nearest(&self, tmdb_id: u32, media_type: MediaType, k: usize) -> Vec<Neighbor> {
        let Some(&query_row) = self.rows.get(&(media_type, tmdb_id)) else { return Vec::new() };
        let query = self.row_vector(query_row as usize);
        self.top_k(
            k,
            |row| row != query_row as usize && self.records[row].media_type == Some(media_type),
            query,
        )
    }

    /// The `k` titles nearest to an outside vector — one embedded by the same model and quantiser as this
    /// index (semantic search, or a synopsis standing in for an unindexed title). Empty on a dimension
    /// mismatch: that's another vector space.
    pub fn nearest_to_vector(&self, query: &[i8], media_type: Option<MediaType>, k: usize) -> Vec<Neighbor> {
        if query.len() != self.dim {
            return Vec::new();
        }
        let query: Vec<u8> = query.iter().map(|&v| v as u8).collect();
        self.top_k(
            k,
            |row| {
                let kind = self.records[row].media_type;
                kind.is_some() && (media_type.is_none() || kind == media_type)
            },
            &query,
        )
    }

    /// The unit-length mean of the given titles' vectors — a taste centroid in THIS index's space. Titles not
    /// in the index are skipped; `None` when none are. Double precision, so a candidate's cosine to it isn't
    /// quantised twice.
    pub fn centroid(&self, titles: &[(u32, MediaType)]) -> Option<Vec<f64>> {
        let mut sum = vec![0.0; self.dim];
        let mut n = 0;
        for &(tmdb_id, media_type) in titles {
            let Some(&row) = self.rows.get(&(media_type, tmdb_id)) else { continue };
            for (s, &v) in sum.iter_mut().zip(self.row_vector(row as usize)) {
                *s += f64::from(v as i8);
            }
            n += 1;
        }
        if n == 0 {
            return None;
        }
        let mut norm = 0.0;
        for s in &mut sum {
            *s /= f64::from(n);
            norm += *s * *s;
        }
        let norm = norm.sqrt();
        if norm == 0.0 {
            return None;
        }
        sum.iter_mut().for_each(|s| *s /= norm);
        Some(sum)
    }

    /// A title's cosine similarity to a unit centroid from `centroid`, in [-1, 1]; `None` when the title isn't
    /// indexed or the centroid is from another space.
    pub fn cosine(&self, tmdb_id: u32, media_type: MediaType, centroid: &[f64]) -> Option<f64> {
        if centroid.len() != self.dim {
            return None;
        }
        let row = *self.rows.get(&(media_type, tmdb_id))? as usize;
        let (mut dot, mut norm) = (0.0, 0.0);
        for (&v, &c) in self.row_vector(row).iter().zip(centroid) {
            let x = f64::from(v as i8);
            dot += x * c;
            norm += x * x;
        }
        Some(if norm > 0.0 { dot / norm.sqrt() } else { 0.0 })
    }

    /// The taste boost the tvOS app's `TasteVector.boost` gives: the cosine to the liked centroid, clamped at
    /// 0 so the tilt only ever promotes; 0 for a title the index doesn't hold.
    pub fn taste_boost(&self, tmdb_id: u32, media_type: MediaType, centroid: &[f64]) -> f64 {
        self.cosine(tmdb_id, media_type, centroid).unwrap_or(0.0).max(0.0)
    }

    fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    fn row_vector(&self, row: usize) -> &[u8] {
        let start = HEADER_BYTES + row * self.dim;
        &self.vectors[start..start + self.dim]
    }

    fn labels_by_size(&self, buckets: &HashMap<u32, Vec<Entry>>) -> Vec<&str> {
        let mut labels: Vec<(&str, usize)> =
            buckets.iter().map(|(&name, entries)| (self.name(name), entries.len())).collect();
        labels.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        labels.into_iter().map(|(name, _)| name).collect()
    }

    fn ranked(
        &self,
        buckets: &HashMap<u32, Vec<Entry>>,
        label: &str,
        media_type: Option<MediaType>,
        min_confidence: f64,
        skip: usize,
        limit: usize,
    ) -> Vec<(u32, MediaType)> {
        let Some(bucket) =
            self.names.iter().position(|n| &**n == label).and_then(|id| buckets.get(&(id as u32)))
        else {
            return Vec::new();
        };
        bucket
            .iter()
            // Most confident first, so the first entry under the floor means every later one is too.
            .take_while(|e| e.confidence >= min_confidence)
            .filter_map(|e| {
                let record = &self.records[e.row as usize];
                let kind = record.media_type?;
                (media_type.is_none() || media_type == Some(kind)).then_some((record.tmdb_id, kind))
            })
            .skip(skip)
            .take(limit)
            .collect()
    }

    fn top_k(&self, k: usize, include: impl Fn(usize) -> bool, query: &[u8]) -> Vec<Neighbor> {
        let mut scored: Vec<(i32, u32)> = (0..self.records.len())
            .filter(|&row| include(row))
            .map(|row| (dot(query, self.row_vector(row)), row as u32))
            .collect();
        let order = |a: &(i32, u32), b: &(i32, u32)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
        if scored.len() > k {
            if k == 0 {
                return Vec::new();
            }
            scored.select_nth_unstable_by(k - 1, order);
            scored.truncate(k);
        }
        scored.sort_by(order);
        scored
            .into_iter()
            .filter_map(|(score, row)| {
                let record = &self.records[row as usize];
                Some(Neighbor { tmdb_id: record.tmdb_id, media_type: record.media_type?, score })
            })
            .collect()
    }
}

/// int8 · int8, accumulated in i32 (1024 dims × 127² fits with room to spare). The bytes are int8 stored as
/// u8, so each is reinterpreted before the multiply.
fn dot(a: &[u8], b: &[u8]) -> i32 {
    a.iter().zip(b).map(|(&x, &y)| i32::from(x as i8) * i32::from(y as i8)).sum()
}

/// The label → titles buckets for one label family. A record joins each label once, with the confidence of
/// its first occurrence; each bucket is stable-sorted by confidence, so ties keep record order.
fn buckets(records: &[Record], family: impl Fn(&Record) -> &[(u32, f64)]) -> HashMap<u32, Vec<Entry>> {
    let mut buckets: HashMap<u32, Vec<Entry>> = HashMap::new();
    for (row, record) in records.iter().enumerate() {
        let mut seen = HashSet::new();
        for &(name, confidence) in family(record) {
            if seen.insert(name) {
                buckets.entry(name).or_default().push(Entry { row: row as u32, confidence });
            }
        }
    }
    for bucket in buckets.values_mut() {
        bucket.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    }
    buckets
}

/// The vectors blob's dimension, checked against its header and the label count — without copying the rows.
fn vector_dimension(blob: &[u8], rows: usize) -> Result<usize, LoadError> {
    let header =
        |at: usize| -> Option<i32> { Some(i32::from_le_bytes(blob.get(at..at + 4)?.try_into().ok()?)) };
    let (Some(count), Some(dim)) = (header(0), header(4)) else {
        return Err(LoadError::Vectors("shorter than its header".to_owned()));
    };
    let (count, dim) = (usize::try_from(count).unwrap_or(usize::MAX), usize::try_from(dim).unwrap_or(0));
    if count != rows {
        return Err(LoadError::Vectors(format!("{count} rows for {rows} labelled titles")));
    }
    if dim == 0 || blob.len() != HEADER_BYTES + count * dim {
        return Err(LoadError::Vectors(format!("{} bytes don't hold {count} × {dim}", blob.len())));
    }
    Ok(dim)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One fixture title: id, type, primary genre, animated, subgenres, moods, vector.
    pub(crate) type Row<'a> =
        (u32, &'a str, &'a str, bool, &'a [(&'a str, f64)], &'a [(&'a str, f64)], [i8; 3]);

    /// A labels blob + vectors blob for a handful of titles, `dim` 3.
    pub(crate) fn fixture(records: &[Row<'_>]) -> Index {
        let json = serde_json::json!({
            "taxonomyVersion": "t02",
            "count": records.len(),
            "records": records.iter().map(|(id, kind, genre, animated, subs, moods, _)| serde_json::json!({
                "tmdbId": id, "mediaType": kind, "primaryGenre": genre, "animated": animated, "source": "llm",
                "subgenres": subs.iter().map(|(l, c)| serde_json::json!({"label": l, "confidence": c})).collect::<Vec<_>>(),
                "moods": moods.iter().map(|(l, c)| serde_json::json!({"label": l, "confidence": c})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        let mut vectors = Vec::new();
        vectors.extend_from_slice(&(records.len() as i32).to_le_bytes());
        vectors.extend_from_slice(&3i32.to_le_bytes());
        for record in records {
            vectors.extend(record.6.iter().map(|&v| v as u8));
        }
        Index::from_blobs(json.to_string().as_bytes(), vectors).unwrap()
    }

    fn sample() -> Index {
        fixture(&[
            (
                1,
                "movie",
                "Thriller",
                false,
                &[("Heist", 0.9), ("Survival", 0.5)],
                &[("Tense", 0.8)],
                [100, 0, 0],
            ),
            (2, "movie", "Thriller", false, &[("Heist", 0.95)], &[], [90, 40, 0]),
            (3, "tv", "Drama", false, &[("Heist", 0.7), ("Heist", 0.1)], &[("Tense", 0.6)], [100, 0, 0]),
            (4, "movie", "Comedy", false, &[("Survival", 0.6)], &[], [0, 100, 0]),
            (1, "tv", "Fantasy", true, &[], &[], [0, 0, 100]),
        ])
    }

    #[test]
    fn label_rows_are_most_confident_first_and_respect_the_floor_and_type() {
        let idx = sample();
        assert_eq!(
            idx.titles_with_subgenre("Heist", None, 0.55, 0, 10),
            vec![(2, MediaType::Movie), (1, MediaType::Movie), (3, MediaType::Tv)]
        );
        assert_eq!(
            idx.titles_with_subgenre("Heist", Some(MediaType::Tv), 0.55, 0, 10),
            vec![(3, MediaType::Tv)]
        );
        assert_eq!(idx.titles_with_subgenre("Survival", None, 0.55, 0, 10), vec![(4, MediaType::Movie)]);
        assert_eq!(idx.titles_with_subgenre("Heist", None, 0.0, 1, 1), vec![(1, MediaType::Movie)], "paging");
        assert!(idx.titles_with_mood("Nope", None, 0.0, 0, 10).is_empty());
    }

    #[test]
    fn a_label_repeated_on_one_title_counts_once_at_its_first_confidence() {
        let idx = sample();
        assert_eq!(idx.titles_with_subgenre("Heist", Some(MediaType::Tv), 0.0, 0, 10).len(), 1);
    }

    #[test]
    fn the_taxonomy_is_most_populated_first() {
        let idx = sample();
        assert_eq!(idx.subgenre_labels(), vec!["Heist", "Survival"]);
        assert_eq!(idx.mood_labels(), vec!["Tense"]);
    }

    #[test]
    fn ids_are_looked_up_with_their_type() {
        let idx = sample();
        assert_eq!(idx.labels(1, MediaType::Movie).unwrap().primary_genre, "Thriller");
        assert_eq!(idx.labels(1, MediaType::Tv).unwrap().primary_genre, "Fantasy");
        assert!(idx.labels(1, MediaType::Tv).unwrap().animated);
    }

    #[test]
    fn nearest_neighbours_stay_in_type_and_skip_the_title_itself() {
        let idx = sample();
        let near = idx.nearest(1, MediaType::Movie, 5);
        assert_eq!(near.iter().map(|n| n.tmdb_id).collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(near[0].score, 100 * 90);
        assert!(idx.nearest(99, MediaType::Movie, 5).is_empty());
    }

    #[test]
    fn a_vector_query_can_span_both_types() {
        let idx = sample();
        let near = idx.nearest_to_vector(&[0, 0, 100], None, 1);
        assert_eq!((near[0].tmdb_id, near[0].media_type), (1, MediaType::Tv));
        assert!(idx.nearest_to_vector(&[1, 2], None, 1).is_empty(), "another dimension is another space");
    }

    #[test]
    fn the_taste_centroid_promotes_similar_titles_and_never_demotes() {
        let idx = sample();
        let centroid = idx.centroid(&[(1, MediaType::Movie)]).unwrap();
        assert!((idx.cosine(1, MediaType::Movie, &centroid).unwrap() - 1.0).abs() < 1e-9);
        assert!(
            idx.taste_boost(2, MediaType::Movie, &centroid) > idx.taste_boost(4, MediaType::Movie, &centroid)
        );
        assert_eq!(idx.taste_boost(1, MediaType::Tv, &centroid), 0.0);
        assert!(idx.centroid(&[(99, MediaType::Movie)]).is_none());
    }

    #[test]
    fn a_vector_count_that_disagrees_with_the_labels_fails_the_load() {
        let json = br#"{"taxonomyVersion":"t02","records":[]}"#;
        let mut vectors = 1i32.to_le_bytes().to_vec();
        vectors.extend_from_slice(&3i32.to_le_bytes());
        vectors.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(Index::from_blobs(json, vectors), Err(LoadError::Vectors(_))));
        assert!(matches!(Index::from_blobs(b"nope", Vec::new()), Err(LoadError::Labels(_))));
    }
}
