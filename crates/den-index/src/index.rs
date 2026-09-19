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
/// The quantiser's scale: a unit vector's components were stored as `round(x × 127)`.
const QUANTUM: f64 = 127.0;

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

    /// Every primary genre and how many usable titles carry it, largest first. Empty genres and records with
    /// an unknown media type do not count as covered: neither can answer a query.
    pub fn primary_genre_counts(&self) -> Vec<(&str, usize)> {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for record in &self.records {
            if record.media_type.is_some() && !self.name(record.primary_genre).is_empty() {
                *counts.entry(record.primary_genre).or_default() += 1;
            }
        }
        self.named_counts(counts)
    }

    /// Every label in one family and the number of usable titles carrying it at or above `min_confidence`.
    /// This is the population a displayed label row actually draws from, rather than the number of raw guesses
    /// the producer happened to retain.
    pub fn subgenre_counts(&self, min_confidence: f64) -> Vec<(&str, usize)> {
        self.bucket_counts(&self.subgenres, min_confidence)
    }

    /// As `subgenre_counts`, for moods.
    pub fn mood_counts(&self, min_confidence: f64) -> Vec<(&str, usize)> {
        self.bucket_counts(&self.moods, min_confidence)
    }

    /// Titles with at least one usable value in a label family. A count without this denominator makes a thin
    /// classifier pass look like an exhaustive census.
    pub fn subgenre_coverage(&self, min_confidence: f64) -> usize {
        self.subgenre_coverage_for(None, min_confidence)
    }

    /// As `subgenre_coverage`, for moods.
    pub fn mood_coverage(&self, min_confidence: f64) -> usize {
        self.mood_coverage_for(None, min_confidence)
    }

    pub fn subgenre_coverage_for(&self, media_type: Option<MediaType>, min_confidence: f64) -> usize {
        self.family_coverage(|record| &record.subgenres, media_type, min_confidence)
    }

    pub fn mood_coverage_for(&self, media_type: Option<MediaType>, min_confidence: f64) -> usize {
        self.family_coverage(|record| &record.moods, media_type, min_confidence)
    }

    /// Usable titles by media type. `Index::len()` includes malformed/unknown type rows so it is not an honest
    /// coverage count for a type filter.
    pub fn media_type_counts(&self) -> [(MediaType, usize); 2] {
        let mut movie = 0;
        let mut tv = 0;
        for record in &self.records {
            match record.media_type {
                Some(MediaType::Movie) => movie += 1,
                Some(MediaType::Tv) => tv += 1,
                None => {}
            }
        }
        [(MediaType::Movie, movie), (MediaType::Tv, tv)]
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

    pub fn count_with_subgenre(
        &self,
        label: &str,
        media_type: Option<MediaType>,
        min_confidence: f64,
    ) -> usize {
        self.counted(&self.subgenres, label, media_type, min_confidence)
    }

    pub fn count_with_mood(&self, label: &str, media_type: Option<MediaType>, min_confidence: f64) -> usize {
        self.counted(&self.moods, label, media_type, min_confidence)
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

    /// Every indexed title, in row order; a duplicate row is left out, as lookups leave it out.
    pub fn titles(&self) -> impl Iterator<Item = (MediaType, u32)> + '_ {
        self.records.iter().enumerate().filter_map(|(row, record)| {
            let key = (record.media_type?, record.tmdb_id);
            (self.rows.get(&key) == Some(&(row as u32))).then_some(key)
        })
    }

    /// A title's row, for `similarity` and `projection`; `None` when it isn't indexed.
    pub fn row_of(&self, tmdb_id: u32, media_type: MediaType) -> Option<u32> {
        self.rows.get(&(media_type, tmdb_id)).copied()
    }

    /// Two rows' cosine similarity. The vectors were L2-normalised and quantised to ±127, so their int8 dot product
    /// over 127² is the cosine, give or take the rounding.
    pub fn similarity(&self, a: u32, b: u32) -> f64 {
        f64::from(dot(self.row_vector(a as usize), self.row_vector(b as usize))) / (QUANTUM * QUANTUM)
    }

    /// The mean of every row's vector, in `similarity`'s units: a row's `projection` on it is how close that title
    /// sits to the index as a whole.
    pub fn mean_vector(&self) -> Vec<f64> {
        let mut mean = vec![0.0; self.dim];
        for row in 0..self.records.len() {
            for (m, &v) in mean.iter_mut().zip(self.row_vector(row)) {
                *m += f64::from(v as i8);
            }
        }
        let scale = QUANTUM * self.records.len().max(1) as f64;
        mean.iter_mut().for_each(|m| *m /= scale);
        mean
    }

    /// A row's dot product with a vector in `similarity`'s units (`mean_vector`).
    pub fn projection(&self, row: u32, vector: &[f64]) -> f64 {
        self.row_vector(row as usize).iter().zip(vector).map(|(&v, &x)| f64::from(v as i8) * x).sum::<f64>()
            / QUANTUM
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

    fn named_counts(&self, counts: HashMap<u32, usize>) -> Vec<(&str, usize)> {
        let mut named: Vec<(&str, usize)> =
            counts.into_iter().map(|(name, count)| (self.name(name), count)).collect();
        named.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        named
    }

    fn bucket_counts(&self, buckets: &HashMap<u32, Vec<Entry>>, min_confidence: f64) -> Vec<(&str, usize)> {
        self.named_counts(
            buckets
                .iter()
                .map(|(&name, entries)| {
                    let count = entries
                        .iter()
                        .take_while(|entry| entry.confidence >= min_confidence)
                        .filter(|entry| self.records[entry.row as usize].media_type.is_some())
                        .count();
                    (name, count)
                })
                .filter(|(_, count)| *count > 0)
                .collect(),
        )
    }

    fn family_coverage(
        &self,
        family: impl Fn(&Record) -> &[(u32, f64)],
        media_type: Option<MediaType>,
        min_confidence: f64,
    ) -> usize {
        self.records
            .iter()
            .filter(|record| {
                record.media_type.is_some()
                    && (media_type.is_none() || record.media_type == media_type)
                    && family(record).iter().any(|(_, score)| *score >= min_confidence)
            })
            .count()
    }

    fn counted(
        &self,
        buckets: &HashMap<u32, Vec<Entry>>,
        label: &str,
        media_type: Option<MediaType>,
        min_confidence: f64,
    ) -> usize {
        let Some(bucket) =
            self.names.iter().position(|name| &**name == label).and_then(|id| buckets.get(&(id as u32)))
        else {
            return 0;
        };
        bucket
            .iter()
            .take_while(|entry| entry.confidence >= min_confidence)
            .filter(|entry| {
                let kind = self.records[entry.row as usize].media_type;
                kind.is_some() && (media_type.is_none() || kind == media_type)
            })
            .count()
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

    /// The `k` titles nearest to an outside vector among those `include` admits, best first, and the mean and
    /// standard deviation of every admitted title's score — so a caller can read a score against this query's own
    /// spread instead of calibrating raw dot products, which shift with the query. Empty on a dimension mismatch.
    pub fn scan_vector(
        &self,
        query: &[i8],
        include: impl Fn(u32, MediaType) -> bool,
        k: usize,
    ) -> (Vec<Neighbor>, ScanStats) {
        if query.len() != self.dim {
            return (Vec::new(), ScanStats::default());
        }
        let query: Vec<u8> = query.iter().map(|&v| v as u8).collect();
        let scored = self.scores(
            |row| {
                let record = &self.records[row];
                record.media_type.is_some_and(|kind| include(record.tmdb_id, kind))
            },
            &query,
        );
        let stats = ScanStats::of(&scored);
        (self.best(scored, k), stats)
    }

    fn top_k(&self, k: usize, include: impl Fn(usize) -> bool, query: &[u8]) -> Vec<Neighbor> {
        self.best(self.scores(include, query), k)
    }

    fn scores(&self, include: impl Fn(usize) -> bool, query: &[u8]) -> Vec<(i32, u32)> {
        (0..self.records.len())
            .filter(|&row| include(row))
            .map(|row| (dot(query, self.row_vector(row)), row as u32))
            .collect()
    }

    fn best(&self, mut scored: Vec<(i32, u32)>, k: usize) -> Vec<Neighbor> {
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

/// The spread of one scan's scores: their mean and standard deviation, zero when nothing was scanned.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanStats {
    pub mean: f64,
    pub sd: f64,
}

impl ScanStats {
    fn of(scored: &[(i32, u32)]) -> ScanStats {
        if scored.is_empty() {
            return ScanStats::default();
        }
        let n = scored.len() as f64;
        let mean = scored.iter().map(|&(s, _)| f64::from(s)).sum::<f64>() / n;
        let variance = scored.iter().map(|&(s, _)| (f64::from(s) - mean).powi(2)).sum::<f64>() / n;
        ScanStats { mean, sd: variance.sqrt() }
    }
}

/// int8 · int8, accumulated in i32 (1024 dims × 128² fits with room to spare). The bytes are int8 stored as
/// u8, so each is reinterpreted before the multiply. Every scan is this ~38k times over, so on a CPU with AVX2
/// it runs sixteen dimensions at a time; the image stays portable, since the check is made where it runs.
fn dot(a: &[u8], b: &[u8]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU supports AVX2, checked just above.
        return unsafe { dot_avx2(a, b) };
    }
    dot_scalar(a, b)
}

fn dot_scalar(a: &[u8], b: &[u8]) -> i32 {
    a.iter().zip(b).map(|(&x, &y)| i32::from(x as i8) * i32::from(y as i8)).sum()
}

/// `dot` sixteen bytes at a time: both sides sign-extended to 16 bits, multiplied and added pairwise into 32 bits
/// (`madd`), the eight lanes summed at the end, and a tail under sixteen bytes done one at a time.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_avx2(a: &[u8], b: &[u8]) -> i32 {
    use std::arch::x86_64::{
        __m256i, _mm256_add_epi32, _mm256_cvtepi8_epi16, _mm256_madd_epi16, _mm256_setzero_si256,
        _mm256_storeu_si256, _mm_loadu_si128,
    };
    let n = a.len().min(b.len());
    let mut sum = _mm256_setzero_si256();
    let mut at = 0;
    while at + 16 <= n {
        // SAFETY: `at + 16 <= n`, and both slices hold at least `n` bytes; `loadu` reads unaligned.
        let (x, y) = unsafe {
            (_mm_loadu_si128(a.as_ptr().add(at).cast()), _mm_loadu_si128(b.as_ptr().add(at).cast()))
        };
        sum = _mm256_add_epi32(sum, _mm256_madd_epi16(_mm256_cvtepi8_epi16(x), _mm256_cvtepi8_epi16(y)));
        at += 16;
    }
    let mut lanes = [0i32; 8];
    // SAFETY: `lanes` is 32 bytes, the width of one __m256i; `storeu` writes unaligned.
    unsafe { _mm256_storeu_si256(lanes.as_mut_ptr().cast::<__m256i>(), sum) };
    lanes.iter().fold(0i32, |total, &lane| total.wrapping_add(lane)) + dot_scalar(&a[at..n], &b[at..n])
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

    #[test]
    fn the_simd_dot_product_matches_the_scalar_one() {
        let bytes = |multiplier: u32, n: usize| -> Vec<u8> {
            (0..n as u32)
                .map(|i| (i.wrapping_mul(multiplier).wrapping_add(0x9e37_79b9) >> 11) as u8)
                .collect()
        };
        for n in [0, 1, 15, 16, 17, 1024, 1027] {
            let (a, b) = (bytes(2_654_435_761, n), bytes(40_503, n));
            assert_eq!(dot(&a, &b), dot_scalar(&a, &b), "{n} dimensions");
        }
        // -128 everywhere: the largest product there is, 1024 times over.
        let extreme = vec![0x80u8; 1024];
        assert_eq!(dot(&extreme, &extreme), 1024 * 16384);
    }

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
    fn a_scan_ranks_only_what_it_admits_and_reports_their_spread() {
        let idx = sample();
        let (near, stats) = idx.scan_vector(&[100, 0, 0], |_, kind| kind == MediaType::Movie, 2);
        assert_eq!(near.iter().map(|n| n.tmdb_id).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(near[0].score, 10_000);
        // Over the three films admitted: 10000, 9000 and 0.
        assert!((stats.mean - 19_000.0 / 3.0).abs() < 1e-9);
        let variance =
            [10_000.0f64, 9_000.0, 0.0].iter().map(|s| (s - stats.mean).powi(2)).sum::<f64>() / 3.0;
        assert!((stats.sd - variance.sqrt()).abs() < 1e-9);
        assert_eq!(
            idx.scan_vector(&[1, 2], |_, _| true, 5),
            (Vec::new(), ScanStats::default()),
            "another space"
        );
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
