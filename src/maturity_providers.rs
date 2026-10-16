//! Runtime maturity authorities kept on the atlas box, never in den-dataset or a downloadable artifact.
//!
//! A provider resolves the corpus identities it understands and returns normalized, attributed observations.
//! The coordinator owns the common operational policy: a daily request ceiling, pacing, refresh age, negative
//! caching, atomic persistence under `CACHE_DIR`, and applicability rules independent of an authority's
//! jurisdiction. Adding another authority should only require another `Backend`; it must not grow another cache
//! or another policy path.

use crate::maturity::Region;
use crate::store::MappedStore;
use async_trait::async_trait;
use den_index::MediaType;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

type Key = (MediaType, u32);
const FILE: &str = "maturity-providers.v2.json";
const SCHEMA: u32 = 2;
const DAY: u64 = 86_400;
const REFRESH_AFTER: u64 = 30 * DAY;
const RETRY_AFTER: u64 = 3600;
const DEFAULT_DAILY_MAX: u32 = 4_000;
const DEFAULT_PACE_MS: u64 = 250;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRating {
    pub authority: String,
    pub authority_id: String,
    pub jurisdiction: String,
    pub rating: String,
    pub normalized_age: u8,
    pub warnings: Vec<String>,
    pub source_url: String,
    pub fetched_at: u64,
}

#[derive(Clone, Debug)]
pub struct Identity {
    key: Key,
    year: Option<i64>,
    names: Vec<String>,
    unique: Arc<HashMap<String, HashSet<Key>>>,
}

impl Identity {
    fn candidate_is_unique(&self, candidate: &str) -> bool {
        self.unique.get(&name_key(candidate)).is_some_and(|keys| {
            keys.contains(&self.key) && keys.iter().filter(|key| key.0 == self.key.0).count() == 1
        })
    }
}

#[derive(Debug)]
pub enum Lookup {
    Complete(Vec<RuntimeRating>),
    Deferred,
}

pub struct Budget {
    left: u32,
    asked: u32,
    pace: Duration,
}

impl Budget {
    pub async fn take(&mut self) -> bool {
        if self.left == 0 {
            return false;
        }
        tokio::time::sleep(self.pace).await;
        self.left -= 1;
        self.asked += 1;
        true
    }
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn authority(&self) -> &'static str;
    fn media_types(&self) -> &'static [MediaType];
    /// Whether this authority's observation is useful for this viewer. This is deliberately separate from the
    /// observation's jurisdiction: an official Finnish classification can still inform a viewer in the US.
    fn applies_to(&self, viewer_region: Option<Region>) -> bool;
    async fn lookup(&self, identity: &Identity, budget: &mut Budget) -> Result<Lookup, String>;
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Cached {
    checked_at: u64,
    ratings: Vec<RuntimeRating>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ProviderState {
    day: u64,
    spent: u32,
    cursor: usize,
    entries: HashMap<String, Cached>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheFile {
    schema: u32,
    providers: HashMap<String, ProviderState>,
}

impl Default for CacheFile {
    fn default() -> Self {
        Self { schema: SCHEMA, providers: HashMap::new() }
    }
}

struct Configured {
    backend: Arc<dyn Backend>,
    daily_max: u32,
    pace: Duration,
}

pub struct Providers {
    catalog: Arc<[Identity]>,
    configured: Vec<Configured>,
    state: Mutex<CacheFile>,
    file: Option<PathBuf>,
    generation: AtomicU64,
}

pub struct Coverage {
    pub authority: &'static str,
    pub media_type: MediaType,
    pub catalog: usize,
    pub checked: usize,
    pub matched: usize,
}

impl Providers {
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            catalog: Arc::from([]),
            configured: Vec::new(),
            state: Mutex::new(CacheFile::default()),
            file: None,
            generation: AtomicU64::new(0),
        })
    }

    pub fn new(
        store: &MappedStore,
        cache_dir: Option<&Path>,
        enabled: &str,
        daily_max: u32,
        pace: Duration,
    ) -> Result<Arc<Self>, String> {
        let names: HashSet<String> = enabled
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let mut configured = Vec::new();
        if names.contains("kavi") {
            configured.push(Configured { backend: Arc::new(Kavi::new()?), daily_max, pace });
        }
        let unknown: Vec<&String> = names.iter().filter(|name| name.as_str() != "kavi").collect();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown MATURITY_PROVIDERS: {}",
                unknown.into_iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if configured.is_empty() {
            return Ok(Self::disabled());
        }
        let Some(cache_dir) = cache_dir else {
            return Err(
                "MATURITY_PROVIDERS requires CACHE_DIR; runtime authority data must survive restarts".into(),
            );
        };
        let catalog = identities(store)?;
        let file = Some(cache_dir.join(FILE));
        let state = file.as_deref().map(read_cache).transpose()?.unwrap_or_default();
        Ok(Arc::new(Self {
            catalog: catalog.into(),
            configured,
            state: Mutex::new(state),
            file,
            generation: AtomicU64::new(1),
        }))
    }

    pub fn enabled(&self) -> bool {
        !self.configured.is_empty()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn applies_to(&self, region: Option<Region>) -> bool {
        self.configured.iter().any(|provider| provider.backend.applies_to(region))
    }

    pub fn ratings(&self, key: Key, region: Option<Region>) -> Vec<RuntimeRating> {
        let state = crate::util::lock(&self.state);
        self.configured
            .iter()
            .filter(|provider| provider.backend.applies_to(region))
            .filter_map(|provider| state.providers.get(provider.backend.authority()))
            .filter_map(|provider| provider.entries.get(&key_string(key)))
            .flat_map(|cached| cached.ratings.iter().cloned())
            .collect()
    }

    pub fn minimum_age(&self, key: Key, region: Option<Region>) -> Option<u8> {
        self.ratings(key, region).into_iter().map(|rating| rating.normalized_age).max()
    }

    pub fn coverage(&self) -> Vec<Coverage> {
        let state = crate::util::lock(&self.state);
        self.configured
            .iter()
            .flat_map(|configured| {
                configured.backend.media_types().iter().copied().map(|media_type| {
                    let authority = configured.backend.authority();
                    let prefix = if media_type == MediaType::Tv { "tv:" } else { "movie:" };
                    let entries = state.providers.get(authority).map(|provider| &provider.entries);
                    Coverage {
                        authority,
                        media_type,
                        catalog: self.catalog.iter().filter(|identity| identity.key.0 == media_type).count(),
                        checked: entries.map_or(0, |entries| {
                            entries.keys().filter(|key| key.starts_with(prefix)).count()
                        }),
                        matched: entries.map_or(0, |entries| {
                            entries
                                .iter()
                                .filter(|(key, cached)| key.starts_with(prefix) && !cached.ratings.is_empty())
                                .count()
                        }),
                    }
                })
            })
            .collect()
    }

    pub async fn refresh_forever(self: Arc<Self>) {
        if !self.enabled() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            for configured in &self.configured {
                eprintln!("{}", self.refresh(configured).await);
            }
            tokio::time::sleep(Duration::from_secs(RETRY_AFTER)).await;
        }
    }

    async fn refresh(&self, configured: &Configured) -> String {
        let authority = configured.backend.authority();
        let now = now_secs();
        if self.catalog.is_empty() {
            return format!("maturity {authority}: no series in the corpus");
        }
        let (mut cursor, spent) = {
            let mut state = crate::util::lock(&self.state);
            let provider = state.providers.entry(authority.to_owned()).or_default();
            if provider.day != now / DAY {
                provider.day = now / DAY;
                provider.spent = 0;
            }
            (provider.cursor % self.catalog.len(), provider.spent)
        };
        let mut budget =
            Budget { left: configured.daily_max.saturating_sub(spent), asked: 0, pace: configured.pace };
        let mut checked = 0usize;
        let mut matched = 0usize;
        let mut changed = false;
        let total = self.catalog.len();
        let mut visited = 0usize;
        while visited < total && budget.left > 0 {
            let identity = &self.catalog[cursor];
            visited += 1;
            if !configured.backend.media_types().contains(&identity.key.0) {
                cursor = (cursor + 1) % total;
                continue;
            }
            let fresh = {
                let state = crate::util::lock(&self.state);
                state
                    .providers
                    .get(authority)
                    .and_then(|provider| provider.entries.get(&key_string(identity.key)))
                    .is_some_and(|cached| cached.checked_at + REFRESH_AFTER > now)
            };
            if fresh {
                cursor = (cursor + 1) % total;
                continue;
            }
            match configured.backend.lookup(identity, &mut budget).await {
                Ok(Lookup::Complete(ratings)) => {
                    checked += 1;
                    matched += usize::from(!ratings.is_empty());
                    let mut state = crate::util::lock(&self.state);
                    let provider = state.providers.entry(authority.to_owned()).or_default();
                    let key = key_string(identity.key);
                    changed |= provider.entries.get(&key).is_none_or(|old| old.ratings != ratings);
                    provider.entries.insert(key, Cached { checked_at: now, ratings });
                    cursor = (cursor + 1) % total;
                }
                Ok(Lookup::Deferred) => break,
                Err(error) => {
                    self.finish_refresh(authority, cursor, &budget, changed);
                    return format!("maturity {authority}: refresh stopped after {checked} titles ({error})");
                }
            }
        }
        self.finish_refresh(authority, cursor, &budget, changed);
        format!(
            "maturity {authority}: {checked} titles checked, {matched} matched, {} requests, {} left today, {} cached",
            budget.asked,
            budget.left,
            crate::util::lock(&self.state)
                .providers
                .get(authority)
                .map_or(0, |provider| provider.entries.len())
        )
    }

    fn finish_refresh(&self, authority: &str, cursor: usize, budget: &Budget, changed: bool) {
        {
            let mut state = crate::util::lock(&self.state);
            let provider = state.providers.entry(authority.to_owned()).or_default();
            provider.cursor = cursor;
            provider.spent = provider.spent.saturating_add(budget.asked);
        }
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        if let (Some(file), Ok(body)) = (&self.file, serde_json::to_vec(&*crate::util::lock(&self.state))) {
            if let Err(error) = write_atomically(file, &body) {
                eprintln!("maturity providers: could not write {} ({error})", file.display());
            }
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
}

fn key_string((media, id): Key) -> String {
    format!("{}:{id}", if media == MediaType::Tv { "tv" } else { "movie" })
}

fn read_cache(path: &Path) -> Result<CacheFile, String> {
    match std::fs::read(path) {
        Ok(body) => {
            let parsed: CacheFile =
                serde_json::from_slice(&body).map_err(|e| format!("{}: {e}", path.display()))?;
            if parsed.schema != SCHEMA {
                return Err(format!("{}: unsupported schema {}", path.display(), parsed.schema));
            }
            Ok(parsed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CacheFile::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn write_atomically(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, body)?;
    std::fs::rename(partial, path)
}

fn identities(store: &MappedStore) -> Result<Vec<Identity>, String> {
    let view = store.view();
    let err = |error: den_store::StoreError| error.to_string();
    let keys = view.per_row::<u64>("keys").map_err(err)?;
    let card_titles = view.per_row::<u32>("card_title").map_err(err)?;
    let years = view.per_row::<i16>("card_year").map_err(err)?;
    let aliases = view.list::<u32>("alias_titles_v", "alias_titles_o").map_err(err)?;
    let strings = view.strings().map_err(err)?;
    let mut raw = Vec::new();
    for (at, &packed) in keys.iter().enumerate() {
        let media_type = match packed >> 32 {
            0 => MediaType::Movie,
            1 => MediaType::Tv,
            _ => continue,
        };
        let row = den_store::Row(at);
        let mut names: Vec<String> = std::iter::once(card_titles[at])
            .chain(aliases.get(row).iter().copied())
            .filter_map(|id| strings.get(id))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        let mut seen = HashSet::new();
        names.retain(|name| seen.insert(name_key(name)));
        names.truncate(12);
        if names.is_empty() {
            continue;
        }
        raw.push((
            (media_type, packed as u32),
            (years[at] != den_store::NONE_I16).then_some(i64::from(years[at])),
            names,
        ));
    }
    raw.sort_by_key(|(key, _, _)| key.1);
    let mut unique: HashMap<String, HashSet<Key>> = HashMap::new();
    for (key, _, names) in &raw {
        for name in names {
            unique.entry(name_key(name)).or_default().insert(*key);
        }
    }
    let unique = Arc::new(unique);
    Ok(raw
        .into_iter()
        .map(|(key, year, names)| Identity { key, year, names, unique: Arc::clone(&unique) })
        .collect())
}

fn name_key(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

struct Kavi {
    client: reqwest::Client,
    base: reqwest::Url,
}

impl Kavi {
    fn new() -> Result<Self, String> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?,
            base: reqwest::Url::parse("https://luokittelu.kavi.fi/agelimit/").map_err(|e| e.to_string())?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KaviRow {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    name_fi: Option<String>,
    name_sv: Option<String>,
    year: Option<i64>,
    agelimit: Option<u8>,
    #[serde(default)]
    warnings: Vec<String>,
}

fn one_kavi_match(identity: &Identity, candidate: &str, rows: Vec<KaviRow>) -> Result<Option<KaviRow>, ()> {
    let wanted = name_key(candidate);
    let mut matched: Vec<KaviRow> = rows
        .into_iter()
        .filter(|row| row.kind == "series")
        .filter(|row| row.year.zip(identity.year).is_none_or(|(a, b)| a == b))
        .filter(|row| {
            [Some(row.name.as_str()), row.name_fi.as_deref(), row.name_sv.as_deref()]
                .into_iter()
                .flatten()
                .any(|name| name_key(name) == wanted)
        })
        .collect();
    matched.sort_by_key(|row| row.id);
    matched.dedup_by_key(|row| row.id);
    match matched.len() {
        0 => Ok(None),
        1 => Ok(matched.pop()),
        _ => Err(()),
    }
}

#[async_trait]
impl Backend for Kavi {
    fn authority(&self) -> &'static str {
        "KAVI"
    }

    fn media_types(&self) -> &'static [MediaType] {
        &[MediaType::Tv]
    }

    fn applies_to(&self, _viewer_region: Option<Region>) -> bool {
        true
    }

    async fn lookup(&self, identity: &Identity, budget: &mut Budget) -> Result<Lookup, String> {
        for candidate in &identity.names {
            if !identity.candidate_is_unique(candidate) {
                continue;
            }
            if !budget.take().await {
                return Ok(Lookup::Deferred);
            }
            let mut url = self.base.clone();
            url.path_segments_mut()
                .map_err(|()| "KAVI base URL cannot take a path".to_owned())?
                .push(candidate);
            url.query_pairs_mut().append_pair("type", "series").append_pair("count", "100");
            let response = self
                .client
                .get(url.clone())
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|e| format!("{}: {e}", url))?;
            if !response.status().is_success() {
                return Err(format!("{} answered {}", url, response.status()));
            }
            let rows: Vec<KaviRow> = response.json().await.map_err(|e| format!("{}: {e}", url))?;
            let row = match one_kavi_match(identity, candidate, rows) {
                Ok(Some(row)) => row,
                Ok(None) => continue,
                Err(()) => return Ok(Lookup::Complete(Vec::new())),
            };
            let Some(age) = row.agelimit.filter(|age| matches!(age, 0 | 7 | 12 | 16 | 18)) else {
                return Ok(Lookup::Complete(Vec::new()));
            };
            return Ok(Lookup::Complete(vec![RuntimeRating {
                authority: self.authority().to_owned(),
                authority_id: format!("iklu:{}", row.id),
                jurisdiction: "FI".to_owned(),
                rating: age.to_string(),
                normalized_age: age,
                warnings: row.warnings,
                source_url: format!("https://luokittelu.kavi.fi/agelimit/{}", row.id),
                fetched_at: now_secs(),
            }]));
        }
        Ok(Lookup::Complete(Vec::new()))
    }
}

pub fn defaults_from_env() -> (u32, Duration) {
    let daily = std::env::var("MATURITY_DAILY_MAX")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_DAILY_MAX);
    let pace = std::env::var("MATURITY_PACE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PACE_MS);
    (daily, Duration::from_millis(pace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_keys_fold_case_marks_spaces_and_punctuation() {
        assert_eq!(name_key("Pippi Långstrump"), "pippilangstrump");
        assert_eq!(name_key("PIPPI-LANGSTRUMP"), "pippilangstrump");
    }

    #[test]
    fn cache_refuses_an_unknown_schema() {
        let dir = std::env::temp_dir().join(format!("den-atlas-maturity-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        std::fs::write(&path, br#"{"schema":3,"providers":{}}"#).unwrap();
        assert!(read_cache(&path).unwrap_err().contains("unsupported schema 3"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn pippi() -> Identity {
        let key = (MediaType::Tv, 3714);
        let mut unique = HashMap::new();
        unique.insert(name_key("Pippi Långstrump"), HashSet::from([key]));
        Identity {
            key,
            year: Some(1969),
            names: vec!["Pippi Långstrump".to_owned()],
            unique: Arc::new(unique),
        }
    }

    #[test]
    fn kavi_matches_the_unique_pippi_series_not_an_episode_or_another_year() {
        let rows: Vec<KaviRow> = serde_json::from_str(
            r#"[
                {"id":58417,"type":"series","name":"Pippi Långstrump","year":null,"agelimit":7,"warnings":["anxiety"]},
                {"id":111435,"type":"episode","name":"Pippi Långstrump","year":1969,"agelimit":7},
                {"id":9,"type":"series","name":"Pippi Långstrump","year":1997,"agelimit":7}
            ]"#,
        )
        .unwrap();
        let row = one_kavi_match(&pippi(), "Pippi Långstrump", rows).unwrap().unwrap();
        assert_eq!(row.id, 58417);
        assert_eq!(row.agelimit, Some(7));
        assert_eq!(row.warnings, ["anxiety"]);
    }

    #[test]
    fn an_authoritys_jurisdiction_does_not_limit_where_its_rating_is_useful() {
        struct Fake;
        #[async_trait]
        impl Backend for Fake {
            fn authority(&self) -> &'static str {
                "FAKE"
            }
            fn media_types(&self) -> &'static [MediaType] {
                &[MediaType::Tv]
            }
            fn applies_to(&self, _viewer_region: Option<Region>) -> bool {
                true
            }
            async fn lookup(&self, _: &Identity, _: &mut Budget) -> Result<Lookup, String> {
                unreachable!()
            }
        }
        let key = (MediaType::Tv, 3714);
        let rating = RuntimeRating {
            authority: "FAKE".into(),
            authority_id: "record:1".into(),
            jurisdiction: "FI".into(),
            rating: "7".into(),
            normalized_age: 7,
            warnings: vec!["anxiety".into()],
            source_url: "https://example.test/1".into(),
            fetched_at: 1,
        };
        let mut cache = CacheFile::default();
        cache.providers.insert(
            "FAKE".into(),
            ProviderState {
                entries: HashMap::from([(key_string(key), Cached { checked_at: 1, ratings: vec![rating] })]),
                ..ProviderState::default()
            },
        );
        let providers = Providers {
            catalog: Arc::from([]),
            configured: vec![Configured { backend: Arc::new(Fake), daily_max: 1, pace: Duration::ZERO }],
            state: Mutex::new(cache),
            file: None,
            generation: AtomicU64::new(1),
        };
        assert_eq!(providers.minimum_age(key, Some(Region::parse("fi").unwrap())), Some(7));
        assert_eq!(providers.minimum_age(key, Some(Region::parse("us").unwrap())), Some(7));
        assert_eq!(providers.minimum_age(key, None), Some(7));
        let coverage = providers.coverage();
        assert_eq!(coverage.len(), 1);
        assert_eq!(coverage[0].checked, 1);
        assert_eq!(coverage[0].matched, 1);
        let json = serde_json::to_string(&*crate::util::lock(&providers.state)).unwrap();
        assert!(json.contains("record:1"));
        assert!(!json.contains("Pippi"), "the operational cache stores no source title or raw response");
    }
}
