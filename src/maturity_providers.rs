//! Runtime maturity authorities kept on the atlas box, never in den-dataset or a downloadable artifact.
//!
//! A provider resolves the corpus identities it understands and returns normalized, attributed observations.
//! The coordinator owns the common operational policy: a daily request ceiling, pacing, refresh age, negative
//! caching, atomic persistence under `CACHE_DIR`, and applicability rules independent of an authority's
//! jurisdiction. Adding another authority should only require another `Backend`; it must not grow another cache
//! or another policy path.

use crate::maturity::Region;
use crate::store::MappedStore;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use den_index::MediaType;
use reqwest::Response;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

mod imda;
mod kmrb;

type Key = (MediaType, u32);
const FILE: &str = "maturity-providers.v2.json";
const SCHEMA: u32 = 2;
const DAY: u64 = 86_400;
const REFRESH_AFTER: u64 = 30 * DAY;
const RETRY_AFTER: u64 = 3600;
const DEFAULT_DAILY_MAX: u32 = 4_000;
const DEFAULT_PACE_MS: u64 = 250;
const MAX_KAVI_BODY: usize = 2 * 1024 * 1024;

fn user_agent() -> String {
    format!("den-atlas/{} (+https://github.com/oxyc/den-atlas)", crate::release_version())
}

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
    /// A bulk provider may answer more identities from an already loaded snapshot after its request budget is
    /// exhausted. Per-title providers keep the default: no budget means no useful work remains.
    fn can_lookup_without_request(&self) -> bool {
        false
    }
    fn retry_in(&self) -> Option<Duration> {
        None
    }
    fn snapshot_age(&self) -> Option<Duration> {
        None
    }
    fn persists_http_no_store(&self) -> bool {
        false
    }
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
    requests: AtomicU64,
    errors: AtomicU64,
    last_success: AtomicU64,
    last_error: AtomicU64,
    last_refresh_ms: AtomicU64,
}

struct InitFailure {
    authority: &'static str,
    at: u64,
}

impl Configured {
    fn new(backend: Arc<dyn Backend>, daily_max: u32, pace: Duration) -> Self {
        Self {
            backend,
            daily_max,
            pace,
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_success: AtomicU64::new(0),
            last_error: AtomicU64::new(0),
            last_refresh_ms: AtomicU64::new(0),
        }
    }
}

#[derive(Default)]
struct ReadView {
    providers: Vec<ProviderView>,
    coverage: Vec<Coverage>,
}

struct ProviderView {
    minimum_ages: HashMap<Key, u8>,
    ratings: HashMap<Key, Arc<[RuntimeRating]>>,
}

pub struct Providers {
    catalog: Arc<[Identity]>,
    configured: Vec<Configured>,
    init_failures: Vec<InitFailure>,
    state: Mutex<CacheFile>,
    view: ArcSwap<ReadView>,
    file: Option<PathBuf>,
    generation: AtomicU64,
    dirty: AtomicBool,
    cache_writes: AtomicU64,
    cache_write_bytes: AtomicU64,
    cache_write_errors: AtomicU64,
    cache_bytes: AtomicU64,
    last_cache_write_ms: AtomicU64,
}

#[derive(Clone)]
pub struct Coverage {
    pub authority: &'static str,
    pub media_type: MediaType,
    pub catalog: usize,
    pub checked: usize,
    pub matched: usize,
}

pub struct ProviderStatus {
    pub authority: &'static str,
    pub available: bool,
    pub persists_http_no_store: bool,
    pub requests: u64,
    pub errors: u64,
    pub last_success: u64,
    pub last_error: u64,
    pub last_refresh_ms: u64,
    pub retry_seconds: u64,
    pub snapshot_age_seconds: u64,
}

pub struct CacheStatus {
    pub writes: u64,
    pub bytes: u64,
    pub errors: u64,
    pub size: u64,
    pub last_write_ms: u64,
}

impl Providers {
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            catalog: Arc::from([]),
            configured: Vec::new(),
            init_failures: Vec::new(),
            state: Mutex::new(CacheFile::default()),
            view: ArcSwap::from_pointee(ReadView::default()),
            file: None,
            generation: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            cache_writes: AtomicU64::new(0),
            cache_write_bytes: AtomicU64::new(0),
            cache_write_errors: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            last_cache_write_ms: AtomicU64::new(0),
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
        let mut init_failures = Vec::new();
        if names.contains("kavi") {
            match Kavi::new() {
                Ok(backend) => configured.push(Configured::new(Arc::new(backend), daily_max, pace)),
                Err(error) => {
                    eprintln!("maturity KAVI disabled ({error})");
                    init_failures.push(InitFailure { authority: "KAVI", at: now_secs() });
                }
            }
        }
        if names.contains("imda") {
            match imda::Imda::new() {
                Ok(backend) => configured.push(Configured::new(Arc::new(backend), daily_max, pace)),
                Err(error) => {
                    eprintln!("maturity IMDA disabled ({error})");
                    init_failures.push(InitFailure { authority: "IMDA", at: now_secs() });
                }
            }
        }
        if names.contains("kmrb") {
            match kmrb::Kmrb::from_env() {
                Ok(backend) => configured.push(Configured::new(Arc::new(backend), daily_max, pace)),
                Err(error) => {
                    eprintln!("maturity KMRB disabled ({error})");
                    init_failures.push(InitFailure { authority: "KMRB", at: now_secs() });
                }
            }
        }
        let unknown: Vec<&String> =
            names.iter().filter(|name| !matches!(name.as_str(), "kavi" | "imda" | "kmrb")).collect();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown MATURITY_PROVIDERS: {}",
                unknown.into_iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if configured.is_empty() {
            let mut providers = Self::disabled();
            Arc::get_mut(&mut providers).expect("newly constructed provider state is unique").init_failures =
                init_failures;
            return Ok(providers);
        }
        let Some(cache_dir) = cache_dir else {
            return Err(
                "MATURITY_PROVIDERS requires CACHE_DIR; runtime authority data must survive restarts".into(),
            );
        };
        let catalog = identities(store)?;
        let file = Some(cache_dir.join(FILE));
        let mut state = file.as_deref().map(read_cache_recovering).unwrap_or_default();
        let cache_bytes = file
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map_or(0, |metadata| metadata.len());
        let pruned = prune_cache(&mut state, &names, &catalog);
        let view = build_view(&state, &configured, &catalog);
        let providers = Arc::new(Self {
            catalog: catalog.into(),
            configured,
            init_failures,
            state: Mutex::new(state),
            view: ArcSwap::from_pointee(view),
            file,
            generation: AtomicU64::new(1),
            dirty: AtomicBool::new(pruned),
            cache_writes: AtomicU64::new(0),
            cache_write_bytes: AtomicU64::new(0),
            cache_write_errors: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(cache_bytes),
            last_cache_write_ms: AtomicU64::new(0),
        });
        if pruned {
            providers.persist_if_dirty();
        }
        Ok(providers)
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
        let view = self.view.load();
        view.providers
            .iter()
            .zip(&self.configured)
            .filter(|(_, configured)| configured.backend.applies_to(region))
            .map(|(provider, _)| provider)
            .filter_map(|provider| provider.ratings.get(&key))
            .flat_map(|ratings| ratings.iter().cloned())
            .collect()
    }

    pub fn minimum_age(&self, key: Key, region: Option<Region>) -> Option<u8> {
        let view = self.view.load();
        view.providers
            .iter()
            .zip(&self.configured)
            .filter(|(_, configured)| configured.backend.applies_to(region))
            .map(|(provider, _)| provider)
            .filter_map(|provider| provider.minimum_ages.get(&key).copied())
            .max()
    }

    pub fn coverage(&self) -> Vec<Coverage> {
        self.view.load().coverage.clone()
    }

    pub fn status(&self) -> Vec<ProviderStatus> {
        self.configured
            .iter()
            .map(|configured| ProviderStatus {
                authority: configured.backend.authority(),
                available: true,
                persists_http_no_store: configured.backend.persists_http_no_store(),
                requests: configured.requests.load(Ordering::Relaxed),
                errors: configured.errors.load(Ordering::Relaxed),
                last_success: configured.last_success.load(Ordering::Relaxed),
                last_error: configured.last_error.load(Ordering::Relaxed),
                last_refresh_ms: configured.last_refresh_ms.load(Ordering::Relaxed),
                retry_seconds: configured.backend.retry_in().map_or(0, |wait| wait.as_secs().max(1)),
                snapshot_age_seconds: configured.backend.snapshot_age().map_or(0, |age| age.as_secs()),
            })
            .chain(self.init_failures.iter().map(|failure| ProviderStatus {
                authority: failure.authority,
                available: false,
                persists_http_no_store: false,
                requests: 0,
                errors: 1,
                last_success: 0,
                last_error: failure.at,
                last_refresh_ms: 0,
                retry_seconds: 0,
                snapshot_age_seconds: 0,
            }))
            .collect()
    }

    pub fn cache_status(&self) -> CacheStatus {
        CacheStatus {
            writes: self.cache_writes.load(Ordering::Relaxed),
            bytes: self.cache_write_bytes.load(Ordering::Relaxed),
            errors: self.cache_write_errors.load(Ordering::Relaxed),
            size: self.cache_bytes.load(Ordering::Relaxed),
            last_write_ms: self.last_cache_write_ms.load(Ordering::Relaxed),
        }
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
            self.persist_if_dirty();
            let retry = self
                .configured
                .iter()
                .filter_map(|configured| configured.backend.retry_in())
                .fold(Duration::from_secs(RETRY_AFTER), Duration::min);
            tokio::time::sleep(retry.max(Duration::from_secs(1))).await;
        }
    }

    async fn refresh(&self, configured: &Configured) -> String {
        let started = std::time::Instant::now();
        let authority = configured.backend.authority();
        let now = now_secs();
        if self.catalog.is_empty() {
            return format!("maturity {authority}: no titles in the corpus");
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
        while visited < total && (budget.left > 0 || configured.backend.can_lookup_without_request()) {
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
                    if let Some(old) =
                        provider.entries.get_mut(&key).filter(|old| same_ratings(&old.ratings, &ratings))
                    {
                        // `fetched_at` is presentation provenance, not cache freshness. Preserve it when the
                        // authority's facts are unchanged so ETags and generations do not churn every sweep.
                        old.checked_at = now;
                    } else {
                        changed = true;
                        provider.entries.insert(key, Cached { checked_at: now, ratings });
                    }
                    self.dirty.store(true, Ordering::Relaxed);
                    cursor = (cursor + 1) % total;
                }
                Ok(Lookup::Deferred) => break,
                Err(error) => {
                    self.finish_refresh(authority, cursor, &budget, changed);
                    configured.requests.fetch_add(u64::from(budget.asked), Ordering::Relaxed);
                    configured.errors.fetch_add(1, Ordering::Relaxed);
                    configured.last_error.store(now_secs(), Ordering::Relaxed);
                    configured.last_refresh_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    return format!("maturity {authority}: refresh stopped after {checked} titles ({error})");
                }
            }
        }
        self.finish_refresh(authority, cursor, &budget, changed);
        configured.requests.fetch_add(u64::from(budget.asked), Ordering::Relaxed);
        if checked > 0 {
            configured.last_success.store(now_secs(), Ordering::Relaxed);
        }
        configured.last_refresh_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
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
        if budget.asked > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
            let state = crate::util::lock(&self.state);
            self.view.store(Arc::new(build_view(&state, &self.configured, &self.catalog)));
        }
    }

    fn persist_if_dirty(&self) {
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        let Some(file) = &self.file else { return };
        let started = std::time::Instant::now();
        let body = match serde_json::to_vec(&*crate::util::lock(&self.state)) {
            Ok(body) => body,
            Err(error) => {
                self.cache_write_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("maturity providers: could not serialize cache ({error})");
                return;
            }
        };
        match write_atomically(file, &body) {
            Ok(()) => {
                self.dirty.store(false, Ordering::Relaxed);
                self.cache_writes.fetch_add(1, Ordering::Relaxed);
                self.cache_write_bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                self.cache_bytes.store(body.len() as u64, Ordering::Relaxed);
                self.last_cache_write_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            Err(error) => {
                self.cache_write_errors.fetch_add(1, Ordering::Relaxed);
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

fn parse_key(value: &str) -> Option<Key> {
    let (kind, id) = value.split_once(':')?;
    let media = match kind {
        "tv" => MediaType::Tv,
        "movie" => MediaType::Movie,
        _ => return None,
    };
    Some((media, id.parse().ok()?))
}

fn same_ratings(old: &[RuntimeRating], new: &[RuntimeRating]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(old, new)| {
            old.authority == new.authority
                && old.authority_id == new.authority_id
                && old.jurisdiction == new.jurisdiction
                && old.rating == new.rating
                && old.normalized_age == new.normalized_age
                && old.warnings == new.warnings
                && old.source_url == new.source_url
        })
}

fn build_view(state: &CacheFile, configured: &[Configured], catalog: &[Identity]) -> ReadView {
    let mut providers = Vec::with_capacity(configured.len());
    let mut coverage = Vec::new();
    for configured in configured {
        let authority = configured.backend.authority();
        let entries = state.providers.get(authority).map(|provider| &provider.entries);
        let mut ratings = HashMap::new();
        let mut minimum_ages = HashMap::new();
        if let Some(entries) = entries {
            for (stored_key, cached) in entries {
                let Some(key) = parse_key(stored_key) else { continue };
                if cached.ratings.is_empty() {
                    continue;
                }
                if let Some(age) = cached.ratings.iter().map(|rating| rating.normalized_age).max() {
                    minimum_ages.insert(key, age);
                }
                ratings.insert(key, Arc::from(cached.ratings.clone()));
            }
        }
        for &media_type in configured.backend.media_types() {
            let (mut checked, mut matched) = (0, 0);
            if let Some(entries) = entries {
                for (stored_key, cached) in entries {
                    if parse_key(stored_key).is_some_and(|key| key.0 == media_type) {
                        checked += 1;
                        matched += usize::from(!cached.ratings.is_empty());
                    }
                }
            }
            coverage.push(Coverage {
                authority,
                media_type,
                catalog: catalog.iter().filter(|identity| identity.key.0 == media_type).count(),
                checked,
                matched,
            });
        }
        providers.push(ProviderView { minimum_ages, ratings });
    }
    ReadView { providers, coverage }
}

fn prune_cache(state: &mut CacheFile, requested: &HashSet<String>, catalog: &[Identity]) -> bool {
    let keys: HashSet<Key> = catalog.iter().map(|identity| identity.key).collect();
    let before_providers = state.providers.len();
    state.providers.retain(|authority, _| requested.contains(&authority.to_ascii_lowercase()));
    let mut changed = state.providers.len() != before_providers;
    for provider in state.providers.values_mut() {
        let before = provider.entries.len();
        provider.entries.retain(|stored_key, _| parse_key(stored_key).is_some_and(|key| keys.contains(&key)));
        changed |= provider.entries.len() != before;
    }
    changed
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

fn read_cache_recovering(path: &Path) -> CacheFile {
    match read_cache(path) {
        Ok(cache) => {
            harden_cache_permissions(path);
            cache
        }
        Err(error) => {
            let quarantine = path.with_extension(format!("json.corrupt-{}", now_secs()));
            match std::fs::rename(path, &quarantine) {
                Ok(()) => eprintln!(
                    "maturity providers: quarantined unreadable cache as {} ({error})",
                    quarantine.display()
                ),
                Err(rename_error) if rename_error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("maturity providers: cache disappeared while reading ({error})")
                }
                Err(rename_error) => eprintln!(
                    "maturity providers: could not quarantine unreadable cache {} ({error}; {rename_error})",
                    path.display()
                ),
            }
            CacheFile::default()
        }
    }
}

#[cfg(unix)]
fn harden_cache_permissions(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else { return };
    if metadata.permissions().mode() & 0o777 == 0o600 {
        return;
    }
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("maturity providers: could not make {} private ({error})", path.display());
    }
}

#[cfg(not(unix))]
fn harden_cache_permissions(_path: &Path) {}

fn write_atomically(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("json.partial");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output = options.open(&partial)?;
    #[cfg(unix)]
    output.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::Write::write_all(&mut output, body)?;
    output.sync_all()?;
    std::fs::rename(partial, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

async fn read_limited(mut response: Response, limit: usize, authority: &str) -> Result<Bytes, String> {
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(format!("{authority} response exceeds {limit} bytes"));
    }
    let mut body = BytesMut::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("{authority} response body: {}", error.without_url()))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(format!("{authority} response exceeds {limit} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn requested_wait(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    crate::util::retry_after(headers).or_else(|| crate::util::exhausted_for(headers))
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
    backoff: crate::util::Backoff,
}

impl Kavi {
    fn new() -> Result<Self, String> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(user_agent())
                .build()
                .map_err(|e| e.to_string())?,
            // No trailing slash: `path_segments_mut().push()` would otherwise preserve its empty final segment
            // and produce `/agelimit//Title`, which KAVI correctly rejects with 404.
            base: reqwest::Url::parse("https://luokittelu.kavi.fi/agelimit").map_err(|e| e.to_string())?,
            backoff: crate::util::Backoff::new(Duration::from_secs(10), Duration::from_secs(RETRY_AFTER)),
        })
    }

    fn search_url(&self, candidate: &str) -> Result<reqwest::Url, String> {
        let mut url = self.base.clone();
        url.path_segments_mut().map_err(|()| "KAVI base URL cannot take a path".to_owned())?.push(candidate);
        url.query_pairs_mut().append_pair("type", "series").append_pair("count", "100");
        Ok(url)
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

    fn retry_in(&self) -> Option<Duration> {
        self.backoff.paused()
    }

    fn persists_http_no_store(&self) -> bool {
        true
    }

    async fn lookup(&self, identity: &Identity, budget: &mut Budget) -> Result<Lookup, String> {
        if self.backoff.paused().is_some() {
            return Ok(Lookup::Deferred);
        }
        for candidate in &identity.names {
            if !identity.candidate_is_unique(candidate) {
                continue;
            }
            if self.backoff.paused().is_some() {
                return Ok(Lookup::Deferred);
            }
            if !budget.take().await {
                return Ok(Lookup::Deferred);
            }
            let url = self.search_url(candidate)?;
            let response =
                self.client.get(url.clone()).header("accept", "application/json").send().await.map_err(
                    |error| {
                        self.backoff.refused(None);
                        format!("KAVI request failed: {}", error.without_url())
                    },
                )?;
            if !response.status().is_success() {
                let status = response.status();
                if crate::util::backs_off(status) {
                    self.backoff.refused(requested_wait(response.headers()));
                } else {
                    self.backoff.answered();
                }
                return Err(format!("KAVI answered {status}"));
            }
            let exhausted_for = crate::util::exhausted_for(response.headers());
            let body = read_limited(response, MAX_KAVI_BODY, "KAVI").await.inspect_err(|_error| {
                self.backoff.refused(None);
            })?;
            let rows: Vec<KaviRow> = serde_json::from_slice(&body).map_err(|error| {
                self.backoff.refused(None);
                format!("KAVI JSON: {error}")
            })?;
            self.backoff.answered();
            if let Some(wait) = exhausted_for {
                self.backoff.refused(Some(wait));
            }
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

    #[cfg(unix)]
    #[test]
    fn an_existing_cache_is_made_private_without_rewriting_it() {
        let dir = std::env::temp_dir().join(format!("den-atlas-maturity-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        std::fs::write(&path, serde_json::to_vec(&CacheFile::default()).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        assert_eq!(read_cache_recovering(&path).schema, SCHEMA);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        assert_eq!(after.len(), before.len());
        assert_eq!(
            after.modified().unwrap(),
            before.modified().unwrap(),
            "chmod must not rewrite cache contents"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unchanged_cache_is_not_written_and_an_actual_write_is_private() {
        let dir = std::env::temp_dir().join(format!("den-atlas-maturity-writes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE);
        let providers = Providers {
            catalog: Arc::from([]),
            configured: Vec::new(),
            init_failures: Vec::new(),
            state: Mutex::new(CacheFile::default()),
            view: ArcSwap::from_pointee(ReadView::default()),
            file: Some(file.clone()),
            generation: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            cache_writes: AtomicU64::new(0),
            cache_write_bytes: AtomicU64::new(0),
            cache_write_errors: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            last_cache_write_ms: AtomicU64::new(0),
        };
        providers.persist_if_dirty();
        assert!(!file.exists());
        providers.dirty.store(true, Ordering::Relaxed);
        providers.persist_if_dirty();
        assert_eq!(providers.cache_status().writes, 1);
        providers.persist_if_dirty();
        assert_eq!(providers.cache_status().writes, 1, "a clean cache must cause no filesystem write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn presentation_fetch_time_does_not_make_an_unchanged_rating_new() {
        let rating = |fetched_at| RuntimeRating {
            authority: "TEST".into(),
            authority_id: "1".into(),
            jurisdiction: "ZZ".into(),
            rating: "12".into(),
            normalized_age: 12,
            warnings: vec!["warning".into()],
            source_url: "https://example.test/1".into(),
            fetched_at,
        };
        assert!(same_ratings(&[rating(1)], &[rating(2)]));
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
    fn kavi_search_url_has_one_path_separator_and_encodes_the_title() {
        let url = Kavi::new().unwrap().search_url("Clerks: The Animated Series").unwrap();
        assert_eq!(
            url.as_str(),
            "https://luokittelu.kavi.fi/agelimit/Clerks:%20The%20Animated%20Series?type=series&count=100"
        );
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
        let configured = vec![Configured::new(Arc::new(Fake), 1, Duration::ZERO)];
        let view = build_view(&cache, &configured, &[]);
        let providers = Providers {
            catalog: Arc::from([]),
            configured,
            init_failures: Vec::new(),
            state: Mutex::new(cache),
            view: ArcSwap::from_pointee(view),
            file: None,
            generation: AtomicU64::new(1),
            dirty: AtomicBool::new(false),
            cache_writes: AtomicU64::new(0),
            cache_write_bytes: AtomicU64::new(0),
            cache_write_errors: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            last_cache_write_ms: AtomicU64::new(0),
        };
        assert_eq!(providers.minimum_age(key, Some(Region::parse("fi").unwrap())), Some(7));
        assert_eq!(providers.minimum_age(key, Some(Region::parse("us").unwrap())), Some(7));
        assert_eq!(providers.minimum_age(key, None), Some(7));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = providers.state.lock().unwrap();
            panic!("poison the persistence lock");
        }));
        assert_eq!(
            providers.minimum_age(key, None),
            Some(7),
            "request reads use the immutable view, not the persistence mutex"
        );
        let coverage = providers.coverage();
        assert_eq!(coverage.len(), 1);
        assert_eq!(coverage[0].checked, 1);
        assert_eq!(coverage[0].matched, 1);
        let json = serde_json::to_string(&*crate::util::lock(&providers.state)).unwrap();
        assert!(json.contains("record:1"));
        assert!(!json.contains("Pippi"), "the operational cache stores no source title or raw response");
    }
}
