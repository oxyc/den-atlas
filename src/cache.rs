//! Tiny in-process TTL cache for the JustWatch catalog rows (docs/CATALOG-justwatch.md). Holds the
//! serialized `metas` JSON per catalog key. Serve-stale-on-error: an expired entry is kept until a
//! successful refresh replaces it, so a JustWatch blip serves the last-good rows instead of going empty.
//!
//! With `CACHE_DIR` the rows are also kept on disk, so a restart — every deploy, the weekly rebuild, a
//! dataset change — serves the rows it had instead of making the next visitor wait on JustWatch for each.
//! The file is written only when a row is stored, never on a timer.

use crate::util::lock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub struct TtlCache {
    ttl: Duration,
    max_entries: usize,
    map: Arc<Mutex<HashMap<String, Entry>>>,
    /// Where the rows are kept across restarts; `None` keeps them in memory only.
    file: Option<PathBuf>,
    /// A row changed since the file was last written.
    dirty: Arc<AtomicBool>,
    /// A writer is running, so a burst of refreshes writes the file a few times rather than once per row.
    saving: Arc<AtomicBool>,
}

/// The file under `CACHE_DIR`. A new format takes a new name, so an old file is ignored, not misread.
const FILE: &str = "catalog-rows.v1.json";

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
            dirty: Arc::default(),
            saving: Arc::default(),
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

    pub fn get(&self, key: &str) -> Lookup {
        let map = lock(&self.map);
        match map.get(key) {
            // A clock stepped back reads as just stored: fresh, rather than a row that never expires.
            Some(e) if e.stored.elapsed().unwrap_or_default() < self.ttl => Lookup::Fresh(e.value.clone()),
            Some(e) => Lookup::Stale(e.value.clone()),
            None => Lookup::Miss,
        }
    }

    pub fn put(&self, key: &str, value: String) {
        {
            let mut map = lock(&self.map);
            if map.len() >= self.max_entries && !map.contains_key(key) {
                evict_one(&mut map);
            }
            map.insert(key.to_owned(), Entry { stored: SystemTime::now(), value });
        }
        self.save();
    }

    /// Write the rows to the file off the request path. One writer at a time; rows stored while it
    /// writes are picked up by its next pass.
    fn save(&self) {
        let Some(file) = self.file.clone() else { return };
        self.dirty.store(true, Ordering::SeqCst);
        if self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.saving.store(false, Ordering::SeqCst);
            return;
        };
        let (map, dirty, saving) = (Arc::clone(&self.map), Arc::clone(&self.dirty), Arc::clone(&self.saving));
        runtime.spawn_blocking(move || loop {
            while dirty.swap(false, Ordering::SeqCst) {
                let body = serde_json::to_vec(&*lock(&map));
                if let Err(e) = body.map_err(std::io::Error::other).and_then(|b| write_atomically(&file, &b))
                {
                    eprintln!("catalog cache: could not write {} ({e})", file.display());
                }
            }
            saving.store(false, Ordering::SeqCst);
            // A row stored between the last pass and clearing `saving` saw a writer and left it to this one.
            if !dirty.load(Ordering::SeqCst) || saving.swap(true, Ordering::SeqCst) {
                break;
            }
        });
    }
}

/// Beside the file, then over it: a reader or a crash never sees half a file.
fn write_atomically(file: &Path, body: &[u8]) -> std::io::Result<()> {
    let partial = file.with_extension("json.partial");
    std::fs::write(&partial, body)?;
    std::fs::rename(&partial, file)
}

#[cfg(test)]
mod tests {
    use super::*;

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
