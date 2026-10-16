//! Singapore IMDA's openly licensed classification dataset.
//!
//! Unlike per-title search providers, a few paged requests load the complete current dataset. The in-memory index is
//! discarded on restart; only normalized matches enter the coordinator's persistent cache. A row with an episode
//! range is TV evidence, while a row without one is film evidence. We never reinterpret an individual episode
//! title as its parent series.

use super::{name_key, now_secs, Backend, Budget, Identity, Lookup, RuntimeRating};
use crate::maturity::Region;
use async_trait::async_trait;
use den_index::MediaType;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DATASET: &str = "d_ee0231c767e05c37f61e01923df2e461";
const SOURCE: &str = "https://data.gov.sg/datasets/d_ee0231c767e05c37f61e01923df2e461/view";
const SNAPSHOT_MAX_AGE: u64 = 24 * 60 * 60;
const PAGE_SIZE: usize = 1_000;

type Index = HashMap<(MediaType, String), Vec<ImdaRow>>;

pub(super) struct Imda {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    snapshot: Mutex<Option<(u64, Arc<Index>)>>,
}

impl Imda {
    pub(super) fn new() -> Result<Self, String> {
        let mut endpoint = reqwest::Url::parse("https://data.gov.sg/api/action/datastore_search")
            .map_err(|error| error.to_string())?;
        endpoint.query_pairs_mut().append_pair("resource_id", DATASET);
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| error.to_string())?,
            endpoint,
            snapshot: Mutex::new(None),
        })
    }

    fn current(&self) -> Option<Arc<Index>> {
        let now = now_secs();
        crate::util::lock(&self.snapshot)
            .as_ref()
            .filter(|(loaded_at, _)| loaded_at.saturating_add(SNAPSHOT_MAX_AGE) > now)
            .map(|(_, index)| Arc::clone(index))
    }

    async fn index(&self, budget: &mut Budget) -> Result<Option<Arc<Index>>, String> {
        if let Some(index) = self.current() {
            return Ok(Some(index));
        }
        let mut records = Vec::new();
        let mut total = None;
        loop {
            if total.is_some_and(|total| records.len() >= total) {
                break;
            }
            if !budget.take().await {
                // A stale snapshot is preferable to stopping halfway through a catalogue refresh. It is not
                // written anywhere; the coordinator still refreshes matches on its ordinary schedule.
                return Ok(crate::util::lock(&self.snapshot).as_ref().map(|(_, index)| Arc::clone(index)));
            }
            let mut url = self.endpoint.clone();
            url.query_pairs_mut()
                .append_pair("limit", &PAGE_SIZE.to_string())
                .append_pair("offset", &records.len().to_string());
            let response = self
                .client
                .get(url)
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|error| format!("{}: {error}", self.endpoint))?;
            if !response.status().is_success() {
                return Err(format!("{} answered {}", self.endpoint, response.status()));
            }
            let payload: ImdaResponse =
                response.json().await.map_err(|error| format!("{}: {error}", self.endpoint))?;
            if !payload.success || total.is_some_and(|expected| expected != payload.result.total) {
                return Err(format!("{} returned an inconsistent dataset snapshot", self.endpoint));
            }
            total = Some(payload.result.total);
            if payload.result.records.is_empty() && records.len() < payload.result.total {
                return Err(format!(
                    "{} stopped after {} of {} rows; refusing a partial classification snapshot",
                    self.endpoint,
                    records.len(),
                    payload.result.total
                ));
            }
            records.extend(payload.result.records);
        }
        if records.len() != total.unwrap_or_default() {
            return Err(format!(
                "{} returned {} of {} rows; refusing a partial classification snapshot",
                self.endpoint,
                records.len(),
                total.unwrap_or_default()
            ));
        }
        let index = Arc::new(build_index(records));
        *crate::util::lock(&self.snapshot) = Some((now_secs(), Arc::clone(&index)));
        Ok(Some(index))
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ImdaRow {
    #[serde(rename = "_id")]
    id: u64,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Romanised_Title")]
    romanised_title: Option<String>,
    #[serde(rename = "Also_Known_As")]
    also_known_as: Option<String>,
    #[serde(rename = "Rating")]
    rating: Option<String>,
    #[serde(rename = "Consumer_Advice")]
    consumer_advice: Option<String>,
    #[serde(rename = "Episode_No")]
    episode_no: Option<String>,
    #[serde(rename = "Episode_Title")]
    episode_title: Option<String>,
}

#[derive(Deserialize)]
struct ImdaResponse {
    success: bool,
    result: ImdaResult,
}

#[derive(Deserialize)]
struct ImdaResult {
    total: usize,
    records: Vec<ImdaRow>,
}

fn text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty() && *value != "-")
}

fn media_type(row: &ImdaRow) -> MediaType {
    if text(row.episode_no.as_deref()).is_some() || text(row.episode_title.as_deref()).is_some() {
        MediaType::Tv
    } else {
        MediaType::Movie
    }
}

fn build_index(rows: Vec<ImdaRow>) -> Index {
    let mut index = Index::new();
    for row in rows {
        if normalized_age(row.rating.as_deref().unwrap_or_default()).is_none() {
            continue;
        }
        let media = media_type(&row);
        let mut keys =
            [Some(row.title.as_str()), row.romanised_title.as_deref(), row.also_known_as.as_deref()]
                .into_iter()
                .flatten()
                .filter_map(|name| text(Some(name)))
                .map(name_key)
                .filter(|key| !key.is_empty())
                .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        for key in keys {
            index.entry((media, key)).or_default().push(row.clone());
        }
    }
    index
}

fn normalized_age(rating: &str) -> Option<u8> {
    let code = rating.split_whitespace().next()?.trim_end_matches('-');
    match code {
        "G" | "PG" => Some(0),
        "PG13" => Some(13),
        "NC16" => Some(16),
        "M18" => Some(18),
        "R21" => Some(21),
        _ => None,
    }
}

fn matches(identity: &Identity, index: &Index) -> Vec<RuntimeRating> {
    let mut rows = HashMap::new();
    for candidate in identity.names.iter().filter(|name| identity.candidate_is_unique(name)) {
        if let Some(found) = index.get(&(identity.key.0, name_key(candidate))) {
            for row in found {
                rows.insert(row.id, row);
            }
        }
    }
    let fetched_at = now_secs();
    let mut ratings = rows
        .into_values()
        .filter_map(|row| {
            let rating = row.rating.as_deref()?;
            Some(RuntimeRating {
                authority: "IMDA".to_owned(),
                authority_id: format!("{DATASET}:{}", row.id),
                jurisdiction: "SG".to_owned(),
                rating: rating.to_owned(),
                normalized_age: normalized_age(rating)?,
                warnings: text(row.consumer_advice.as_deref())
                    .map_or_else(Vec::new, |value| vec![value.to_owned()]),
                source_url: SOURCE.to_owned(),
                fetched_at,
            })
        })
        .collect::<Vec<_>>();
    ratings.sort_by(|a, b| a.authority_id.cmp(&b.authority_id));
    ratings
}

#[async_trait]
impl Backend for Imda {
    fn authority(&self) -> &'static str {
        "IMDA"
    }

    fn media_types(&self) -> &'static [MediaType] {
        &[MediaType::Movie, MediaType::Tv]
    }

    fn applies_to(&self, _viewer_region: Option<Region>) -> bool {
        true
    }

    fn can_lookup_without_request(&self) -> bool {
        crate::util::lock(&self.snapshot).is_some()
    }

    async fn lookup(&self, identity: &Identity, budget: &mut Budget) -> Result<Lookup, String> {
        let Some(index) = self.index(budget).await? else { return Ok(Lookup::Deferred) };
        Ok(Lookup::Complete(matches(identity, &index)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn identity(media: MediaType, name: &str) -> Identity {
        let key = (media, 1);
        Identity {
            key,
            year: Some(2023),
            names: vec![name.to_owned()],
            unique: Arc::new(HashMap::from([(name_key(name), HashSet::from([key]))])),
        }
    }

    fn row(id: u64, title: &str, rating: &str, episodes: Option<&str>) -> ImdaRow {
        ImdaRow {
            id,
            title: title.to_owned(),
            romanised_title: None,
            also_known_as: None,
            rating: Some(rating.to_owned()),
            consumer_advice: Some("Some Violence".to_owned()),
            episode_no: episodes.map(str::to_owned),
            episode_title: None,
        }
    }

    #[test]
    fn whole_films_and_episode_ranges_are_kept_in_separate_media_namespaces() {
        let index = build_index(vec![
            row(1, "Same Name", "PG - Parental Guidance", None),
            row(2, "Same Name", "NC16 - No Children Below 16", Some("1 TO 10")),
        ]);
        let movie = matches(&identity(MediaType::Movie, "Same Name"), &index);
        let tv = matches(&identity(MediaType::Tv, "Same Name"), &index);
        assert_eq!(movie.iter().map(|rating| rating.normalized_age).collect::<Vec<_>>(), [0]);
        assert_eq!(tv.iter().map(|rating| rating.normalized_age).collect::<Vec<_>>(), [16]);
    }

    #[test]
    fn singapore_codes_are_normalized_without_turning_pg_into_a_hard_age() {
        assert_eq!(normalized_age("G - General"), Some(0));
        assert_eq!(normalized_age("PG - Parental Guidance"), Some(0));
        assert_eq!(normalized_age("PG13 - Parental Guidance 13"), Some(13));
        assert_eq!(normalized_age("NC16 - No Children Below 16"), Some(16));
        assert_eq!(normalized_age("M18 - Mature 18"), Some(18));
        assert_eq!(normalized_age("R21 - Restricted 21"), Some(21));
    }
}
