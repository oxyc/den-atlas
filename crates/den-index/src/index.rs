//! One index: the labels and the int8 vectors of one embedding space, read out of the store
//! (den-spec `wire/store-v1.md`). The dataset has two spaces — plot and premise — and each is an `Index`;
//! nothing here mixes them.
//!
//! Vectors are held in the layout the old `vectors-*.bin` blob had — a little-endian `[i32 count][i32 dim]`
//! header, then `count × dim` int8 rows in record order — because every scorer below addresses them that
//! way. The blob itself is no longer read: [`Index::from_blobs`] is kept only for the tests that hold the
//! store reader to what the blobs answered.

use crate::MediaType;
/// Only the blob reader below parses JSON, and only tests call it — so serde is a dev-dependency here
/// and this crate's serving path has no deserialiser in it at all.
#[cfg(test)]
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::OnceLock;

/// The confidence a label needs to be shown in a row — the tvOS app's `displayConfidenceFloor`. The producer
/// keeps weaker labels as review material, but a row built from them puts the classifier's least confident
/// guesses in front of the viewer.
pub const DISPLAY_CONFIDENCE_FLOOR: f64 = 0.55;
const HEADER_BYTES: usize = 8;
/// The quantiser's scale: a unit vector's components were stored as `round(x × 127)`.
const QUANTUM: f64 = 127.0;

/// The direction in the plot space (bge-m3, 1024 dimensions) along which a vector moves with the length of
/// the plot it was embedded from: 1024 little-endian `f32`, unit length, pointing towards longer plots.
///
/// Short plots pull short plots and long ones long (oxyc/den-dataset#109): on store `b2c60751c955` a seed
/// under 400 characters has 78% of its plot top 20 under 1,000 characters, a seed over 2,500 has 2.5%,
/// against a corpus rate of 22%. Most of that is this one direction. A vector's projection on it correlates
/// 0.87 with log plot length.
///
/// Fitted offline, because the store carries no plot text and nothing in it stands in for its length. The
/// best proxy there is (distributor count, correlated 0.36 with log length) gives a direction at cosine 0.86
/// to this one, whose projection correlates only 0.74 with length. A canonical correlation over every credit
/// list's count finds popularity instead (cosine 0.18). The fit: the least-squares slope of the unit plot
/// vectors on `ln(plot characters)`, over the 33,657 titles whose plot is English, the plot counted as it was
/// embedded (after translation and the 3,500-character cap), then normalised.
///
/// It belongs to the embedding model and the way documents are composed, not to one store: a store embedded
/// by another model has another space, and a direction of another dimension is not applied at all.
const PLOT_LENGTH_DIRECTION: &[u8] = include_bytes!("plot-length-direction.f32");

fn plot_length_direction() -> Box<[f32]> {
    PLOT_LENGTH_DIRECTION.as_chunks::<4>().0.iter().map(|&b| f32::from_le_bytes(b)).collect()
}

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

#[cfg(test)]
#[derive(Deserialize)]
struct LabelsArtifact {
    #[serde(rename = "taxonomyVersion")]
    taxonomy_version: String,
    records: Vec<RawRecord>,
}

#[cfg(test)]
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

#[cfg(test)]
#[derive(Deserialize)]
struct RawLabel {
    label: String,
    confidence: f64,
}

/// A record with its label and genre names interned: the same few hundred names repeat across ~37k titles.
#[derive(Clone)]
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

/// Which of the store's two vector matrices an index is built over.
///
/// Private on purpose: the public way in is [`Index::from_store_plot`] or [`Index::from_store_premise`],
/// so no call site has an argument to get the wrong way round. Its one job is to name the values section
/// and its `_has` column TOGETHER — pairing `vec_premise` with `vec_plot_has` would index the right
/// vectors for the wrong rows and nothing downstream could tell.
#[derive(Clone, Copy)]
enum Space {
    Plot,
    Premise,
}

impl Space {
    /// (values, `_has`) — always as a pair, never separately.
    fn sections(self) -> (&'static str, &'static str) {
        match self {
            Space::Plot => ("vec_plot", "vec_plot_has"),
            Space::Premise => ("vec_premise", "vec_premise_has"),
        }
    }
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

/// The store's label sections, resolved once: the one reader of them.
///
/// `Index` builds its records through this, and a caller that ranks without an `Index` — one holding the
/// label columns but no vectors — reads labels through it too, so the two cannot disagree about what a
/// title's labels are.
pub struct LabelColumns<'a> {
    keys: &'a [u64],
    strings: den_store::Strings<'a>,
    primary_genre: &'a [u32],
    animated: &'a [u8],
    // Values and confidences share one offsets array, so row i owns the same span in both.
    subgenre_v: den_store::List<'a, u32>,
    subgenre_c: den_store::List<'a, u8>,
    mood_v: den_store::List<'a, u32>,
    mood_c: den_store::List<'a, u8>,
}

impl<'a> LabelColumns<'a> {
    pub fn new(store: &den_store::Store<'a>) -> Result<Self, den_store::StoreError> {
        Ok(LabelColumns {
            keys: store.per_row::<u64>("keys")?,
            strings: store.strings()?,
            primary_genre: store.per_row::<u32>("primary_genre")?,
            animated: store.per_row::<u8>("animated")?,
            subgenre_v: store.list::<u32>("subgenre_v", "subgenre_o")?,
            subgenre_c: store.list::<u8>("subgenre_c", "subgenre_o")?,
            mood_v: store.list::<u32>("mood_v", "mood_o")?,
            mood_c: store.list::<u8>("mood_c", "mood_o")?,
        })
    }

    /// A title's labels; `None` when the store does not hold it.
    pub fn of(&self, tmdb_id: u32, media_type: MediaType) -> Option<Labels<'a>> {
        let media = u64::from(media_type == MediaType::Tv);
        let row = self.keys.binary_search(&((media << 32) | u64::from(tmdb_id))).ok()?;
        Some(self.labels(den_store::Row(row)))
    }

    /// A row's labels. Confidence is stored in hundredths, so `57u8 as f64 / 100.0` is bit-identical to
    /// parsing `"0.57"`.
    fn labels(&self, row: den_store::Row) -> Labels<'a> {
        // An id the dictionary cannot resolve is dropped, never read as "": a nameless label would collide
        // with the absent primary genre and become a bucket nothing can ask for.
        let scored = |values: &'a [u32], confidences: &'a [u8]| -> Vec<(&'a str, f64)> {
            values
                .iter()
                .zip(confidences)
                .filter_map(|(&value, &confidence)| {
                    Some((self.strings.get(value)?, f64::from(confidence) / 100.0))
                })
                .collect()
        };
        Labels {
            // Absent is "", as it is in the blob, where `primaryGenre` defaults to the empty string.
            primary_genre: self
                .primary_genre
                .get(row.0)
                .and_then(|&id| self.strings.get(id))
                .unwrap_or_default(),
            animated: self.animated.get(row.0).is_some_and(|&a| a != 0),
            subgenres: scored(self.subgenre_v.get(row), self.subgenre_c.get(row)),
            moods: scored(self.mood_v.get(row), self.mood_c.get(row)),
        }
    }
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
    /// The length direction to remove for `without_length`: the plot index's, when its dimension fits;
    /// `None` for the premise index, which is another space.
    length_direction: Option<Box<[f32]>>,
    without_length: OnceLock<Box<Index>>,
}

impl Index {
    /// Build from the two blobs. Fails when either doesn't parse or the vector count disagrees with the labels.
    ///
    /// **Tests only.** Serving reads the store; `labels-tNN.json` and `vectors-*.bin` are no longer
    /// opened by anything that runs in the binary. It stays behind `#[cfg(test)]` rather than being
    /// deleted because two tests need a second implementation to be worth anything: `tests::fixture`,
    /// which builds the tiny in-memory indexes every unit test here and in `similar.rs` runs on, and
    /// `the_store_answers_what_the_blobs_did`, which is the evidence that the store reader answers what
    /// the blob reader did on the real corpus. A deleted reader cannot disagree with anything.
    #[cfg(test)]
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
        Ok(assemble(artifact.taxonomy_version, dim, records, names, vectors))
    }

    /// The plot index, out of the store — den-spec `wire/store-v1.md` — instead of the two blobs. Same
    /// `Index`, same answers; the labels and the plot vectors are already in there, columnar and mmap'd,
    /// so reading them again out of `labels-tNN.json` + `vectors-bge-m3.bin` is 60 MB of artifact and a
    /// JSON parse for signals the process has already mapped.
    ///
    /// See [`from_store_space`](Index::from_store_space) for what the move has to reconcile.
    pub fn from_store_plot(store: &den_store::Store<'_>) -> Result<Index, LoadError> {
        Index::from_store_space(store, Space::Plot)
    }

    /// The premise index, out of the store: `labels-premise.json` + `vectors-premise.bin`, which cover the
    /// 44,531 titles that got a premise embedding rather than the plot index's 47,539.
    ///
    /// A SEPARATE constructor rather than a `Space` argument, and the enum that picks the sections is
    /// private, so there is no argument at a call site to get the wrong way round. The two indexes are
    /// **different embedding spaces** — a cosine between them is meaningless, not merely inaccurate — and
    /// a wrong argument here would build a thing that answers confidently and wrongly forever. The one
    /// value inside also names BOTH sections together, so `vec_premise` can never be paired with
    /// `vec_plot_has`, which is the mistake that would silently index the right vectors for the wrong rows.
    ///
    /// The taxonomy version is shared: both blobs declare the same `taxonomyVersion` (`t02` in the
    /// generation this was written against), because the premise labels are the same labelling pass,
    /// restricted to the titles that have a premise vector. So the caller stamps both indexes from the
    /// same manifest field.
    pub fn from_store_premise(store: &den_store::Store<'_>) -> Result<Index, LoadError> {
        Index::from_store_space(store, Space::Premise)
    }

    /// One body for both spaces, so the plot and premise paths cannot drift.
    ///
    /// Four things the move has to reconcile, each of which is a way to be quietly wrong:
    ///
    /// **Which rows.** The store is the whole corpus; this is a VECTOR index. A row whose `_has` column is
    /// 0 has no vector in this space, and a zero vector is not a missing one — it dots to 0 against every
    /// query, so it would rank ahead of every genuinely dissimilar title in More Like This. Such a row is
    /// left out. On the corpus this was written against, `vec_plot_has` selects exactly the 47,539 titles
    /// of `labels-t02.json` and `vec_premise_has` exactly the 44,531 of `labels-premise.json` — the key
    /// sets match both ways, and `vec_premise`'s all-zero rows agree with `vec_premise_has` exactly.
    ///
    /// **Confidence.** The blob carries `f64`, the store `u8` hundredths, and `57u8 as f64 / 100.0` is
    /// bit-identical to parsing `"0.57"` — which is why the writer refuses a probability with more than
    /// two decimals. The blob is not held to that: 799 of its confidence values, over 793 titles, are a
    /// float-arithmetic residue such as `0.7999999999999999`, one ULP below the 0.80 the store keeps —
    /// the same 799 in both label blobs. The store is the one telling the truth about what the model said,
    /// and this is a visible change, not a rounding detail: `plotrows::label_confidence` tiers a label at
    /// `>= 0.8`, and that tier is the primary sort key of every subgenre and mood browse row, so the 791
    /// labels written `0.7999999999999999` move from tier 2 to tier 3 and rise within their row.
    ///
    /// **Row order.** `keys` is sorted ascending — every movie by tmdb id, then every series — where the
    /// blob kept the producer's record order. Nothing reads row numbers from outside, but order is the tie
    /// break in two places: `buckets` is stable, so labels of equal confidence come out in row order, and
    /// `best` gives an equal score to the earlier row. Both orders are arbitrary; this one is at least a
    /// property of the data rather than of whichever pass wrote the file.
    ///
    /// **Names.** The store's string table has its own ids, over the whole corpus. They are re-interned
    /// here in row order, so `names` stays this index's own dense id space — the one the bucket keys and
    /// `primary_genre` index.
    ///
    /// The store carries no `taxonomyVersion` (it is a property of the labelling pass, not of the corpus),
    /// so it starts empty; the caller stamps it from the manifest with [`with_taxonomy_version`].
    ///
    /// # A known loss, on the premise side only
    ///
    /// The store has ONE set of label sections and the writer fills them from the plot labels pass alone.
    /// Three titles in the current generation have a premise vector and premise labels but no plot labels
    /// — movie 51870, movie 121329, tv 42680 — so the premise index built here answers no primary genre,
    /// no subgenres and no moods for them where the blob answered all three. It is three of 44,531 and the
    /// fix belongs in den-dataset's `build_store.py` (fill the label sections from the UNION of both
    /// passes); the parity test below bounds it so it cannot grow unnoticed.
    ///
    /// [`with_taxonomy_version`]: Index::with_taxonomy_version
    fn from_store_space(store: &den_store::Store<'_>, space: Space) -> Result<Index, LoadError> {
        let (vector_section, has_section) = space.sections();
        let labelled = |e: den_store::StoreError| LoadError::Labels(e.to_string());
        let vectored = |e: den_store::StoreError| LoadError::Vectors(e.to_string());
        let columns = LabelColumns::new(store).map_err(labelled)?;
        let keys = columns.keys;
        let has_vector = store.per_row::<u8>(has_section).map_err(vectored)?;
        let space_vectors = store.column::<i8>(vector_section).map_err(vectored)?;

        // Read off the section rather than assumed to be 1024: a dimension is a property of the embedding
        // model, and a build that hardcodes one silently misreads the first store written by another.
        let dim = if keys.is_empty() { 0 } else { space_vectors.len() / keys.len() };
        if dim == 0 || space_vectors.len() != keys.len() * dim {
            return Err(LoadError::Vectors(format!(
                "{vector_section} holds {} bytes for {} rows",
                space_vectors.len(),
                keys.len()
            )));
        }

        let mut names: Vec<Box<str>> = Vec::new();
        let mut name_ids: HashMap<String, u32> = HashMap::new();
        let mut intern = |name: String| -> u32 {
            *name_ids.entry(name).or_insert_with_key(|name| {
                names.push(name.as_str().into());
                (names.len() - 1) as u32
            })
        };
        let kept = has_vector.iter().filter(|&&has| has != 0).count();
        let mut records = Vec::with_capacity(kept);
        let mut vectors = Vec::with_capacity(HEADER_BYTES + kept * dim);
        // The header the blob carries and every reader of `self.vectors` assumes; the count is written once
        // the rows are known, and `vector_dimension` below re-reads it rather than trusting this.
        vectors.extend_from_slice(&0i32.to_le_bytes());
        vectors.extend_from_slice(&0i32.to_le_bytes());
        for (i, &packed) in keys.iter().enumerate() {
            if has_vector[i] == 0 {
                continue;
            }
            let labels = columns.labels(den_store::Row(i));
            let mut interned = |pairs: &[(&str, f64)]| -> Box<[(u32, f64)]> {
                pairs.iter().map(|&(name, confidence)| (intern(name.to_owned()), confidence)).collect()
            };
            let subgenres = interned(&labels.subgenres);
            let moods = interned(&labels.moods);
            records.push(Record {
                tmdb_id: packed as u32,
                media_type: Some(if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie }),
                primary_genre: intern(labels.primary_genre.to_owned()),
                animated: labels.animated,
                subgenres,
                moods,
            });
            vectors.extend(space_vectors[i * dim..(i + 1) * dim].iter().map(|&v| v as u8));
        }
        let count = i32::try_from(records.len())
            .map_err(|_| LoadError::Vectors(format!("{} rows do not fit a blob header", records.len())))?;
        let width = i32::try_from(dim)
            .map_err(|_| LoadError::Vectors(format!("{dim} dimensions do not fit a blob header")))?;
        vectors[..4].copy_from_slice(&count.to_le_bytes());
        vectors[4..8].copy_from_slice(&width.to_le_bytes());
        let dim = vector_dimension(&vectors, records.len())?;
        let mut index = assemble(String::new(), dim, records, names, vectors);
        if matches!(space, Space::Plot) {
            index.length_direction = Some(plot_length_direction()).filter(|d| d.len() == dim);
        }
        Ok(index)
    }

    /// Whether `without_length` has a direction to remove: true for a plot index of the dimension the
    /// shipped direction was fitted in.
    pub fn has_length_direction(&self) -> bool {
        self.length_direction.is_some()
    }

    /// This index with the plot-length direction (`PLOT_LENGTH_DIRECTION`) projected out of every vector:
    /// each row is read as a unit vector, loses its component along the direction, and is re-normalised and
    /// requantised to int8, so every scorer below reads it exactly as it reads the original. A seed is a row
    /// of the same index, so the seed and the corpus it is compared with lose the direction alike.
    ///
    /// Built on first use and kept with the index: a second copy of the vectors (~49 MB on the plot index)
    /// that nothing pays for until something asks. An index with no direction answers itself.
    pub fn without_length(&self) -> &Index {
        let Some(direction) = &self.length_direction else { return self };
        self.without_length.get_or_init(|| Box::new(self.with_direction_removed(direction)))
    }

    /// A fixture index given a length direction, as the store path gives the plot index one.
    #[cfg(test)]
    pub(crate) fn with_length_direction(mut self, direction: &[f32]) -> Index {
        assert_eq!(direction.len(), self.dim);
        self.length_direction = Some(direction.into());
        self
    }

    fn with_direction_removed(&self, direction: &[f32]) -> Index {
        let mut vectors = self.vectors[..HEADER_BYTES].to_vec();
        vectors.reserve(self.records.len() * self.dim);
        let mut unit = vec![0.0f64; self.dim];
        for row in 0..self.records.len() {
            for (x, &v) in unit.iter_mut().zip(self.row_vector(row)) {
                *x = f64::from(v as i8);
            }
            let norm = unit.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm > 0.0 {
                unit.iter_mut().for_each(|x| *x /= norm);
                let along: f64 = unit.iter().zip(direction).map(|(x, &d)| x * f64::from(d)).sum();
                unit.iter_mut().zip(direction).for_each(|(x, &d)| *x -= along * f64::from(d));
            }
            // A row that WAS the direction has nothing left; it stays the zero vector it now is.
            let norm = unit.iter().map(|x| x * x).sum::<f64>().sqrt();
            let scale = if norm > 0.0 { QUANTUM / norm } else { 0.0 };
            vectors.extend(unit.iter().map(|x| (x * scale).round().clamp(-QUANTUM, QUANTUM) as i8 as u8));
        }
        Index {
            taxonomy_version: self.taxonomy_version.clone(),
            dim: self.dim,
            records: self.records.clone(),
            names: self.names.clone(),
            rows: self.rows.clone(),
            vectors,
            subgenres: self.subgenres.clone(),
            moods: self.moods.clone(),
            length_direction: None,
            without_length: OnceLock::new(),
        }
    }

    /// Stamp the taxonomy version an index built from the store has no way to know — it is the labelling
    /// pass's version, and it lives in the dataset manifest, not in the corpus.
    pub fn with_taxonomy_version(mut self, version: impl Into<String>) -> Index {
        self.taxonomy_version = version.into();
        self
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

    /// `nearest` for each type in one scan: the title's own type first, then the other, each with its `k`
    /// nearest (the own type's list is exactly `nearest`'s) and the mean and standard deviation of every
    /// title of that type's cosine to it — what a cross-type cosine is read against (`similar.rs`). Empty
    /// when the title isn't indexed.
    pub fn nearest_by_type(&self, tmdb_id: u32, media_type: MediaType, k: usize) -> Vec<TypeNeighbours> {
        let Some(&query_row) = self.rows.get(&(media_type, tmdb_id)) else { return Vec::new() };
        let query = self.row_vector(query_row as usize);
        let other = match media_type {
            MediaType::Movie => MediaType::Tv,
            MediaType::Tv => MediaType::Movie,
        };
        [media_type, other]
            .into_iter()
            .map(|kind| {
                let scored = self.scores(
                    |row| row != query_row as usize && self.records[row].media_type == Some(kind),
                    query,
                );
                let raw = ScanStats::of(&scored);
                let unit = QUANTUM * QUANTUM;
                TypeNeighbours {
                    media_type: kind,
                    stats: ScanStats { mean: raw.mean / unit, sd: raw.sd / unit },
                    nearest: self.best(scored, k),
                }
            })
            .collect()
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

/// One type's answer in `Index::nearest_by_type`.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeNeighbours {
    pub media_type: MediaType,
    pub nearest: Vec<Neighbor>,
    /// Every title of this type's cosine to the query, in `similarity`'s units.
    pub stats: ScanStats,
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

/// The parts every constructor ends the same way: the (type, id) → row map and the two label buckets.
/// Shared, so the store path and the blob path cannot come to index the same records differently.
fn assemble(
    taxonomy_version: String,
    dim: usize,
    records: Vec<Record>,
    names: Vec<Box<str>>,
    vectors: Vec<u8>,
) -> Index {
    let mut rows = HashMap::with_capacity(records.len());
    for (row, record) in records.iter().enumerate() {
        if let Some(media_type) = record.media_type {
            rows.entry((media_type, record.tmdb_id)).or_insert(row as u32);
        }
    }
    let subgenres = buckets(&records, |r| &r.subgenres);
    let moods = buckets(&records, |r| &r.moods);
    Index {
        taxonomy_version,
        dim,
        records,
        names,
        rows,
        vectors,
        subgenres,
        moods,
        length_direction: None,
        without_length: OnceLock::new(),
    }
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

    /// The shipped direction is bge-m3's 1024 dimensions at unit length; a fixture of another dimension
    /// would not be given it.
    #[test]
    fn the_plot_length_direction_is_a_unit_vector_in_the_plot_space() {
        let direction = plot_length_direction();
        assert_eq!(direction.len(), 1024);
        let norm = direction.iter().map(|&d| f64::from(d) * f64::from(d)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm {norm}");
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

    /// Two label lists are the same when they name the same labels in the same order at the same
    /// confidence — **to the hundredth**, which is the precision the store keeps and the only precision
    /// the producer is held to. The blob is not held to it: 799 of its confidence values, over 793 titles,
    /// are written a ULP below the hundredth the model gave — `0.7999999999999999` 791 times, and
    /// `0.6799999999999999` and `0.9199999999999999` four times each, in BOTH label blobs. Comparing those
    /// as `f64` would report a difference that exists in the JSON's float formatting, not in the data.
    fn same_labels(got: &[(&str, f64)], want: &[(&str, f64)]) -> bool {
        got.len() == want.len()
            && got.iter().zip(want).all(|(a, b)| a.0 == b.0 && (a.1 * 100.0).round() == (b.1 * 100.0).round())
    }

    /// Does the store hold no label at all for this title? The three known losses below are all of this
    /// shape — the store's label sections are written from the plot pass alone, so a title labelled only
    /// by the premise pass arrives blank rather than wrong.
    fn unlabelled(got: &Labels<'_>) -> bool {
        got.primary_genre.is_empty() && got.subgenres.is_empty() && got.moods.is_empty() && !got.animated
    }

    /// One space compared both ways. Returns the titles that differ for a reason other than the known,
    /// bounded loss, and the ones that hit that loss.
    fn compare(
        space: &str,
        from_store: &Index,
        from_blobs: &Index,
    ) -> (Vec<String>, Vec<(MediaType, u32)>, usize, usize) {
        let mut differ = Vec::new();
        let mut blank = Vec::new();
        let (mut quantised, mut compared) = (0usize, 0usize);
        for (media, id) in from_blobs.titles() {
            compared += 1;
            let want = from_blobs.labels(id, media).expect("blob labels");
            let (Some(got), Some(store_row), Some(blob_row)) =
                (from_store.labels(id, media), from_store.row_of(id, media), from_blobs.row_of(id, media))
            else {
                differ.push(format!("[{space}] {media:?}:{id} missing from the store"));
                continue;
            };
            // Name the FIELD that differs, not two Debug dumps to eyeball.
            let mut fields = Vec::new();
            if got.primary_genre != want.primary_genre {
                fields.push("primary_genre");
            }
            if got.animated != want.animated {
                fields.push("animated");
            }
            if !same_labels(&got.subgenres, &want.subgenres) {
                fields.push("subgenres");
            }
            if !same_labels(&got.moods, &want.moods) {
                fields.push("moods");
            }
            if got.subgenres != want.subgenres || got.moods != want.moods {
                quantised += 1;
            }
            // The vector is compared regardless: a blank-labelled title must still carry the right bytes.
            if from_store.row_vector(store_row as usize) != from_blobs.row_vector(blob_row as usize) {
                fields.push("vector");
            }
            if fields.is_empty() {
                continue;
            }
            if fields == ["vector"] || !unlabelled(&got) {
                differ.push(format!("[{space}] {media:?}:{id} differs in {}", fields.join(", ")));
            } else {
                blank.push((media, id));
            }
        }
        // A title the store indexes and the blobs do not would not show up above, and would mean the two
        // are describing different corpora however well the shared titles agree.
        for (media, id) in from_store.titles() {
            if from_blobs.labels(id, media).is_none() {
                differ.push(format!("[{space}] {media:?}:{id} is in the store index and not the blobs"));
            }
        }
        (differ, blank, quantised, compared)
    }

    /// The taxonomy itself, not just the per-title answers: the bucket keys are a different id space on
    /// each side, and a mis-interned name would still answer every title correctly while leaving the label
    /// rows pointing at the wrong buckets.
    ///
    /// `allowance` is how many titles the store is known to hold no labels for (the premise-only loss).
    /// The vocabulary is compared as a SET rather than in population order, because a bucket one title
    /// smaller can reorder equally-sized labels — which says nothing — while a label disappearing
    /// altogether says a great deal.
    fn same_taxonomy(space: &str, from_store: &Index, from_blobs: &Index, allowance: usize) {
        for (family, mut a, mut b) in [
            ("subgenre", from_store.subgenre_labels(), from_blobs.subgenre_labels()),
            ("mood", from_store.mood_labels(), from_blobs.mood_labels()),
        ] {
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "{space} {family} vocabulary");
        }
        // Unaffected by the loss: a blank-labelled title is still an indexed title of its own type.
        assert_eq!(from_store.media_type_counts(), from_blobs.media_type_counts(), "{space} type counts");
        let floor = DISPLAY_CONFIDENCE_FLOOR;
        for (what, a, b) in [
            ("subgenre coverage", from_store.subgenre_coverage(floor), from_blobs.subgenre_coverage(floor)),
            ("mood coverage", from_store.mood_coverage(floor), from_blobs.mood_coverage(floor)),
        ] {
            assert!(
                a.abs_diff(b) <= allowance,
                "{space} {what}: store {a} vs blobs {b}, more than the {allowance} titles the store \
                 cannot label"
            );
        }
    }

    /// The store path against the blob path, on the REAL artifacts, for BOTH spaces. Opt-in via
    /// `DEN_STORE` + `DEN_LABELS` + `DEN_VECTORS` + `DEN_PREMISE_LABELS` + `DEN_PREMISE_VECTORS`.
    ///
    /// This is the test that matters for the migration: the fixture tests above cannot see a difference
    /// between the two readers, because they only ever run the blob one. Each `from_store_*` has to answer
    /// the same for every title — primary genre, animated, both label families with their confidences, and
    /// the vector byte for byte — or label rows, More Like This and the taste tilt all change with nothing
    /// failing. Both spaces, because the premise index is the half a plot-only test would have let
    /// through.
    ///
    /// It is the reason `from_blobs` still exists at all: serving no longer opens a blob, so this is the
    /// only thing left that can disagree with the store reader. It needs the real artifacts, which is why
    /// it is opt-in — and why it skips rather than fails when they are not to hand.
    #[test]
    fn the_store_answers_what_the_blobs_did() {
        let (
            Ok(store_path),
            Ok(labels_path),
            Ok(vectors_path),
            Ok(premise_labels_path),
            Ok(premise_vectors_path),
        ) = (
            std::env::var("DEN_STORE"),
            std::env::var("DEN_LABELS"),
            std::env::var("DEN_VECTORS"),
            std::env::var("DEN_PREMISE_LABELS"),
            std::env::var("DEN_PREMISE_VECTORS"),
        )
        else {
            eprintln!(
                "SKIP: set DEN_STORE, DEN_LABELS, DEN_VECTORS, DEN_PREMISE_LABELS and \
                 DEN_PREMISE_VECTORS to compare the two readers"
            );
            return;
        };
        // `read`, not mmap: `memmap2` compiles for neither wasm32 nor tvOS, and this crate has to.
        let bytes = std::fs::read(&store_path).expect("read the store");
        let store = den_store::Store::open(&bytes).expect("open the store");
        let blobs = |labels: &str, vectors: &str| {
            Index::from_blobs(
                &std::fs::read(labels).expect("read the labels"),
                std::fs::read(vectors).expect("read the vectors"),
            )
            .expect("index from the blobs")
        };
        let plot = (
            Index::from_store_plot(&store).expect("plot from the store"),
            blobs(&labels_path, &vectors_path),
        );
        let premise = (
            Index::from_store_premise(&store).expect("premise from the store"),
            blobs(&premise_labels_path, &premise_vectors_path),
        );

        // The premise index is a DIFFERENT embedding space over a subset of the corpus. If the two store
        // constructors ever came to read the same section, every number below would still agree while More
        // Like This quietly answered plot neighbours for a premise query.
        assert_ne!(plot.0.len(), premise.0.len(), "the two spaces cover different title counts");
        let first = plot.0.titles().next().expect("a title");
        let (a, b) = (plot.0.row_of(first.1, first.0).unwrap(), premise.0.row_of(first.1, first.0).unwrap());
        assert_ne!(
            plot.0.row_vector(a as usize),
            premise.0.row_vector(b as usize),
            "premise must not be built off vec_plot"
        );

        let mut all_blank = Vec::new();
        for (space, (from_store, from_blobs)) in [("plot", &plot), ("premise", &premise)] {
            let dropped = store.rows() - from_store.len();
            eprintln!(
                "[{space}] store rows: {}, indexed: {}, dropped for want of a vector: {dropped}",
                store.rows(),
                from_store.len()
            );
            assert_eq!(from_store.len(), from_blobs.len(), "{space} record count");
            assert_eq!(from_store.dimension(), from_blobs.dimension(), "{space} dimension");
            assert_eq!(from_store.taxonomy_version(), "", "the store carries no taxonomy version");

            let (differ, blank, quantised, compared) = compare(space, from_store, from_blobs);
            eprintln!(
                "[{space}] titles compared: {compared}; differing: {}; blank in the store: {}; \
                 agreeing to the hundredth but not to the bit: {quantised}",
                differ.len(),
                blank.len()
            );
            for line in differ.iter().take(20) {
                eprintln!("  {line}");
            }
            assert!(
                differ.is_empty(),
                "[{space}] {} of {compared} titles differ; first few:\n{}",
                differ.len(),
                differ.iter().take(3).cloned().collect::<Vec<_>>().join("\n")
            );
            same_taxonomy(space, from_store, from_blobs, blank.len());
            all_blank.extend(blank.into_iter().map(|key| (space, key)));
        }

        // The plot side is held to IDENTICAL, not to "within an allowance": nothing is known to be lost
        // there, so the full ordered taxonomy and every count must match.
        let (store_plot, blob_plot) = (&plot.0, &plot.1);
        assert_eq!(store_plot.subgenre_labels(), blob_plot.subgenre_labels(), "plot subgenre taxonomy");
        assert_eq!(store_plot.mood_labels(), blob_plot.mood_labels(), "plot mood taxonomy");
        assert_eq!(store_plot.primary_genre_counts(), blob_plot.primary_genre_counts(), "plot genre counts");
        let floor = DISPLAY_CONFIDENCE_FLOOR;
        assert_eq!(store_plot.subgenre_counts(floor), blob_plot.subgenre_counts(floor), "plot subgenres");
        assert_eq!(store_plot.mood_counts(floor), blob_plot.mood_counts(floor), "plot moods");

        // The known loss, bounded rather than waved through: the store's label sections are filled from
        // the PLOT labels pass alone, so a title the premise pass labelled and the plot pass did not
        // arrives with no labels. Every such title must be exactly that — `vec_plot_has = 0` — and there
        // must be none at all on the plot side, where those rows are not indexed in the first place.
        let has_plot = store.per_row::<u8>("vec_plot_has").expect("vec_plot_has");
        for &(space, (media, id)) in &all_blank {
            assert_eq!(space, "premise", "a blank-labelled title on the plot side: {media:?}:{id}");
            let row = store.row_of(u8::from(media == MediaType::Tv), id).expect("row").expect("indexed");
            assert_eq!(
                has_plot[row.0], 0,
                "{media:?}:{id} has plot labels in the store yet came back blank — not the known loss"
            );
        }
        eprintln!(
            "titles the store cannot label (premise-only labels, den-dataset build_store.py): {:?}",
            all_blank.iter().map(|&(_, key)| key).collect::<Vec<_>>()
        );
        assert!(
            all_blank.len() <= 3,
            "the premise-only label loss grew to {} titles; fix build_store.py rather than this bound",
            all_blank.len()
        );
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
