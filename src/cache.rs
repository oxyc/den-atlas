//! Tiny in-process TTL cache for the JustWatch catalog rows (docs/CATALOG-justwatch.md). Holds the
//! serialized `metas` JSON per catalog key. Serve-stale-on-error: an expired entry is kept until a
//! successful refresh replaces it, so a JustWatch blip serves the last-good rows instead of going empty.
//!
//! With `CACHE_DIR` the rows are also kept on disk, so a restart — every deploy, the weekly rebuild, a
//! dataset change — serves the rows it had instead of making the next visitor wait on JustWatch for each.
//! The file is written only when a row is stored, never on a timer.

use crate::util::lock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::UNIX_EPOCH;
use std::time::{Duration, Instant, SystemTime};

pub struct TtlCache {
    ttl: Duration,
    max_entries: usize,
    map: Arc<Mutex<HashMap<String, Entry>>>,
    /// Where the rows are kept across restarts; `None` keeps them in memory only.
    file: Option<PathBuf>,
    /// A row changed since the file was last written.
    generation: Arc<AtomicU64>,
    /// A writer is scheduled or running. One fixed batch window bounds both write frequency and delay;
    /// a later generation gets another window instead of being lost behind the active writer.
    saving: Arc<AtomicBool>,
    flush_after: Duration,
    persistence: Arc<CachePersistence>,
    #[cfg(test)]
    before_write: Arc<Mutex<Option<WritePause>>>,
    #[cfg(test)]
    panic_next_write: Arc<AtomicBool>,
}

/// The file under `CACHE_DIR`. A new format takes a new name, so an old file is ignored, not misread.
const FILE: &str = "catalog-rows.v1.json";

/// Catalog rows are disposable upstream cache data. Waiting one second greatly reduces full-map
/// rewrites during a refresh wave while putting a hard ceiling on how long a successful row remains
/// memory-only. This is fixed-window batching, not a sliding debounce, so continuous traffic cannot
/// postpone a flush forever. The detached coordinator is process-bound: a stop or restart does not
/// flush it, so at most this window of disposable cache rows may need fetching again.
const FLUSH_AFTER: Duration = Duration::from_secs(1);

#[derive(Default)]
struct CachePersistence {
    writes: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    last_write_ms: AtomicU64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CachePersistenceStatus {
    pub writes: u64,
    pub bytes: u64,
    pub errors: u64,
    pub last_write_ms: u64,
}

#[cfg(test)]
struct WritePause {
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
}

/// How many rendered rows to keep. Nothing here ever expired an entry — serve-stale reads expired
/// ones, so the map only grew, for the process lifetime. The keyspace is bounded (countries are
/// validated to two letters, provider subsets to 127) but the ceiling is ~190k keys of ~22 KB,
/// roughly 4 GB, reachable unauthenticated by enumerating `country` × provider selection, against a
/// 256 MB container. A real install touches a handful of countries, so this is far above normal use
/// and far below the ceiling.
const DEFAULT_MAX_ENTRIES: usize = 2_000;

/// Stamped with wall-clock time rather than an `Instant`, which means nothing to the next process.
#[derive(Serialize, Deserialize)]
struct Entry {
    stored: SystemTime,
    value: String,
}

/// Drop the oldest entry. Oldest-first rather than least-recently-used because `get` does not touch
/// the entry and making it do so would put a write lock on every read: the value is a rendered row
/// that is replaced on refresh anyway, so age is the closest cheap proxy for "least useful to keep".
/// Only ever called when the map is at its cap.
fn evict_one(map: &mut HashMap<String, Entry>) {
    if let Some(oldest) = map.iter().min_by_key(|(_, e)| e.stored).map(|(k, _)| k.clone()) {
        map.remove(&oldest);
    }
}

/// The result of a lookup: within TTL, expired-but-present, or absent.
pub enum Lookup {
    Fresh(String),
    Stale(String),
    Miss,
}

impl TtlCache {
    /// How many keys are held. Tests use it to prove two lookups landed on DIFFERENT keys, which
    /// is the only way to see a key that silently omits part of what the value depends on.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        lock(&self.map).len()
    }

    pub fn new(ttl: Duration) -> Self {
        Self::with_max_entries(ttl, DEFAULT_MAX_ENTRIES)
    }

    pub fn with_max_entries(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            map: Arc::default(),
            file: None,
            generation: Arc::default(),
            saving: Arc::default(),
            flush_after: FLUSH_AFTER,
            persistence: Arc::default(),
            #[cfg(test)]
            before_write: Arc::default(),
            #[cfg(test)]
            panic_next_write: Arc::default(),
        }
    }

    /// A cache that is also kept in `dir`, starting from what was kept there. A file that is missing
    /// starts it empty; one that can't be read is reported and does the same.
    pub fn persisted(ttl: Duration, dir: &Path) -> Self {
        let file = dir.join(FILE);
        let mut map = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<HashMap<String, Entry>>(&bytes).unwrap_or_else(|e| {
                eprintln!("catalog cache: {} is unreadable ({e}) — starting empty", file.display());
                HashMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                eprintln!("catalog cache: could not read {} ({e}) — starting empty", file.display());
                HashMap::new()
            }
        };
        let cache = Self::new(ttl);
        while map.len() > cache.max_entries {
            evict_one(&mut map);
        }
        *lock(&cache.map) = map;
        Self { file: Some(file), ..cache }
    }

    pub fn persistence_status(&self) -> CachePersistenceStatus {
        CachePersistenceStatus {
            writes: self.persistence.writes.load(Ordering::Relaxed),
            bytes: self.persistence.bytes.load(Ordering::Relaxed),
            errors: self.persistence.errors.load(Ordering::Relaxed),
            last_write_ms: self.persistence.last_write_ms.load(Ordering::Relaxed),
        }
    }

    pub fn get(&self, key: &str) -> Lookup {
        let map = lock(&self.map);
        match map.get(key) {
            // A clock stepped back reads as just stored: fresh, rather than a row that never expires.
            Some(e) if e.stored.elapsed().unwrap_or_default() < self.ttl => Lookup::Fresh(e.value.clone()),
            Some(e) => Lookup::Stale(e.value.clone()),
            None => Lookup::Miss,
        }
    }

    /// How long ago the row was stored; `None` when there is none.
    pub fn age(&self, key: &str) -> Option<Duration> {
        lock(&self.map).get(key).map(|e| e.stored.elapsed().unwrap_or_default())
    }

    pub fn put(&self, key: &str, value: String) {
        {
            let mut map = lock(&self.map);
            if map.len() >= self.max_entries && !map.contains_key(key) {
                evict_one(&mut map);
            }
            map.insert(key.to_owned(), Entry { stored: SystemTime::now(), value });
            self.generation.fetch_add(1, Ordering::Release);
        }
        self.save();
    }

    /// Write the rows to the file off the request path. The first row opens a fixed batch window;
    /// rows stored while it waits or writes are included in that pass or a guaranteed follow-up.
    fn save(&self) {
        let Some(file) = self.file.clone() else { return };
        if self.saving.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.saving.store(false, Ordering::Release);
            return;
        };
        let map = Arc::clone(&self.map);
        let generation = Arc::clone(&self.generation);
        let saving = Arc::clone(&self.saving);
        let persistence = Arc::clone(&self.persistence);
        let flush_after = self.flush_after;
        #[cfg(test)]
        let before_write = Arc::clone(&self.before_write);
        #[cfg(test)]
        let panic_next_write = Arc::clone(&self.panic_next_write);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(flush_after).await;
                let map = Arc::clone(&map);
                let writer_generation = Arc::clone(&generation);
                let writer_file = file.clone();
                let writer_persistence = Arc::clone(&persistence);
                #[cfg(test)]
                let before_write = Arc::clone(&before_write);
                #[cfg(test)]
                let panic_next_write = Arc::clone(&panic_next_write);
                let written = tokio::task::spawn_blocking(move || {
                    let started = Instant::now();
                    // Read the generation while holding the map lock. A put is therefore either in this
                    // snapshot or has a strictly later generation that forces another pass.
                    let (body, snapshot_generation) = {
                        let map = lock(&map);
                        (serde_json::to_vec(&*map), writer_generation.load(Ordering::Acquire))
                    };
                    #[cfg(test)]
                    if let Some(pause) = lock(&before_write).take() {
                        pause.entered.wait();
                        pause.release.wait();
                    }
                    #[cfg(test)]
                    if panic_next_write.swap(false, Ordering::AcqRel) {
                        panic!("injected catalog cache writer failure");
                    }
                    match body.map_err(std::io::Error::other).and_then(|body| {
                        write_atomically(&writer_file, &body)?;
                        Ok(body.len())
                    }) {
                        Ok(bytes) => {
                            writer_persistence.writes.fetch_add(1, Ordering::Relaxed);
                            writer_persistence.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                            writer_persistence
                                .last_write_ms
                                .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                        }
                        Err(error) => {
                            writer_persistence.errors.fetch_add(1, Ordering::Relaxed);
                            eprintln!("catalog cache: could not write {} ({error})", writer_file.display());
                        }
                    }
                    snapshot_generation
                })
                .await;
                let written = match written {
                    Ok(written) => Some(written),
                    Err(error) => {
                        persistence.errors.fetch_add(1, Ordering::Relaxed);
                        eprintln!("catalog cache: writer task failed for {} ({error})", file.display());
                        None
                    }
                };

                // A failed cache write is retried by the next put, as before; do not spin on a full or
                // read-only filesystem. A successful write follows every generation it did not include.
                if written.is_some_and(|written| generation.load(Ordering::Acquire) != written) {
                    continue;
                }
                saving.store(false, Ordering::Release);
                let caught_up =
                    written.map(|written| generation.load(Ordering::Acquire) == written).unwrap_or(true);
                if caught_up || saving.swap(true, Ordering::AcqRel) {
                    break;
                }
                // A put landed between the caught-up check and clearing `saving`. It saw the active
                // writer, so this task owns the follow-up window.
            }
        });
    }

    #[cfg(test)]
    fn persisted_with_flush_after(ttl: Duration, dir: &Path, flush_after: Duration) -> Self {
        Self { flush_after, ..Self::persisted(ttl, dir) }
    }

    #[cfg(test)]
    fn pause_next_write(&self) -> (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *lock(&self.before_write) =
            Some(WritePause { entered: Arc::clone(&entered), release: Arc::clone(&release) });
        (entered, release)
    }

    #[cfg(test)]
    fn fail_next_writer_task(&self) {
        self.panic_next_write.store(true, Ordering::Release);
    }
}

/// Beside the file, then over it: a reader or a crash never sees half a file.
fn write_atomically(file: &Path, body: &[u8]) -> std::io::Result<()> {
    let partial = file.with_extension("json.partial");
    std::fs::write(&partial, body)?;
    std::fs::rename(&partial, file)
}

/// How many query vectors the embed memo keeps, and for how long.
pub const EMBED_MEMO_ENTRIES: usize = 1_000;
pub const EMBED_MEMO_TTL: Duration = Duration::from_secs(24 * 3600);
/// Library hints are bounded by `recommend::MAX_LIBRARY`; room for two maximum-size libraries avoids
/// one household immediately evicting another while keeping the on-disk vector file bounded.
pub const LIBRARY_EMBED_MEMO_ENTRIES: usize = 10_000;
const LIBRARY_EMBED_FILE: &str = "library-embeds.v1.json";

/// Query text → the vector den-embed gave it, so a search typed again (another page, the same word from the
/// next viewer, a client revalidating) does not queue a model call behind the embed permits. Least recently
/// used goes first when full, and nothing is kept past its TTL. In memory only: a restart costs one embed per
/// query, which is what every search cost before.
///
/// The key names the embedding space along with the text (`key`), so a vector is never handed to a corpus
/// embedded by another model or at another width.
pub struct EmbedMemo {
    ttl: Duration,
    max_entries: usize,
    map: Arc<Mutex<HashMap<String, Remembered>>>,
    file: Option<PathBuf>,
    dirty: Arc<AtomicBool>,
    saving: Arc<AtomicBool>,
}

#[derive(Serialize, Deserialize)]
struct Remembered {
    vector: Vec<i8>,
    stored: SystemTime,
    used: SystemTime,
}

impl EmbedMemo {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            map: Arc::default(),
            file: None,
            dirty: Arc::default(),
            saving: Arc::default(),
        }
    }

    /// Library vectors survive deploys without persisting a viewer's search text. Keys contain only the
    /// semantic space, title identity and a digest of the bounded hint text.
    pub fn persisted_library(dir: &Path) -> Self {
        let file = dir.join(LIBRARY_EMBED_FILE);
        let map = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("library embed memo: {} is unreadable ({e}) — starting empty", file.display());
                HashMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                eprintln!("library embed memo: could not read {} ({e}) — starting empty", file.display());
                HashMap::new()
            }
        };
        let memo = Self {
            ttl: Duration::MAX,
            max_entries: LIBRARY_EMBED_MEMO_ENTRIES,
            map: Arc::new(Mutex::new(map)),
            file: Some(file),
            dirty: Arc::default(),
            saving: Arc::default(),
        };
        while memo.len_unchecked() > memo.max_entries {
            memo.evict_one();
        }
        memo
    }

    /// The memo key for `text` in a complete semantic-query space identity. That identity includes a
    /// dataset-declared plot transform: the raw den-embed bytes may be equal, but the plot scan derived from
    /// them is not, and a cache entry must never silently straddle those contracts.
    pub fn key(space: &str, text: &str) -> String {
        format!("{space}\u{0}{text}")
    }

    /// A persistent library key: dataset space and title identity in the clear, bounded hint text only as
    /// a digest. A changed title, year or overview changes `text`, misses, and re-embeds that title.
    pub fn library_key(space: &str, kind: &str, id: u32, text: &str) -> String {
        let digest: String =
            Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
        format!("library-v1\0{space}\0{kind}:{id}\0{digest}")
    }

    /// A kept vector still inside its TTL; an expired one is dropped here.
    pub fn get(&self, key: &str) -> Option<Vec<i8>> {
        let mut map = lock(&self.map);
        let entry = map.get_mut(key)?;
        if entry.stored.elapsed().unwrap_or_default() >= self.ttl {
            map.remove(key);
            return None;
        }
        entry.used = SystemTime::now();
        Some(entry.vector.clone())
    }

    pub fn put(&self, key: String, vector: Vec<i8>) {
        let mut map = lock(&self.map);
        if map.len() >= self.max_entries && !map.contains_key(&key) {
            // A thousand entries: a scan per insert when full is cheaper than keeping an order list.
            if let Some(coldest) = map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone()) {
                map.remove(&coldest);
            }
        }
        let now = SystemTime::now();
        map.insert(key, Remembered { vector, stored: now, used: now });
        drop(map);
        self.save();
    }

    fn len_unchecked(&self) -> usize {
        lock(&self.map).len()
    }

    fn evict_one(&self) {
        let mut map = lock(&self.map);
        if let Some(coldest) = map.iter().min_by_key(|(_, entry)| entry.used).map(|(key, _)| key.clone()) {
            map.remove(&coldest);
        }
    }

    fn save(&self) {
        let Some(file) = self.file.clone() else { return };
        self.dirty.store(true, Ordering::SeqCst);
        if self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let (map, dirty, saving) = (Arc::clone(&self.map), Arc::clone(&self.dirty), Arc::clone(&self.saving));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.saving.store(false, Ordering::SeqCst);
            return;
        };
        runtime.spawn_blocking(move || loop {
            while dirty.swap(false, Ordering::SeqCst) {
                let body = serde_json::to_vec(&*lock(&map));
                if let Err(e) =
                    body.map_err(std::io::Error::other).and_then(|bytes| write_atomically(&file, &bytes))
                {
                    eprintln!("library embed memo: could not write {} ({e})", file.display());
                }
            }
            saving.store(false, Ordering::SeqCst);
            if !dirty.load(Ordering::SeqCst) || saving.swap(true, Ordering::SeqCst) {
                break;
            }
        });
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        lock(&self.map).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_until_idle(cache: &TtlCache) {
        for _ in 0..400 {
            if !cache.saving.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("catalog cache writer did not become idle");
    }

    /// The map had no eviction at all and `get` deliberately keeps expired entries (serve-stale
    /// reads them), so it only ever grew — one entry per key ever requested, for the process
    /// lifetime, against a 256 MB container.
    #[test]
    fn the_cache_stops_growing_at_its_cap() {
        let c = TtlCache::with_max_entries(Duration::from_secs(3600), 3);
        for i in 0..50 {
            c.put(&format!("k{i}"), format!("v{i}"));
        }
        assert_eq!(c.len(), 3, "the cache grew past its cap");
        // The newest survive; the oldest are the ones dropped.
        assert!(matches!(c.get("k49"), Lookup::Fresh(_)), "the newest entry was evicted");
        assert!(matches!(c.get("k0"), Lookup::Miss), "the oldest entry was kept");
    }

    /// Re-putting an existing key must not evict a different one — it replaces in place, so the map
    /// never grows and there is nothing to make room for. A cap that evicted here would throw away
    /// a live row on every single refresh once full.
    #[test]
    fn refreshing_an_existing_key_evicts_nothing() {
        let c = TtlCache::with_max_entries(Duration::from_secs(3600), 3);
        for i in 0..3 {
            c.put(&format!("k{i}"), format!("v{i}"));
        }
        for _ in 0..10 {
            c.put("k0", "refreshed".to_owned());
        }
        assert_eq!(c.len(), 3);
        for i in 0..3 {
            assert!(matches!(c.get(&format!("k{i}")), Lookup::Fresh(_)), "k{i} was evicted by a refresh");
        }
    }

    /// A deploy restarted the process with an empty map, so the first request for every row waited on
    /// JustWatch again. Kept rows come back with their age: fresh ones are served, old ones are stale.
    #[tokio::test]
    async fn kept_rows_survive_a_restart_with_their_age() {
        let dir = std::env::temp_dir().join(format!("den-atlas-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE);
        let c = TtlCache::persisted(Duration::from_secs(3600), &dir);
        c.put("k", "v".to_owned());
        for _ in 0..200 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let restarted = TtlCache::persisted(Duration::from_secs(3600), &dir);
        assert!(matches!(restarted.get("k"), Lookup::Fresh(v) if v == "v"), "a kept row was lost");
        assert!(matches!(TtlCache::persisted(Duration::ZERO, &dir).get("k"), Lookup::Stale(_)));

        std::fs::write(&file, b"not json").unwrap();
        assert!(matches!(TtlCache::persisted(Duration::from_secs(3600), &dir).get("k"), Lookup::Miss));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Spaced upstream responses used to finish after the active writer and each start another full-map
    /// serialization. A fixed window makes write count depend on burst duration, not row count.
    #[tokio::test]
    async fn a_spaced_put_burst_is_batched_into_a_few_writes() {
        let dir = std::env::temp_dir().join(format!(
            "den-atlas-cache-burst-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache =
            TtlCache::persisted_with_flush_after(Duration::from_secs(3600), &dir, Duration::from_millis(200));
        for i in 0..40 {
            cache.put(&format!("k{i}"), format!("v{i}"));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        wait_until_idle(&cache).await;

        let status = cache.persistence_status();
        assert!((1..=2).contains(&status.writes), "40 spaced puts caused {} full writes", status.writes);
        assert!(status.bytes > 0, "successful replacement bytes were not counted");
        assert_eq!(status.errors, 0);
        let restarted = TtlCache::persisted(Duration::from_secs(3600), &dir);
        assert_eq!(restarted.len(), 40);
        assert!(matches!(restarted.get("k39"), Lookup::Fresh(value) if value == "v39"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// The generation is captured under the same lock as the serialized map. A put after that snapshot
    /// cannot be mistaken for part of it: the active coordinator must perform a follow-up replacement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_put_during_a_write_is_in_a_guaranteed_follow_up() {
        let dir = std::env::temp_dir().join(format!(
            "den-atlas-cache-generation-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache =
            TtlCache::persisted_with_flush_after(Duration::from_secs(3600), &dir, Duration::from_millis(5));
        let (entered, release) = cache.pause_next_write();
        cache.put("before", "one".to_owned());
        tokio::task::spawn_blocking(move || entered.wait()).await.unwrap();
        cache.put("during", "two".to_owned());
        tokio::task::spawn_blocking(move || release.wait()).await.unwrap();
        wait_until_idle(&cache).await;

        assert_eq!(cache.persistence_status().writes, 2);
        let restarted = TtlCache::persisted(Duration::from_secs(3600), &dir);
        assert!(matches!(restarted.get("before"), Lookup::Fresh(value) if value == "one"));
        assert!(matches!(restarted.get("during"), Lookup::Fresh(value) if value == "two"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_failed_replacement_is_counted_and_the_next_put_retries() {
        let dir = std::env::temp_dir().join(format!(
            "den-atlas-cache-retry-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let cache =
            TtlCache::persisted_with_flush_after(Duration::from_secs(3600), &dir, Duration::from_millis(5));
        cache.put("before", "one".to_owned());
        wait_until_idle(&cache).await;
        assert_eq!(
            cache.persistence_status(),
            CachePersistenceStatus { writes: 0, bytes: 0, errors: 1, last_write_ms: 0 }
        );

        std::fs::create_dir_all(&dir).unwrap();
        cache.put("after", "two".to_owned());
        wait_until_idle(&cache).await;
        let status = cache.persistence_status();
        assert_eq!(status.writes, 1);
        assert!(status.bytes > 0);
        assert_eq!(status.errors, 1);
        let restarted = TtlCache::persisted(Duration::from_secs(3600), &dir);
        assert!(matches!(restarted.get("before"), Lookup::Fresh(value) if value == "one"));
        assert!(matches!(restarted.get("after"), Lookup::Fresh(value) if value == "two"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_failed_writer_task_is_counted_and_the_next_put_retries() {
        let dir = std::env::temp_dir().join(format!(
            "den-atlas-cache-task-retry-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache =
            TtlCache::persisted_with_flush_after(Duration::from_secs(3600), &dir, Duration::from_millis(5));
        cache.fail_next_writer_task();
        cache.put("before", "one".to_owned());
        wait_until_idle(&cache).await;
        assert_eq!(cache.persistence_status().writes, 0);
        assert_eq!(cache.persistence_status().errors, 1, "the JoinError was hidden");

        cache.put("after", "two".to_owned());
        wait_until_idle(&cache).await;
        let status = cache.persistence_status();
        assert_eq!(status.writes, 1);
        assert_eq!(status.errors, 1);
        let restarted = TtlCache::persisted(Duration::from_secs(3600), &dir);
        assert!(matches!(restarted.get("before"), Lookup::Fresh(value) if value == "one"));
        assert!(matches!(restarted.get("after"), Lookup::Fresh(value) if value == "two"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Manual release-mode measurement on a catalog-cache-sized map:
    /// `cargo test --release catalog_cache_refresh_wave -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "release-mode persistence benchmark"]
    async fn catalog_cache_refresh_wave() {
        let dir = std::env::temp_dir().join(format!(
            "den-atlas-cache-bench-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = TtlCache::persisted(Duration::from_secs(3600), &dir);
        let row = "x".repeat(22 * 1024);
        for i in 0..250 {
            cache.put(&format!("k{i}"), row.clone());
        }
        wait_until_idle(&cache).await;
        let before = cache.persistence_status();
        let started = Instant::now();
        for i in 0..100 {
            cache.put(&format!("k{i}"), row.clone());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        wait_until_idle(&cache).await;
        let after = cache.persistence_status();
        let writes = after.writes - before.writes;
        let bytes = after.bytes - before.bytes;
        eprintln!(
            "catalog cache refresh wave: writes={writes} bytes={bytes} elapsed_ms={} last_write_ms={}",
            started.elapsed().as_millis(),
            after.last_write_ms
        );
        assert_eq!(writes, 1, "a sub-window refresh wave should have one replacement");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_embed_memo_drops_the_least_recently_used_at_its_cap() {
        let memo = EmbedMemo::new(Duration::from_secs(3600), 2);
        memo.put("a".into(), vec![1]);
        memo.put("b".into(), vec![2]);
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(memo.get("a"), Some(vec![1]), "a read makes `a` the most recently used");
        memo.put("c".into(), vec![3]);
        assert_eq!(memo.len(), 2);
        assert_eq!(memo.get("b"), None, "the least recently used survived");
        assert_eq!(memo.get("a"), Some(vec![1]));
        assert_eq!(memo.get("c"), Some(vec![3]));
    }

    #[test]
    fn the_embed_memo_forgets_past_its_ttl() {
        let memo = EmbedMemo::new(Duration::ZERO, 10);
        memo.put("a".into(), vec![1]);
        assert_eq!(memo.get("a"), None);
        assert_eq!(memo.len(), 0, "an expired vector was kept");
    }

    #[tokio::test]
    async fn library_embeddings_survive_a_restart_without_storing_hint_text() {
        let dir = std::env::temp_dir().join(format!("den-atlas-library-embed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(LIBRARY_EMBED_FILE);
        let memo = EmbedMemo::persisted_library(&dir);
        let prose = "private plot prose";
        let key = EmbedMemo::library_key("space", "movie", 42, prose);
        memo.put(key.to_owned(), vec![1, -2, 3]);
        for _ in 0..200 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let bytes = std::fs::read(&file).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(prose));
        let restarted = EmbedMemo::persisted_library(&dir);
        assert_eq!(restarted.get(&key), Some(vec![1, -2, 3]));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One text in two full semantic spaces is two keys. A plot transform is part of that identity even
    /// though both spaces obtain the same raw bytes from den-embed.
    #[test]
    fn the_embed_memo_key_names_the_transform_as_well_as_the_raw_space() {
        let text = "korean heist";
        let keys = [
            EmbedMemo::key("canary-v1:raw", text),
            EmbedMemo::key("canary-v1:raw:plot-transform:aaa", text),
            EmbedMemo::key("canary-v1:raw:plot-transform:bbb", text),
        ];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
        assert_eq!(keys[0], EmbedMemo::key("canary-v1:raw", text));
    }

    #[test]
    fn fresh_then_miss() {
        let c = TtlCache::new(Duration::from_secs(3600));
        assert!(matches!(c.get("k"), Lookup::Miss));
        c.put("k", "v".to_owned());
        match c.get("k") {
            Lookup::Fresh(v) => assert_eq!(v, "v"),
            _ => panic!("expected fresh"),
        }
    }

    #[test]
    fn expired_is_stale_not_gone() {
        let c = TtlCache::new(Duration::ZERO); // everything is immediately past its TTL
        c.put("k", "v".to_owned());
        match c.get("k") {
            Lookup::Stale(v) => assert_eq!(v, "v"),
            _ => panic!("expected stale (kept for serve-stale-on-error)"),
        }
    }
}
