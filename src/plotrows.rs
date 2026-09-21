//! Browse rows from the store's facet axes: closed axes read from Wikipedia plots — how a story ends, when
//! it is set, how it is told — which cut across genre in a way a primary genre can't. A row may also name a
//! mood or subgenre from the labels, alone or with the facets.
//!
//! Rows only, never filters. A title the corpus does not describe is unknown, not a negative: a row lists
//! what is known, and nothing may read it as exhaustive. Not `facets.bin`, which is country, language and
//! year for attribute search.
//!
//! # These used to come from `plotFacetsFile`
//!
//! A 5,336-title JSON sidecar, frozen at a dead `datasetVersion` because nothing rebuilt it. The store
//! carries the same axes for every title — **47,618** — and three more besides, so the file was describing
//! 11% of the corpus while the answer for all of it sat in a section beside it.
//!
//! Two things the move had to reconcile.
//!
//! **Confidence.** The file said `high`/`medium`/`low`; the store keeps the number the model gave. So a row
//! needs a floor, and there already is one: `DISPLAY_CONFIDENCE_FLOOR`, which the label rows use for the
//! same decision — "confident enough to put in front of a viewer". Reusing it keeps one rule rather than
//! inventing a second for the same question.
//!
//! **`structure` was three questions.** The file's single `structure` axis mixed how the telling is ordered
//! (`linear`, `nonlinear`, `framed`, `parallel-strands`) with how much time the story covers (`single-day`)
//! and whether episodes connect (`anthology`). The store separates them into `chronology`, `timespan` and
//! `continuity`. `STRUCTURE_ALIAS` maps an incoming `structure=…` onto whichever axis actually answers it,
//! so a client that still asks the old way keeps working.

use crate::queries::Indexes;
use den_index::MediaType;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

type Key = (MediaType, u32);

/// A facet is shown in a row only at or above this confidence — the same floor the label rows use.
///
/// The store keeps every value the model produced, including the ones it was unsure of, because a store
/// that has already thrown data away cannot be re-tuned. Deciding what is confident enough to SHOW is the
/// reader's job, and this is where it happens.
const FACET_FLOOR: f64 = den_index::DISPLAY_CONFIDENCE_FLOOR;

/// The sidecar layout the fallback reader understands (`"schema"` in the file).
const SCHEMA: u32 = 1;

/// `structure=<value>` → the axis in the store that answers it.
///
/// The old sidecar's `structure` axis conflated three separate questions; the store asks them separately.
/// Anything not listed here is a chronology value, which is what `structure` mostly meant.
const STRUCTURE_ALIAS: &[(&str, &str)] = &[("single-day", "timespan"), ("anthology", "continuity")];

/// The axis a constraint really names, after the alias above.
fn resolve_axis(axis: &str, value: &str) -> String {
    if axis != "structure" {
        return axis.to_owned();
    }
    STRUCTURE_ALIAS.iter().find(|(v, _)| *v == value).map_or("chronology", |(_, axis)| *axis).to_owned()
}

pub struct PlotFacets {
    /// axis → value → the titles carrying it, each with its confidence: 3 high, 2 medium, 1 low.
    by_value: HashMap<String, HashMap<String, Vec<(Key, u8)>>>,
    titles: usize,
}

pub struct FacetSchema {
    pub axis: String,
    pub known: usize,
    pub values: Vec<(String, usize)>,
}

impl PlotFacets {
    /// Every facet the store holds, inverted to axis → value → titles, once, at load.
    ///
    /// Inverted rather than scanned per request because a row asks "every title with `ending=bittersweet`",
    /// which over 47,618 rows x 12 axes is a 571k-cell scan each time. One pass here, then O(1) lookups —
    /// the same shape the JSON reader built, so nothing downstream changes.
    pub fn from_store(store: &den_store::Store<'_>) -> Result<PlotFacets, String> {
        let keys = store.per_row::<u64>("keys").map_err(|e| e.to_string())?;
        let values = store.column::<u32>("facet_v").map_err(|e| e.to_string())?;
        let confs = store.column::<u8>("facet_c").map_err(|e| e.to_string())?;
        let strings = store.strings().map_err(|e| e.to_string())?;
        let axes = den_store::FACET_AXES.len();
        if values.len() != keys.len() * axes || confs.len() != keys.len() * axes {
            return Err(format!(
                "facets hold {}/{} cells for {} rows x {axes} axes",
                values.len(),
                confs.len(),
                keys.len()
            ));
        }

        let floor = (FACET_FLOOR * 100.0).round() as u8;
        let mut by_value: HashMap<String, HashMap<String, Vec<(Key, u8)>>> = HashMap::new();
        let mut described = 0usize;
        for (row, &packed) in keys.iter().enumerate() {
            let media = if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie };
            let key = (media, packed as u32);
            let mut any = false;
            for (axis, name) in den_store::FACET_AXES.iter().enumerate() {
                let at = row * axes + axis;
                let (value, conf) = (values[at], confs[at]);
                // A declined axis is stored absent, never as a value, and a value under the floor is one
                // the classifier was not sure of — review material, not a row.
                if value == den_store::NONE_U32 || conf < floor {
                    continue;
                }
                let Some(value) = strings.get(value) else { continue };
                any = true;
                // The 3/2/1 scale the row order sorts on, from the same thresholds the labels use.
                let confidence = match f64::from(conf) / 100.0 {
                    c if c >= 0.8 => 3,
                    c if c >= 0.7 => 2,
                    _ => 1,
                };
                by_value
                    .entry((*name).to_owned())
                    .or_default()
                    .entry(value.to_owned())
                    .or_default()
                    .push((key, confidence));
            }
            if any {
                described += 1;
            }
        }
        Ok(PlotFacets { by_value, titles: described })
    }

    /// The old `plotFacetsFile` sidecar.
    ///
    /// Kept as a FALLBACK for a dataset published before the store carried facets — the same
    /// degrade-rather-than-fail rule every other optional blob here follows. Production no longer ships
    /// one; `from_store` is the path that runs.
    pub fn read(path: &Path) -> Result<PlotFacets, String> {
        let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        PlotFacets::from_bytes(&raw).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn from_bytes(raw: &[u8]) -> Result<PlotFacets, String> {
        let file: RawFile = serde_json::from_slice(&gunzipped(raw)?).map_err(|e| format!("parse: {e}"))?;
        if file.schema != SCHEMA {
            return Err(format!("schema {} (this atlas reads {SCHEMA})", file.schema));
        }
        let mut by_value: HashMap<String, HashMap<String, Vec<(Key, u8)>>> = HashMap::new();
        let titles = file.facets.len();
        for (key, axes) in file.facets {
            let Some(key) = title_key(&key) else { continue };
            for (axis, facet) in axes {
                let Some(RawFacet { value: Some(value), confidence }) = facet else { continue };
                let confidence = match confidence.as_deref() {
                    Some("high") => 3,
                    Some("medium") => 2,
                    _ => 1,
                };
                by_value.entry(axis).or_default().entry(value).or_default().push((key, confidence));
            }
        }
        Ok(PlotFacets { by_value, titles })
    }

    pub fn len(&self) -> usize {
        self.titles
    }

    /// Every axis, its known-title coverage, and each value's count. Counts are over distinct title keys even
    /// if a malformed input repeats a value; schema/count consumers must never mistake duplicate rows for films.
    pub fn schema(&self) -> Vec<FacetSchema> {
        let mut axes: Vec<FacetSchema> = self
            .by_value
            .iter()
            .map(|(axis, values)| {
                let mut known = std::collections::HashSet::new();
                let mut counts: Vec<(String, usize)> = values
                    .iter()
                    .map(|(value, rows)| {
                        let keys: std::collections::HashSet<Key> = rows.iter().map(|(key, _)| *key).collect();
                        known.extend(keys.iter().copied());
                        (value.clone(), keys.len())
                    })
                    .collect();
                counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                FacetSchema { axis: axis.clone(), known: known.len(), values: counts }
            })
            .collect();
        axes.sort_by(|a, b| a.axis.cmp(&b.axis));
        axes
    }

    /// Known-title coverage for one axis.
    pub fn coverage(&self, axis: &str) -> usize {
        self.coverage_for(axis, None)
    }

    pub fn coverage_for(&self, axis: &str, media_type: Option<MediaType>) -> usize {
        self.by_value
            .get(if axis == "structure" { "chronology" } else { axis })
            .into_iter()
            .flat_map(|values| values.values())
            .flatten()
            .map(|(key, _)| *key)
            .filter(|key| media_type.is_none() || Some(key.0) == media_type)
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// The titles of `media_type` carrying every `(axis, value)`, each at the lowest confidence it carries any
    /// of them. Empty when a constraint names an axis or value the file doesn't have.
    pub fn matching(&self, media_type: MediaType, constraints: &[(String, String)]) -> Vec<(Key, u8)> {
        let mut lists: Vec<&Vec<(Key, u8)>> = Vec::with_capacity(constraints.len());
        for (axis, value) in constraints {
            // `structure=…` is the old sidecar's spelling; resolve it to whichever axis answers it.
            let axis = &resolve_axis(axis, value);
            match self.by_value.get(axis).and_then(|values| values.get(value)) {
                Some(list) => lists.push(list),
                None => return Vec::new(),
            }
        }
        let Some((first, rest)) = lists.split_first() else { return Vec::new() };
        let rest: Vec<HashMap<Key, u8>> = rest.iter().map(|list| list.iter().copied().collect()).collect();
        first
            .iter()
            .filter(|(key, _)| key.0 == media_type)
            .filter_map(|&(key, confidence)| {
                rest.iter()
                    .try_fold(confidence, |lowest, other| other.get(&key).map(|&c| lowest.min(c)))
                    .map(|lowest| (key, lowest))
            })
            .collect()
    }
}

/// What a poster card needs, from the dataset's metadata sidecar.
pub struct Card {
    pub title: String,
    pub poster_path: Option<String>,
    pub year: Option<i64>,
}

/// The metadata sidecar (`metadataFile`) as cards by title.
pub fn read_cards(path: &Path) -> Result<HashMap<Key, Card>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawCard {
        tmdb_id: u32,
        media_type: String,
        title: Option<String>,
        poster_path: Option<String>,
        year: Option<i64>,
    }
    let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let cards: Vec<RawCard> =
        serde_json::from_slice(&gunzipped(&raw)?).map_err(|e| format!("{}: parse: {e}", path.display()))?;
    Ok(cards
        .into_iter()
        .filter_map(|c| {
            let media_type = match c.media_type.as_str() {
                "movie" => MediaType::Movie,
                "tv" => MediaType::Tv,
                _ => return None,
            };
            Some((
                (media_type, c.tmdb_id),
                Card { title: c.title?, poster_path: c.poster_path, year: c.year },
            ))
        })
        .collect())
}

/// The sidecar's poster paths alone, as `(media type, tmdb id) -> "/abc.jpg"`.
///
/// Separate from `read_cards` because the catalog rows want the path and nothing else, and there are 38.5k
/// of them: keeping each title and year too would hold a `String` per title for no reader. Serde drops what
/// this struct does not name, so those are never allocated.
pub fn read_posters(path: &Path) -> Result<HashMap<Key, Box<str>>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawPoster {
        tmdb_id: u32,
        media_type: String,
        poster_path: Option<String>,
    }
    let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let cards: Vec<RawPoster> =
        serde_json::from_slice(&gunzipped(&raw)?).map_err(|e| format!("{}: parse: {e}", path.display()))?;
    Ok(cards
        .into_iter()
        .filter_map(|c| {
            let media_type = match c.media_type.as_str() {
                "movie" => MediaType::Movie,
                "tv" => MediaType::Tv,
                _ => return None,
            };
            Some(((media_type, c.tmdb_id), c.poster_path?.into_boxed_str()))
        })
        .collect())
}

/// A row: the titles of `media_type` carrying every constraint, most confident first, then most voted — each as
/// the card a client draws, with what its hide rules read — `skip` then `limit` of them, and how many there are.
/// A constraint is a plot facet (`tone=bleak`) or one of the labels (`mood=Feel-good`, `subgenre=Heist`), and
/// they combine. A title with no card is left out, since there is nothing to draw. "Most voted" reads TMDB's
/// popularity in its daily `export` for a title facets.bin has no votes for.
pub fn row(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    media_type: MediaType,
    constraints: &[(String, String)],
    skip: usize,
    limit: usize,
) -> serde_json::Value {
    let coverage = crate::schema::row_coverage(indexes, media_type, constraints);
    let Some(cards) = indexes.cards.as_ref() else {
        return serde_json::json!({ "titles": [], "total": 0, "coverage": coverage });
    };
    // Worked out once per type and constraints (`Indexes::row_order`): every page of a row asks for the same order.
    let mut named: Vec<String> = constraints.iter().map(|(axis, value)| format!("{axis}={value}")).collect();
    named.sort_unstable();
    let kind = if media_type == MediaType::Tv { "tv" } else { "movie" };
    let order = indexes.row_order(format!("{kind}?{}", named.join("&")), || {
        let (labels, plot): (Vec<_>, Vec<_>) =
            constraints.iter().cloned().partition(|(axis, _)| axis == "mood" || axis == "subgenre");
        let candidates: Vec<(Key, u8)> = if !plot.is_empty() {
            indexes.plot_facets.as_ref().map_or_else(Vec::new, |facets| facets.matching(media_type, &plot))
        } else if let Some((family, label)) = labels.first() {
            let titles = if family == "mood" {
                indexes.plot.titles_with_mood(label, Some(media_type), LABEL_FLOOR, 0, usize::MAX)
            } else {
                indexes.plot.titles_with_subgenre(label, Some(media_type), LABEL_FLOOR, 0, usize::MAX)
            };
            titles.into_iter().map(|(id, kind)| ((kind, id), 3)).collect()
        } else {
            Vec::new()
        };
        let popularity = |(media_type, id): Key| {
            let votes = indexes.votes(media_type, id);
            let kind = match media_type {
                MediaType::Movie => den_titlesearch::MediaType::Movie,
                MediaType::Tv => den_titlesearch::MediaType::Tv,
            };
            crate::search::attention(votes, export.and_then(|e| e.popularity_of(kind, id)))
        };
        let mut matched: Vec<(Key, u8, f64)> = candidates
            .into_iter()
            .filter(|(key, _)| cards.contains_key(key))
            .filter_map(|(key, confidence)| {
                labels
                    .iter()
                    .try_fold(confidence, |lowest, (family, label)| {
                        label_confidence(indexes, key, family, label).map(|c| lowest.min(c))
                    })
                    .map(|lowest| (key, lowest, popularity(key)))
            })
            .collect();
        matched.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)).then(a.0 .1.cmp(&b.0 .1)));
        matched.into_iter().map(|(key, _, _)| key).collect()
    });
    let total = order.len();
    let titles: Vec<serde_json::Value> = order
        .iter()
        .skip(skip)
        .take(limit)
        .map(|&key| {
            let (media_type, id) = key;
            let card = &cards[&key];
            let mut title = serde_json::json!({
                "type": if media_type == MediaType::Tv { "series" } else { "movie" },
                "id": id,
                "title": card.title,
                "posterPath": card.poster_path,
                "year": card.year,
                "genreIds": genres(indexes, key),
                "primaryGenre": primary_genre(indexes, key),
            });
            // Its IMDb id, which a client's availability check keys streams by: without it the client asks TMDB
            // for it, a request a card.
            if let Some(imdb) =
                indexes.facts.as_ref().and_then(|f| f.get(id, media_type)).and_then(|r| r.imdb_id.as_deref())
            {
                title["imdbId"] = serde_json::json!(imdb);
            }
            if let Some(language) =
                indexes.facets.as_ref().and_then(|f| f.title(id, media_type)).and_then(|t| t.language)
            {
                title["originalLanguage"] = serde_json::json!(String::from_utf8_lossy(&language));
            }
            title
        })
        .collect();
    serde_json::json!({ "titles": titles, "total": total, "coverage": coverage })
}

/// The confidence a label row needs (the label rows' own floor).
const LABEL_FLOOR: f64 = den_index::DISPLAY_CONFIDENCE_FLOOR;

/// How sure the labels are that a title carries a mood or subgenre, on the plot facets' scale (3 high, 2 medium,
/// 1 low), or `None` under the floor.
fn label_confidence(indexes: &Indexes, (media_type, id): Key, family: &str, label: &str) -> Option<u8> {
    let labels = indexes.plot.labels(id, media_type)?;
    let pairs = if family == "mood" { &labels.moods } else { &labels.subgenres };
    let &(_, confidence) = pairs.iter().find(|(name, _)| *name == label)?;
    match confidence {
        c if c >= 0.8 => Some(3),
        c if c >= 0.7 => Some(2),
        c if c >= LABEL_FLOOR => Some(1),
        _ => None,
    }
}

/// The one genre the labels call a title's own, for a client to DISPLAY — "Crime", not a list of ids.
///
/// `None` for a title the corpus does not label, which is most of them: atlas knows 47,618 titles and TMDB
/// has millions, so any row drawn from a TMDB list carries titles this cannot answer for. A client falls
/// back to naming `genreIds` itself; that fallback is the normal case on a new release, not an error.
///
/// Display is not filtering. Hide rules read `genres` above — TMDB ids, which both clients already sync
/// under `den.excludedGenreIDs` — and must not be rewired to this.
pub(crate) fn primary_genre(indexes: &Indexes, (media_type, id): Key) -> Option<&str> {
    indexes.plot.labels(id, media_type).map(|l| l.primary_genre).filter(|g| !g.is_empty())
}

/// Every genre anything names for a title, as TMDB genre ids: the labels' primary genre and animation, and the
/// facts' genres. What a client's hide rules read.
pub(crate) fn genres(indexes: &Indexes, (media_type, id): Key) -> Vec<u16> {
    let mut genres: Vec<u16> = Vec::new();
    if let Some(labels) = indexes.plot.labels(id, media_type) {
        genres.extend(crate::recommend::genre_named(labels.primary_genre));
        if labels.animated {
            genres.push(16);
        }
    }
    if let Some(record) = indexes.facts.as_ref().and_then(|f| f.get(id, media_type)) {
        genres.extend(record.genres.iter().filter(|g| !genres.contains(g)).collect::<Vec<_>>());
    }
    genres
}

fn title_key(key: &str) -> Option<Key> {
    let (media_type, id) = key.split_once(':')?;
    let media_type = match media_type {
        "movie" => MediaType::Movie,
        "tv" => MediaType::Tv,
        _ => return None,
    };
    Some((media_type, id.parse().ok()?))
}

fn gunzipped(raw: &[u8]) -> Result<Vec<u8>, String> {
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Ok(raw.to_vec());
    }
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(raw).read_to_end(&mut plain).map_err(|e| format!("gunzip: {e}"))?;
    Ok(plain)
}

#[derive(Deserialize)]
struct RawFile {
    schema: u32,
    #[serde(default)]
    facets: HashMap<String, HashMap<String, Option<RawFacet>>>,
}

#[derive(Deserialize)]
struct RawFacet {
    value: Option<String>,
    confidence: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Plot facets for the route fixture (`queries::write_fixture`): movies 1–3 and series 4.
    pub(crate) const SAMPLE: &str = r#"{
      "schema": 1, "datasetVersion": "v1", "axes": ["ending", "tone", "pacing"],
      "coverage": {"titles": 4, "corpusTitles": 4},
      "facets": {
        "movie:1": {"ending": {"value": "bittersweet", "confidence": "medium"}, "tone": {"value": "bleak", "confidence": "high"},
                    "pacing": {"value": "slow-burn", "confidence": "high"}},
        "movie:2": {"ending": {"value": "bittersweet", "confidence": "high"}, "tone": {"value": "bleak", "confidence": "low"},
                    "pacing": {"value": null, "confidence": null}},
        "movie:3": {"ending": {"value": "bittersweet", "confidence": "high"}, "tone": {"value": "comic", "confidence": "high"},
                    "pacing": null},
        "tv:4": {"ending": {"value": "bittersweet", "confidence": "high"}}
      }
    }"#;

    #[test]
    fn matches_every_constraint_at_the_lowest_confidence_and_only_of_one_type() {
        let facets = PlotFacets::from_bytes(SAMPLE.as_bytes()).unwrap();
        assert_eq!(facets.len(), 4);
        let pair = |axis: &str, value: &str| (axis.to_owned(), value.to_owned());
        let mut bittersweet = facets.matching(MediaType::Movie, &[pair("ending", "bittersweet")]);
        bittersweet.sort();
        assert_eq!(
            bittersweet,
            vec![((MediaType::Movie, 1), 2), ((MediaType::Movie, 2), 3), ((MediaType::Movie, 3), 3)]
        );
        let mut bleak =
            facets.matching(MediaType::Movie, &[pair("ending", "bittersweet"), pair("tone", "bleak")]);
        bleak.sort();
        assert_eq!(bleak, vec![((MediaType::Movie, 1), 2), ((MediaType::Movie, 2), 1)]);
        // A null value is unknown: movie 2 carries no pacing, not "not slow-burn".
        assert_eq!(
            facets.matching(MediaType::Movie, &[pair("pacing", "slow-burn")]),
            vec![((MediaType::Movie, 1), 3)]
        );
        assert_eq!(
            facets.matching(MediaType::Tv, &[pair("ending", "bittersweet")]),
            vec![((MediaType::Tv, 4), 3)]
        );
        assert!(facets.matching(MediaType::Movie, &[pair("ending", "sad")]).is_empty());
        assert!(facets.matching(MediaType::Movie, &[pair("colour", "blue")]).is_empty());
        assert!(facets.matching(MediaType::Movie, &[]).is_empty());
    }
}
