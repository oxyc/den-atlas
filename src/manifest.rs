//! The `dataset`-resource manifest — a byte-identical port of `src/manifest.ts`. Field order matters: the
//! serialized JSON must match the TS `JSON.stringify` output exactly (same body → same fnv ETag; the app
//! reads it). serde serializes struct fields in declaration order.

use crate::catalog;
use serde::Serialize;

const VERSION: &str = "0.1.0";
const DESCRIPTION: &str = "A map of the catalog: derived labels (genre / subgenre / mood) + semantic vectors the Den app downloads and refreshes, plus \"most popular\" streaming catalogs. Derived data only; catalog data from JustWatch.";

#[derive(Serialize)]
struct BehaviorHints {
    configurable: bool,
    #[serde(rename = "configurationRequired")]
    configuration_required: bool,
}

#[derive(Serialize)]
struct Catalog {
    #[serde(rename = "type")]
    type_: String,
    id: String,
    name: String,
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

pub fn manifest_json() -> String {
    let catalogs = catalog::catalog_entries()
        .into_iter()
        .map(|e| Catalog { type_: e.type_.to_owned(), id: e.id, name: e.name })
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
        behavior_hints: BehaviorHints {
            configurable: false,
            configuration_required: false,
        },
    };
    serde_json::to_string(&m).unwrap()
}
