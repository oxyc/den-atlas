//! Tiny in-process TTL cache for the JustWatch catalog rows (docs/CATALOG-justwatch.md). Holds the
//! serialized `metas` JSON per catalog key. Serve-stale-on-error: an expired entry is kept until a
//! successful refresh replaces it, so a JustWatch blip serves the last-good rows instead of going empty.

use crate::util::lock;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct TtlCache {
    ttl: Duration,
    max_entries: usize,
    map: Mutex<HashMap<String, Entry>>,
}

/// How many rendered rows to keep. Nothing here ever expired an entry — serve-stale reads expired
/// ones, so the map only grew, for the process lifetime. The keyspace is bounded (countries are
/// validated to two letters, provider subsets to 127) but the ceiling is ~190k keys of ~22 KB,
/// roughly 4 GB, reachable unauthenticated by enumerating `country` × provider selection, against a
/// 256 MB container. A real install touches a handful of countries, so this is far above normal use
/// and far below the ceiling.
const DEFAULT_MAX_ENTRIES: usize = 2_000;

struct Entry {
    stored: Instant,
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
        Self { ttl, max_entries: DEFAULT_MAX_ENTRIES, map: Mutex::new(HashMap::new()) }
    }

    #[cfg(test)]
    pub fn with_max_entries(ttl: Duration, max_entries: usize) -> Self {
        Self { ttl, max_entries, map: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, key: &str) -> Lookup {
        let map = lock(&self.map);
        match map.get(key) {
            Some(e) if e.stored.elapsed() < self.ttl => Lookup::Fresh(e.value.clone()),
            Some(e) => Lookup::Stale(e.value.clone()),
            None => Lookup::Miss,
        }
    }

    pub fn put(&self, key: &str, value: String) {
        let mut map = lock(&self.map);
        if map.len() >= self.max_entries && !map.contains_key(key) {
            evict_one(&mut map);
        }
        map.insert(key.to_owned(), Entry { stored: Instant::now(), value });
    }
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
