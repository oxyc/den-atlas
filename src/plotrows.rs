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
use den_sync::{Era, Signals, Weights};
/// Only the sidecar readers below parse JSON, and only tests call them.
#[cfg(test)]
use serde::Deserialize;
use std::collections::HashMap;
#[cfg(test)]
use std::io::Read;
#[cfg(test)]
use std::path::Path;

type Key = (MediaType, u32);

/// A facet is shown in a row only at or above this confidence — the same floor the label rows use.
///
/// It is a second floor, not the only one. The writer applies FACETS-V2's publication gates, which read
/// the probability distribution and the validity judgement — neither of which reaches the store — so any
/// value that is here was already publishable. This floor is on the self-reported confidence the store
/// does carry, and unlike the writer's it is tunable without a rebuild: the corpus keeps every answer the
/// model produced, the store is the publication.
const FACET_FLOOR: f64 = den_index::DISPLAY_CONFIDENCE_FLOOR;

/// The sidecar layout the test-only reader understands (`"schema"` in the file).
#[cfg(test)]
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
    /// **Tests only.** Nothing reads the sidecar: it is not published and `from_store` is the only path
    /// that runs. It stays behind `#[cfg(test)]` so the unit test below still has a second description of
    /// the same four titles to hold `matching` to.
    #[cfg(test)]
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

/// The cards, out of the store.
///
/// Same map, same shape, different input — so plot rows and the search display titles keep working while
/// `metadataFile` stops being read. The store carries `card_title`, `card_poster` and `card_year` for
/// every row it holds, which is MORE rows than the sidecar has records: the writer falls back to the
/// facts' `titles.en`/`titles.orig` for a title the sidecar does not name, so a row can have a card here
/// and none there. Those rows are kept — a title the sidecar never described is not a reason to answer
/// with nothing for it.
///
/// A row with no resolvable title is skipped, exactly as `read_cards` skips a record whose `title` is
/// null: a card is a thing to draw, and there is nothing to draw without a name. The empty string is
/// treated the same way for the same reason — the writer's `title or titles.en or titles.orig` chain
/// cannot emit one, so this is a guard rather than a behaviour, and a blank label is not a card.
/// `posterPath` keeps whatever string the store holds, empty included, because `read_cards` did: only
/// the ABSENT id (`u32::MAX`) is `None`.
pub fn cards_from_store(store: &den_store::Store<'_>) -> Result<HashMap<Key, Card>, String> {
    let err = |e: den_store::StoreError| e.to_string();
    let keys = store.per_row::<u64>("keys").map_err(err)?;
    let titles = store.per_row::<u32>("card_title").map_err(err)?;
    let posters = store.per_row::<u32>("card_poster").map_err(err)?;
    let years = store.per_row::<i16>("card_year").map_err(err)?;
    let strings = store.strings().map_err(err)?;

    let mut cards: HashMap<Key, Card> = HashMap::with_capacity(keys.len());
    for (i, &packed) in keys.iter().enumerate() {
        let media_type = if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie };
        // `Strings::get` answers `None` for `NONE_U32` and for an id it cannot resolve, so the sentinel
        // never becomes the string "4294967295" — and `NONE_I16` is absent, not the year -32768.
        let Some(title) = strings.get(titles[i]).filter(|title| !title.is_empty()) else { continue };
        cards.insert(
            (media_type, packed as u32),
            Card {
                title: title.to_owned(),
                poster_path: strings.get(posters[i]).map(str::to_owned),
                year: (years[i] != den_store::NONE_I16).then(|| i64::from(years[i])),
            },
        );
    }
    Ok(cards)
}

/// The metadata sidecar (`metadataFile`) as cards by title.
///
/// **Tests only.** The cards come out of the store; the sidecar is not read and no longer published.
/// It stays behind `#[cfg(test)]` so `the_store_answers_what_the_metadata_sidecar_did` still has the
/// reader it compares against — a deleted reader cannot disagree with anything.
#[cfg(test)]
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

/// The store's poster paths alone, as `(media type, tmdb id) -> "/abc.jpg"`.
///
/// Separate from `cards_from_store` because the catalog rows want the path and nothing else, and there are
/// tens of thousands of them: keeping each title and year too would hold a `String` per title for no
/// reader. This read the metadata sidecar until the store carried `card_poster`.
pub fn posters_from_store(store: &den_store::Store<'_>) -> Result<HashMap<Key, Box<str>>, String> {
    let err = |e: den_store::StoreError| e.to_string();
    let keys = store.per_row::<u64>("keys").map_err(err)?;
    let posters = store.per_row::<u32>("card_poster").map_err(err)?;
    let strings = store.strings().map_err(err)?;
    Ok(keys
        .iter()
        .enumerate()
        .filter_map(|(i, &packed)| {
            let media_type = if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie };
            let poster = strings.get(posters[i]).filter(|path| !path.is_empty())?;
            Some(((media_type, packed as u32), poster.to_owned().into_boxed_str()))
        })
        .collect())
}

/// The most titles one taste list may name. The same cap `POST /index/score` puts on a candidate list, for
/// the same reason: a centroid over more than this is not a better centroid, and the list travels in a URL.
const MAX_TASTE: usize = 500;

/// The prefix every tilt parameter carries, so a household's taste can never be mistaken for a facet
/// constraint — `tilt.liked` is not an axis called `tilt.liked` — whatever axes the store grows.
pub(crate) const TILT_PREFIX: &str = "tilt.";

/// A household's taste, as a row query asks for it.
///
/// # Why this is a request parameter and not a stored profile
///
/// atlas holds no household state: the same corpus answers everyone, and a row is a function of what the
/// request said. That is also what keeps the answer cacheable — a page is a slice of an order that is fixed
/// for (row, taste, weights), so page 3 is the same work as page 1 and neither can disagree with the other.
///
/// # The weights are parameters too
///
/// All four levers arrive per request (`tilt.w.*`), defaulting to den-core's. Baking them in is what takes
/// them away from a tuner, and retrofitting that later is the expensive version.
pub struct Tilt {
    /// The liked and disliked titles, as `Index::centroid` takes them.
    liked: Vec<(u32, MediaType)>,
    disliked: Vec<(u32, MediaType)>,
    /// The household's era curve, fitted by the CLIENT from its own library years (den-core's
    /// `Era::from_samples`). Present means the era term is on: a row already fixed to an era — a decade row
    /// — simply leaves it out, and atlas never invents a curve it was not given.
    era: Option<Era>,
    weights: Weights,
    /// FNV-1a over the canonical taste: the liked and disliked sets, deduplicated and sorted, and the era
    /// curve. Deduplicated and sorted because `liked=m2,m1` and `liked=m1,m2,m1` are the SAME taste and must
    /// share one memo entry and one order — a fingerprint over the raw query string would fork both.
    ///
    /// It is the household's taste and nothing else, so it changes exactly when the liked/disliked sets do,
    /// and it is sixteen hex digits: cheap to compute, cheap to send, cheap to log.
    taste: String,
    /// The four levers, fingerprinted apart from the taste, so a memo key says which of the two moved.
    weights_key: String,
}

impl Tilt {
    /// The tilt a row query asks for, or `None` when it asks for none.
    ///
    /// Never an error. An unparseable id, a malformed number or an axis this does not know is skipped, and a
    /// query that leaves nothing to tilt WITH — no liked titles, no disliked titles, no era curve — is no
    /// tilt at all, which orders the row byte-identically to a request that named no taste. Degrading to
    /// today's row is always available; refusing the request is not.
    pub fn parse(query: &str) -> Option<Tilt> {
        let value = |key: &str| crate::handler::query_param(query, &format!("{TILT_PREFIX}{key}"));
        let liked = titles(value("liked").as_deref());
        let disliked = titles(value("disliked").as_deref());
        let era = value("era").as_deref().and_then(pair).map(|(center, spread)| Era { center, spread });
        if liked.is_empty() && disliked.is_empty() && era.is_none() {
            return None;
        }
        let number = |key: &str| value(key).and_then(|v| v.parse::<f64>().ok());
        let default = Weights::default();
        let weights = Weights {
            embedding: number("w.embedding").unwrap_or(default.embedding),
            dislike: number("w.dislike").unwrap_or(default.dislike),
            era: number("w.era").unwrap_or(default.era),
            // Anything but an explicit `0` keeps the squaring, which is what makes the dislike weight safe.
            square_dislike: value("w.square").map_or(default.square_dislike, |v| v != "0"),
        };
        let canonical = |titles: &[(u32, MediaType)]| {
            let keys: std::collections::BTreeSet<(MediaType, u32)> =
                titles.iter().map(|&(id, kind)| (kind, id)).collect();
            keys.iter().map(|&(kind, id)| format!("{}{id}", media_letter(kind))).collect::<Vec<_>>().join(",")
        };
        let era_text = era.map_or_else(|| "-".to_owned(), |e| format!("{:.6},{:.6}", e.center, e.spread));
        Some(Tilt {
            taste: crate::util::fnv1a(&format!(
                "l:{};d:{};e:{era_text}",
                canonical(&liked),
                canonical(&disliked)
            )),
            weights_key: crate::util::fnv1a(&format!(
                "{:.6},{:.6},{:.6},{}",
                weights.embedding,
                weights.dislike,
                weights.era,
                u8::from(weights.square_dislike)
            )),
            liked,
            disliked,
            era,
            weights,
        })
    }

    /// `order`, reordered for this household — a PERMUTATION of it and nothing else.
    ///
    /// The whole row, not a page: the order is computed over every title the row holds, so a title on page 3
    /// can be lifted onto page 1. That is the ceiling a client-side tilt cannot pass, since it only ever sees
    /// the page it loaded.
    ///
    /// The composition is den-core's `tilt::order`, not a copy of it. atlas measures the cosines — they need
    /// the vector space, which is why they are here — and den-core decides what they are worth.
    fn applied(&self, indexes: &Indexes, order: &[Key], cards: &HashMap<Key, Card>) -> Vec<Key> {
        let index = &indexes.plot;
        let liked = index.centroid(&self.liked);
        let disliked = index.centroid(&self.disliked);
        // A household whose titles the corpus does not hold has no centroid, and with no era curve either
        // there is nothing to tilt by. The row it gets is today's row.
        if liked.is_none() && disliked.is_none() && self.era.is_none() {
            return order.to_vec();
        }
        let signals: Vec<Signals> = order
            .iter()
            .map(|&key| {
                let (media_type, id) = key;
                let boost = |centroid: &Option<Vec<f64>>| {
                    centroid.as_ref().map_or(0.0, |c| index.taste_boost(id, media_type, c))
                };
                Signals {
                    taste: boost(&liked),
                    dislike: boost(&disliked),
                    // Off the card the row already draws, so the era term costs no lookup of its own.
                    year: cards.get(&key).and_then(|c| c.year).and_then(|y| i32::try_from(y).ok()),
                }
            })
            .collect();
        let curve = self.era.unwrap_or(Era { center: 0.0, spread: 0.0 });
        let permutation = den_sync::order(&signals, &self.weights, &curve, self.era.is_some());
        // Reorder only. A tilt that returned anything but a permutation would drop or duplicate a title,
        // which reads as a filter and invalidates every page already scrolled past — so an answer that is
        // not one leaves the row exactly as it arrived.
        let mut seen = vec![false; order.len()];
        let mut tilted = Vec::with_capacity(order.len());
        for &i in &permutation {
            match order.get(i) {
                Some(&key) if !std::mem::replace(&mut seen[i], true) => tilted.push(key),
                _ => {
                    eprintln!(
                        "tilt: the policy returned {} indices for {} titles — keeping the incoming order",
                        permutation.len(),
                        order.len()
                    );
                    return order.to_vec();
                }
            }
        }
        if tilted.len() != order.len() {
            eprintln!(
                "tilt: {} titles ordered of {} — keeping the incoming order",
                tilted.len(),
                order.len()
            );
            return order.to_vec();
        }
        tilted
    }
}

/// `m550,t1396` → the titles it names. An entry this cannot read is skipped, not fatal.
fn titles(list: Option<&str>) -> Vec<(u32, MediaType)> {
    let Some(list) = list.filter(|l| !l.is_empty()) else { return Vec::new() };
    list.split(',')
        .filter_map(|entry| {
            let (letter, id) = entry.split_at_checked(1)?;
            let media_type = match letter {
                "m" => MediaType::Movie,
                "t" => MediaType::Tv,
                _ => return None,
            };
            Some((id.parse().ok()?, media_type))
        })
        .take(MAX_TASTE)
        .collect()
}

/// `2004.5,12` → the pair it names, when both halves read as numbers.
fn pair(text: &str) -> Option<(f64, f64)> {
    let (a, b) = text.split_once(',')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// The letter a taste list spells a type with, and the canonical fingerprint sorts on.
fn media_letter(media_type: MediaType) -> char {
    match media_type {
        MediaType::Movie => 'm',
        MediaType::Tv => 't',
    }
}

/// A row: the titles of `media_type` carrying every constraint, most confident first, then most voted — each as
/// the card a client draws, with what its hide rules read — `skip` then `limit` of them, and how many there are.
/// A constraint is a plot facet (`tone=bleak`) or one of the labels (`mood=Feel-good`, `subgenre=Heist`), and
/// they combine. A title with no card is left out, since there is nothing to draw. "Most voted" reads TMDB's
/// popularity in its daily `export` for a title facets.bin has no votes for.
///
/// `tilt` reorders the WHOLE row for a household before the page is cut, so a title the untilted order puts
/// on page 3 can lead page 1 — the ceiling a client-side tilt over an already-loaded page cannot pass. It
/// never filters and never changes `total`: the tilted order is a permutation of the untilted one, memoised
/// beside it.
pub fn row(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    media_type: MediaType,
    constraints: &[(String, String)],
    tilt: Option<&Tilt>,
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
    let row_key = format!("{kind}?{}", named.join("&"));
    let order = indexes.row_order(row_key.clone(), || {
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
    // The household's order, memoised BESIDE the untilted one — per (row x taste fingerprint x weights), so
    // every page of this row for this household is a slice of one order and the two cannot disagree. The
    // untilted key is untouched, so a request that names no taste shares the entry every other one does.
    let order = match tilt {
        Some(tilt) => indexes.row_order(format!("{row_key}|t{}|w{}", tilt.taste, tilt.weights_key), || {
            tilt.applied(indexes, &order, cards)
        }),
        None => order,
    };
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
    let mut answer = serde_json::json!({ "titles": titles, "total": total, "coverage": coverage });
    // Which order this page is a slice of, for a client that pages a row while the household's taste moves:
    // a page whose `taste` differs from the one before it came from a different order, so the two must not
    // be concatenated. Absent when no taste was sent, which keeps an untilted answer byte-identical to the
    // one this route gave before rows could be tilted at all.
    if let Some(tilt) = tilt {
        answer["taste"] = serde_json::json!(tilt.taste);
    }
    answer
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

#[cfg(test)]
fn title_key(key: &str) -> Option<Key> {
    let (media_type, id) = key.split_once(':')?;
    let media_type = match media_type {
        "movie" => MediaType::Movie,
        "tv" => MediaType::Tv,
        _ => return None,
    };
    Some((media_type, id.parse().ok()?))
}

#[cfg(test)]
fn gunzipped(raw: &[u8]) -> Result<Vec<u8>, String> {
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Ok(raw.to_vec());
    }
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(raw).read_to_end(&mut plain).map_err(|e| format!("gunzip: {e}"))?;
    Ok(plain)
}

#[cfg(test)]
#[derive(Deserialize)]
struct RawFile {
    schema: u32,
    #[serde(default)]
    facets: HashMap<String, HashMap<String, Option<RawFacet>>>,
}

#[cfg(test)]
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

    /// The fingerprint is over the TASTE, not over the query string that carried it: the same liked and
    /// disliked sets written differently must share one memo entry and one order, or two pages of a row
    /// could come from two orders and a scroll would repeat or skip titles.
    #[test]
    fn the_taste_fingerprint_is_over_the_set_not_the_spelling() {
        let fp = |query: &str| Tilt::parse(query).map(|t| t.taste);
        assert_eq!(fp("tilt.liked=m1,t4"), fp("tilt.liked=t4,m1,m1"), "order and repeats don't fork it");
        assert_ne!(fp("tilt.liked=m1"), fp("tilt.liked=m1,m2"), "a title added changes it");
        assert_ne!(fp("tilt.liked=m1"), fp("tilt.disliked=m1"), "liked is not disliked");
        assert_ne!(fp("tilt.liked=m1"), fp("tilt.liked=t1"), "a movie is not a series");
        assert_ne!(fp("tilt.liked=m1&tilt.era=2010,10"), fp("tilt.liked=m1"), "the era curve is taste too");
        // The levers are fingerprinted apart, so the memo key says which of the two moved.
        let weights = |query: &str| Tilt::parse(query).map(|t| t.weights_key);
        assert_eq!(fp("tilt.liked=m1&tilt.w.dislike=0.9"), fp("tilt.liked=m1"));
        assert_ne!(weights("tilt.liked=m1&tilt.w.dislike=0.9"), weights("tilt.liked=m1"));
    }

    /// Nothing to tilt WITH is no tilt: the row is ordered, and answered, exactly as it was before rows
    /// could be tilted at all. An unreadable taste degrades to the same place rather than to an error.
    #[test]
    fn a_taste_that_names_nothing_is_no_tilt() {
        for query in [
            "",
            "tilt.liked=&tilt.disliked=",
            // Weights with no taste change nothing, so they are not a tilt either.
            "tilt.w.embedding=2&tilt.w.dislike=0.9",
            // Unreadable ids: an entry with no type letter, an unknown one, and a non-number.
            "tilt.liked=550&tilt.disliked=x9,mabc",
            // An era that is not a pair of numbers.
            "tilt.era=recent",
        ] {
            assert!(Tilt::parse(query).is_none(), "{query:?}");
        }
        // And the shape that IS a tilt, so the cases above are not passing by a typo in the parser.
        assert!(Tilt::parse("tilt.liked=m1").is_some());
        assert!(Tilt::parse("tilt.era=2004.5,12").is_some());
    }

    /// The era term is on exactly when the household sent a curve. atlas never fits one itself — it has no
    /// library years — so an absent curve is a row with no era term, not a row tilted toward this year.
    #[test]
    fn the_era_term_is_on_only_when_a_curve_was_sent() {
        assert_eq!(Tilt::parse("tilt.liked=m1").unwrap().era, None);
        assert_eq!(
            Tilt::parse("tilt.liked=m1&tilt.era=2004.5,12").unwrap().era,
            Some(Era { center: 2004.5, spread: 12.0 })
        );
    }

    /// Each lever is independently overridable, and an omitted one keeps den-core's default — so a tuner
    /// can move one weight without restating the other three, and a client that sends none ships what
    /// den-core ships.
    #[test]
    fn every_weight_is_a_request_parameter_defaulting_to_den_cores() {
        assert_eq!(Tilt::parse("tilt.liked=m1").unwrap().weights, Weights::default());
        let tuned = Tilt::parse("tilt.liked=m1&tilt.w.dislike=0.9&tilt.w.square=0").unwrap().weights;
        assert_eq!(
            tuned,
            Weights { dislike: 0.9, square_dislike: false, ..Weights::default() },
            "one lever moved, the rest left alone"
        );
    }

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

    /// The store's cards against the sidecar's, on the REAL artifacts. Opt-in via `DEN_STORE` +
    /// `DEN_METADATA`.
    ///
    /// The cards are the title, poster path and year every `/index/row` answer carries, and a row is
    /// built only for keys that HAVE one — so a reader that silently loses a card drops the title from
    /// the row rather than drawing it wrong, and the fixture tests above cannot see it. Every key the
    /// sidecar describes must come back identical from the store.
    ///
    /// The store legitimately holds cards the sidecar does not: the writer falls back to the facts'
    /// `titles.en`/`titles.orig` when the sidecar has no record for a row, so the extras are classified
    /// here rather than waved through — a store card for a key the sidecar DOES describe with a
    /// different title would be a loss, not an extra.
    #[test]
    fn the_store_answers_what_the_metadata_sidecar_did() {
        let (Ok(store_path), Ok(metadata_path)) = (std::env::var("DEN_STORE"), std::env::var("DEN_METADATA"))
        else {
            eprintln!("SKIP: set DEN_STORE and DEN_METADATA to compare the two card readers");
            return;
        };
        let mapped = crate::store::MappedStore::open(std::path::Path::new(&store_path)).expect("store");
        let from_store = cards_from_store(&mapped.view()).expect("cards from the store");
        let from_json = read_cards(std::path::Path::new(&metadata_path)).expect("cards from the sidecar");
        eprintln!("cards: store {} · sidecar {}", from_store.len(), from_json.len());

        let mut differ = Vec::new();
        for (key, want) in &from_json {
            let Some(got) = from_store.get(key) else {
                differ.push(format!("{key:?} missing from the store"));
                continue;
            };
            // Name the FIELD that differs: three values, and a whole-Card dump says which record but not
            // which of them moved.
            let mut fields = Vec::new();
            if got.title != want.title {
                fields.push(format!("title {:?} vs {:?}", got.title, want.title));
            }
            if got.poster_path != want.poster_path {
                fields.push(format!("posterPath {:?} vs {:?}", got.poster_path, want.poster_path));
            }
            if got.year != want.year {
                fields.push(format!("year {:?} vs {:?}", got.year, want.year));
            }
            if !fields.is_empty() {
                differ.push(format!("{key:?} differs in {}", fields.join(", ")));
            }
        }
        eprintln!("cards differing: {} of {}", differ.len(), from_json.len());
        for line in differ.iter().take(20) {
            eprintln!("  {line}");
        }

        // What the store has and the sidecar does not. Split by whether the sidecar has a RECORD for the
        // key at all: a record the sidecar carries but `read_cards` drops (a null title) is a different
        // story from a row the sidecar never mentions, and lumping them together would hide either one.
        let raw = std::fs::read(&metadata_path).expect("metadata");
        let records: Vec<serde_json::Value> =
            serde_json::from_slice(&gunzipped(&raw).expect("gunzip")).expect("metadata parses");
        let described: std::collections::HashSet<Key> = records
            .iter()
            .filter_map(|r| {
                let media_type = match r.get("mediaType")?.as_str()? {
                    "movie" => MediaType::Movie,
                    "tv" => MediaType::Tv,
                    _ => return None,
                };
                Some((media_type, u32::try_from(r.get("tmdbId")?.as_u64()?).ok()?))
            })
            .collect();
        let (mut untitled, mut absent) = (Vec::new(), Vec::new());
        for key in from_store.keys() {
            if from_json.contains_key(key) {
                continue;
            }
            if described.contains(key) {
                untitled.push(*key);
            } else {
                absent.push(*key);
            }
        }
        eprintln!(
            "sidecar records {} · store-only cards {}: {} the sidecar describes without a title, {} it \
             has no record for",
            records.len(),
            untitled.len() + absent.len(),
            untitled.len(),
            absent.len()
        );
        for key in absent.iter().take(20) {
            eprintln!("  store-only {key:?}: {:?}", from_store[key].title);
        }

        // And the other direction: store rows this can draw no card for at all. They are dropped, as the
        // sidecar reader drops a titleless record — but a count that grows is a writer losing titles, and
        // a silently shrinking card map is exactly the failure this comparison exists to catch.
        let view = mapped.view();
        let keys = view.per_row::<u64>("keys").expect("keys");
        let titles = view.per_row::<u32>("card_title").expect("card_title");
        let strings = view.strings().expect("strings");
        let untitled_rows: Vec<Key> = keys
            .iter()
            .enumerate()
            .filter(|&(i, _)| strings.get(titles[i]).is_none_or(str::is_empty))
            .map(|(_, &packed)| {
                (if (packed >> 32) == 1 { MediaType::Tv } else { MediaType::Movie }, packed as u32)
            })
            .collect();
        eprintln!("store rows with no title: {} of {} — {untitled_rows:?}", untitled_rows.len(), keys.len());

        assert!(
            differ.is_empty(),
            "{} of {} cards differ; first few:\n{}",
            differ.len(),
            from_json.len(),
            differ.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
        );
    }
}
