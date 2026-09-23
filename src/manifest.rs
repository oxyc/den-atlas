//! The addon manifest: the `dataset` resource (the Den app's feature store) plus the `catalog` resource
//! (JustWatch "most popular" rows, which depend on `JW_PROVIDERS`). serde serializes struct fields in
//! declaration order; the body feeds the fnv ETag, so its bytes must be stable for a given config.

use crate::catalog;
use crate::config::{Config, Region};
use crate::titles;
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

/// One statement a data source's terms ask for wherever its data reaches people (den-spec attribution-v1): the whole
/// statement, the part of it that links, and where.
#[derive(Serialize)]
struct Attribution {
    text: &'static str,
    link: &'static str,
    url: &'static str,
}

const WIKIPEDIA: Attribution = Attribution {
    text: "Discovery data (subgenres, moods, and “more like this”) is derived from Wikipedia article text, used under CC BY-SA 4.0 and modified.",
    link: "CC BY-SA 4.0",
    url: "https://creativecommons.org/licenses/by-sa/4.0",
};
const MOVIE_OF_THE_NIGHT: Attribution = Attribution {
    text:
        "Streaming availability information is provided by Streaming Availability API by Movie of the Night.",
    link: "Streaming Availability API by Movie of the Night",
    url: "https://www.movieofthenight.com/about/api",
};
/// IMDb's own wording for its non-commercial datasets (help.imdb.com, "Can I use IMDb data in my software?"):
/// the ratings that order rows and the character names titles are filtered by.
const IMDB: Attribution = Attribution {
    text: "Information courtesy of IMDb (https://www.imdb.com). Used with permission.",
    link: "IMDb",
    url: "https://www.imdb.com",
};
const JUSTWATCH: Attribution = Attribution {
    text: "Streaming availability by JustWatch.",
    link: "JustWatch",
    url: "https://www.justwatch.com",
};

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
    /// The sources this install's data comes from, credited by the client that shows it (den-spec attribution-v1).
    #[serde(rename = "denAttribution")]
    den_attribution: Vec<Attribution>,
}

/// `title_search` adds the fuzzy title-search catalogs (one per type, `search` required, so a client that
/// browses catalogs as rows skips them). `soon` adds each service's leaving and coming rows (Movie of the Night on).
/// `imdb` says answers draw on IMDb's datasets, which credits them.
pub fn manifest_json(config: &Config, title_search: bool, soon: bool, imdb: bool) -> String {
    // Region `auto` → each catalog accepts a `country` extra the app forwards; a fixed country needs none.
    let auto = config.region == Region::Auto;
    let mut catalogs: Vec<Catalog> = catalog::catalog_entries(&config.providers, soon)
        .into_iter()
        .map(|e| Catalog {
            type_: e.type_.to_owned(),
            id: e.id,
            name: e.name,
            den_provider_id: e.package_ids.first().copied(),
            den_provider_ids: e.package_ids.to_vec(),
            extra: {
                let mut extra = if auto {
                    vec![CatalogExtra { name: "country", is_required: false }]
                } else {
                    Vec::new()
                };
                // Stremio pages a catalog with `skip`. Declaring none left a client paging on scroll asking
                // for the same first page forever, with nothing to tell it the row had ended.
                extra.push(CatalogExtra { name: "skip", is_required: false });
                extra
            },
        })
        .collect();
    if title_search {
        for type_ in ["movie", "series"] {
            catalogs.push(Catalog {
                type_: type_.to_owned(),
                id: titles::CATALOG_ID.to_owned(),
                name: titles::CATALOG_NAME.to_owned(),
                extra: vec![CatalogExtra { name: "search", is_required: true }],
                den_provider_id: None,
                den_provider_ids: Vec::new(),
            });
        }
    }
    // The dataset is always derived from Wikipedia; the streaming rows credit only the sources this operator has on —
    // Movie of the Night's lists with its key, JustWatch's catalogs with any provider configured.
    let mut den_attribution = vec![WIKIPEDIA];
    if imdb {
        den_attribution.push(IMDB);
    }
    if soon {
        den_attribution.push(MOVIE_OF_THE_NIGHT);
    }
    if !config.providers.is_empty() {
        den_attribution.push(JUSTWATCH);
    }
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
        behavior_hints: BehaviorHints { configurable: true, configuration_required: false },
        den_attribution,
    };
    serde_json::to_string(&m).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_publish_the_tmdb_provider_id() {
        let json = manifest_json(&Config::default_config(), false, false, false);
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

    /// Wikipedia always; Movie of the Night only with its lists on, and JustWatch only with a provider configured —
    /// an operator is never credited for a source they haven't turned on.
    #[test]
    fn credits_the_sources_this_install_uses() {
        let texts = |json: String| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v["denAttribution"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["text"].as_str().unwrap().to_owned())
                .collect()
        };
        let with_motn = texts(manifest_json(&Config::default_config(), false, true, false));
        assert_eq!(with_motn.len(), 3);
        assert!(with_motn[0].contains("Wikipedia"));
        assert!(with_motn[1].contains("Movie of the Night"));
        assert!(with_motn[2].contains("JustWatch"));
        let without = texts(manifest_json(&Config::default_config(), false, false, false));
        assert!(!without.iter().any(|t| t.contains("Movie of the Night")), "{without:?}");
        let none = Config { providers: Vec::new(), ..Config::default_config() };
        assert_eq!(texts(manifest_json(&none, false, false, false)).len(), 1, "Wikipedia alone");
        // IMDb's datasets, when ratings or characters are joined, in IMDb's own words.
        let imdb = texts(manifest_json(&none, false, false, true));
        assert_eq!(imdb[1], "Information courtesy of IMDb (https://www.imdb.com). Used with permission.");
        // Each link is a part of its statement, so a client can find it there.
        let v: serde_json::Value =
            serde_json::from_str(&manifest_json(&Config::default_config(), false, true, false)).unwrap();
        for a in v["denAttribution"].as_array().unwrap() {
            assert!(a["text"].as_str().unwrap().contains(a["link"].as_str().unwrap()), "{a}");
        }
    }

    /// Title search is declared only when on, as a required `search` extra per type — required, so a
    /// client that shows catalogs as rows (the tvOS app's Browse) never tries to render it as one.
    #[test]
    fn title_search_catalogs_are_declared_only_when_on() {
        let off = manifest_json(&Config::default_config(), false, false, false);
        assert!(!off.contains(titles::CATALOG_ID));
        let on: serde_json::Value =
            serde_json::from_str(&manifest_json(&Config::default_config(), true, false, false)).unwrap();
        let search: Vec<&serde_json::Value> =
            on["catalogs"].as_array().unwrap().iter().filter(|c| c["id"] == titles::CATALOG_ID).collect();
        assert_eq!(search.len(), 2);
        assert_eq!(search[0]["type"], "movie");
        assert_eq!(search[1]["type"], "series");
        assert_eq!(search[0]["extra"][0]["name"], "search");
        assert_eq!(search[0]["extra"][0]["isRequired"], true);
    }
}
