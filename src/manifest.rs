//! The `dataset`-resource manifest — a byte-identical port of `src/manifest.ts`. Field order matters: the
//! serialized JSON must match the TS `JSON.stringify` output exactly (same body → same fnv ETag; the app
//! reads it). serde serializes struct fields in declaration order.

use serde::Serialize;

const VERSION: &str = "0.1.0";
const DESCRIPTION: &str = "A map of the catalog: derived labels (genre / subgenre / mood) + semantic vectors the Den app downloads and refreshes for similar-titles, categories, and billboard. Derived data only.";

#[derive(Serialize)]
struct BehaviorHints {
    configurable: bool,
    #[serde(rename = "configurationRequired")]
    configuration_required: bool,
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
    catalogs: Vec<serde_json::Value>,
    #[serde(rename = "behaviorHints")]
    behavior_hints: BehaviorHints,
}

pub fn manifest_json() -> String {
    let m = Manifest {
        id: "com.den.atlas",
        version: VERSION,
        name: "Den Atlas",
        description: DESCRIPTION,
        resources: vec!["dataset"],
        types: vec!["movie", "series"],
        id_prefixes: vec!["tt"],
        catalogs: vec![],
        behavior_hints: BehaviorHints {
            configurable: false,
            configuration_required: false,
        },
    };
    serde_json::to_string(&m).unwrap()
}
