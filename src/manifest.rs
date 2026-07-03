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
