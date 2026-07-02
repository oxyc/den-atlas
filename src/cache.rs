//! Tiny in-process TTL cache for the JustWatch catalog rows (docs/CATALOG-justwatch.md). Holds the
//! serialized `metas` JSON per catalog key. Serve-stale-on-error: an expired entry is kept until a
//! successful refresh replaces it, so a JustWatch blip serves the last-good rows instead of going empty.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct TtlCache {
    ttl: Duration,
    map: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    stored: Instant,
    value: String,
}

/// The result of a lookup: within TTL, expired-but-present, or absent.
pub enum Lookup {
    Fresh(String),
    Stale(String),
    Miss,
}

impl TtlCache {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, map: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, key: &str) -> Lookup {
        let map = self.map.lock().unwrap();
        match map.get(key) {
            Some(e) if e.stored.elapsed() < self.ttl => Lookup::Fresh(e.value.clone()),
            Some(e) => Lookup::Stale(e.value.clone()),
            None => Lookup::Miss,
        }
    }

    pub fn put(&self, key: &str, value: String) {
        let mut map = self.map.lock().unwrap();
        map.insert(key.to_owned(), Entry { stored: Instant::now(), value });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
