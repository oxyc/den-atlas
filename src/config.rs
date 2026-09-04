//! Per-install configuration carried in the URL path segment (the Stremio config-URL pattern), so a
//! single deployment serves every user's region + provider choice with **no server-side state**. The
//! segment is `<region>_<codes>`:
//!   - `region` = `auto` (the Den app forwards the device country as a `country` catalog extra) or an
//!     ISO-3166 alpha-2 code (a fixed country for this install).
//!   - `codes`  = JustWatch provider short-codes joined by `-` (e.g. `nfx-mxx-dnp`). Empty = no catalog
//!     rows (the "most popular" feature is off for this install).
//! An absent or unparseable segment falls back to the operator default (env `JW_COUNTRY`/`JW_PROVIDERS`).

use crate::catalog::{provider_by_code, selected_providers, Provider};

#[derive(Debug, Clone, PartialEq)]
pub enum Region {
    /// The Den app forwards the device region as a `country` catalog extra (fallback: operator default).
    Auto,
    /// A fixed ISO-3166 alpha-2 country, uppercased.
    Fixed(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub region: Region,
    pub providers: Vec<&'static Provider>,
}

impl Config {
    /// The operator-default config (no URL segment): region `auto`, the env-selected provider set. Keeps
    /// a plain `…/manifest.json` install working exactly as before, with the app forwarding the country.
    pub fn default_config() -> Config {
        Config { region: Region::Auto, providers: selected_providers().to_vec() }
    }

    /// Parse a `<region>_<codes>` path segment. `None` when the segment isn't a config (the caller then
    /// treats it as a normal route). Unknown provider codes are dropped; an empty provider set is valid
    /// and means "no catalog rows" for that install.
    pub fn parse(segment: &str) -> Option<Config> {
        let (region_str, codes_str) = segment.split_once('_')?;
        let region = parse_region(region_str)?;
        // DEDUPED. The selection is a set, and this comes straight off the URL: repeats were kept,
        // so `US_nfx-nfx-nfx…` was a distinct cache key per repetition count AND spawned one
        // upstream POST per element. A single crafted GET opened ~4,000 simultaneous connections to
        // JustWatch, earned 6,687 429s and then a 403 on the host IP — from an unauthenticated
        // request. Deduping bounds the fan-out and the keyspace by the provider table itself.
        let mut providers: Vec<&'static Provider> = Vec::new();
        for code in codes_str.split('-').filter(|c| !c.is_empty()) {
            if let Some(p) = provider_by_code(code) {
                if !providers.iter().any(|q| q.code == p.code) {
                    providers.push(p);
                }
            }
        }
        Some(Config { region, providers })
    }

    /// The effective country for a request: a fixed config country wins; else the app-forwarded
    /// `country` extra when region is `auto`; else the operator default.
    pub fn country(&self, forwarded: Option<&str>, default: &str) -> String {
        match &self.region {
            Region::Fixed(cc) => cc.clone(),
            Region::Auto => forwarded.and_then(normalize_country).unwrap_or_else(|| default.to_owned()),
        }
    }
}

fn parse_region(s: &str) -> Option<Region> {
    if s.eq_ignore_ascii_case("auto") {
        Some(Region::Auto)
    } else {
        normalize_country(s).map(Region::Fixed)
    }
}

/// A valid ISO-3166 alpha-2 country → uppercased, else `None`. We don't ship a full country table:
/// JustWatch validates the code upstream, and an unknown one simply yields empty rows.
fn normalize_country(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic())).then(|| s.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auto_and_providers() {
        let c = Config::parse("auto_nfx-mxx").unwrap();
        assert_eq!(c.region, Region::Auto);
        assert_eq!(c.providers.iter().map(|p| p.code).collect::<Vec<_>>(), vec!["nfx", "mxx"]);
    }

    #[test]
    fn parses_fixed_country_uppercased() {
        let c = Config::parse("se_nfx").unwrap();
        assert_eq!(c.region, Region::Fixed("SE".to_owned()));
    }

    #[test]
    fn empty_provider_set_is_valid_feature_off() {
        let c = Config::parse("US_").unwrap();
        assert!(c.providers.is_empty());
    }

    #[test]
    fn drops_unknown_provider_codes() {
        let c = Config::parse("auto_nfx-bogus-mxx").unwrap();
        assert_eq!(c.providers.iter().map(|p| p.code).collect::<Vec<_>>(), vec!["nfx", "mxx"]);
    }

    #[test]
    fn non_config_segments_are_none() {
        assert!(Config::parse("manifest.json").is_none());
        assert!(Config::parse("catalog").is_none());
        assert!(Config::parse("configure").is_none());
    }

    #[test]
    fn country_resolution() {
        let fixed = Config { region: Region::Fixed("SE".into()), providers: vec![] };
        assert_eq!(fixed.country(Some("de"), "US"), "SE"); // fixed wins over forwarded
        let auto = Config { region: Region::Auto, providers: vec![] };
        assert_eq!(auto.country(Some("de"), "US"), "DE"); // forwarded, uppercased
        assert_eq!(auto.country(None, "US"), "US"); // fallback to operator default
        assert_eq!(auto.country(Some("bogus"), "US"), "US"); // invalid extra → default
    }

    /// The selection comes off the URL and is a SET. Keeping repeats meant `US_nfx-nfx-nfx…` was a
    /// distinct cache key per repetition count AND spawned one upstream POST per element: a single
    /// crafted GET opened ~4,000 simultaneous connections to JustWatch, took 6,687 429s, and got the
    /// host IP 403'd — unauthenticated, and repeatable because each URL was a fresh key.
    #[test]
    fn a_repeated_provider_code_is_counted_once() {
        let one = Config::parse("US_nfx").unwrap();
        for repeated in ["US_nfx-nfx", "US_nfx-nfx-nfx-nfx-nfx", "US_nfx-nfx-mxx-nfx"] {
            let c = Config::parse(repeated).unwrap();
            let codes: Vec<&str> = c.providers.iter().map(|p| p.code).collect();
            let mut uniq = codes.clone();
            uniq.sort_unstable();
            uniq.dedup();
            assert_eq!(codes.len(), uniq.len(), "{repeated} kept a duplicate: {codes:?}");
        }
        // ...and the ordinary case is untouched.
        assert_eq!(one.providers.len(), 1);
        assert_eq!(Config::parse("US_nfx-mxx").unwrap().providers.len(), 2);
        // The upper bound is now the provider table itself, whatever the URL says.
        let huge = format!("US_{}", vec!["nfx"; 4000].join("-"));
        assert!(Config::parse(&huge).unwrap().providers.len() <= crate::catalog::provider_count());
    }
}
