//! The addon manifest: the `dataset` resource (the Den app's feature store) plus the `catalog` resource
//! (JustWatch "most popular" rows, which depend on `JW_PROVIDERS`). serde serializes struct fields in
//! declaration order; the body feeds the fnv ETag, so its bytes must be stable for a given config.

use crate::catalog;
use crate::config::{Config, Region};
use serde::Serialize;

// Single source of truth: the Cargo package version (bumped per release, asserted == the v* tag in
// CI). So the manifest can never drift from Cargo.toml, and the tag can't drift from either.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DESCRIPTION: &str = "A map of the catalog: derived labels (genre / subgenre / mood) + semantic vectors the Den app downloads and refreshes, plus \"most popular\" streaming catalogs. Derived data only; catalog data from JustWatch.";

#[derive(Serialize)]
struct BehaviorHints {
    configurable: bool,
    #[serde(rename = "configurationRequired")]
    configuration_required: bool,
}

#[derive(Serialize)]
struct CatalogExtra {
    name: &'static str,
    #[serde(rename = "isRequired")]
    is_required: bool,
}

#[derive(Serialize)]
struct Catalog {
    #[serde(rename = "type")]
    type_: String,
    id: String,
    name: String,
    // Declared only when region is `auto`: tells the client it may forward a `country` extra (the Den
    // app sends the device region). Omitted for a fixed-country install (country is baked into the URL).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extra: Vec<CatalogExtra>,
    /// Den superset field: the TMDB watch-provider id this row is for (JustWatch's package id is the same
    /// number). Lets a client line the row up with TMDB's provider directory rather than parsing "Popular on
    /// Netflix". Stock Stremio clients ignore unknown catalog fields. Omitted for the cross-provider row.
    #[serde(rename = "denProviderId", skip_serializing_if = "Option::is_none")]
    den_provider_id: Option<i64>,
    /// EVERY id this service is known by. Ids are per-country (Prime is 119 in UY/FI, 9 in the US) and the
    /// manifest is country-agnostic for an `auto` install — it is fetched with no country extra — so a single
    /// id can't be right everywhere. A client matches on any of these.
    #[serde(rename = "denProviderIds", skip_serializing_if = "Vec::is_empty")]
    den_provider_ids: Vec<i64>,
}

#[derive(Serialize)]
struct Manifest {
    id: &'static str,
    version: &'static str,
    name: &'static str,
    description: &'static str,
    resources: Vec<&'static str>,
    types: Vec<&'static str>,
    #[serde(rename = "idPrefixes")]
    id_prefixes: Vec<&'static str>,
    catalogs: Vec<Catalog>,
    #[serde(rename = "behaviorHints")]
    behavior_hints: BehaviorHints,
}

pub fn manifest_json(config: &Config) -> String {
    // Region `auto` → each catalog accepts a `country` extra the app forwards; a fixed country needs none.
    let auto = config.region == Region::Auto;
    let catalogs = catalog::catalog_entries(&config.providers)
        .into_iter()
        .map(|e| Catalog {
            type_: e.type_.to_owned(),
            id: e.id,
            name: e.name,
            den_provider_id: e.package_ids.first().copied(),
            den_provider_ids: e.package_ids.to_vec(),
            extra: if auto {
                vec![CatalogExtra { name: "country", is_required: false }]
            } else {
                Vec::new()
            },
        })
        .collect();
    let m = Manifest {
        id: "com.den.atlas",
        version: VERSION,
        name: "Den Atlas",
        description: DESCRIPTION,
        // dataset = the Den app's feature store; catalog = public "most popular" rows (JustWatch).
        resources: vec!["dataset", "catalog"],
        types: vec!["movie", "series"],
        id_prefixes: vec!["tt"],
        catalogs,
        // Configurable: /configure builds a `<region>_<providers>` install URL. Not *required* — a bare
        // …/manifest.json still serves the operator-default config, so existing installs keep working.
        behavior_hints: BehaviorHints {
            configurable: true,
            configuration_required: false,
        },
    };
    serde_json::to_string(&m).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_publish_the_tmdb_provider_id() {
        let json = manifest_json(&Config::default_config());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cats = v["catalogs"].as_array().unwrap();
        let by = |id: &str| cats.iter().find(|c| c["id"] == id).unwrap_or_else(|| panic!("missing {id}"));
        // 8 is Netflix in BOTH JustWatch and TMDB — that shared id is the whole point of publishing it.
        assert_eq!(by("jw-nfx")["denProviderId"], 8);
        assert_eq!(by("jw-prv")["denProviderId"], 119, "Prime Video is 119, not the legacy 9");
        // The arrivals row points at the same service as its popular row.
        assert_eq!(by("jw-nfx-new")["denProviderId"], 8);
        // The cross-provider aggregate has no single provider, so the field is absent (not null).
        assert!(by(catalog::TRENDING_ID).get("denProviderId").is_none());
    }
}
