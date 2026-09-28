//! Other versions of a title's story (oxyc/den-atlas#112): remakes and other adaptations of the same source
//! work, from the optional store sections `den_store::Store::other_versions` reads. den-dataset groups them
//! from Wikidata P144 (based on) and P4969 (derivative work).
//!
//! They are neither More Like This, which this does not touch, nor the franchise: a separate production of
//! the same books is its own franchise (#92) and appears here instead. The seed's own curated primary
//! franchise members are left out, because `/index/franchise` shows them; the store keeps them, so a change
//! to the franchise grouping needs no change to the versions. Nothing reserves a version from any other row.

use crate::queries::Indexes;
use den_index::MediaType;

type Key = (MediaType, u32);

/// The seed's other versions as served, with each one's relation (`source` or `remake`): its own curated
/// primary franchise left out, then in release order — a title with no release date last — the most popular
/// first on one date, then by key. Empty for a title with none, not in the store, or a store without them.
pub fn of_title(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    seed: Key,
) -> Vec<(Key, &'static str)> {
    let store = indexes.store.view();
    let media = u8::from(seed.0 == MediaType::Tv);
    let (Ok(Some(row)), Ok(versions), Ok(keys), Ok(released)) = (
        store.row_of(media, seed.1),
        store.other_versions(),
        store.per_row::<u64>("keys"),
        store.per_row::<i32>("released"),
    ) else {
        return Vec::new();
    };
    let mut found: Vec<(i32, f64, Key, &'static str)> = versions
        .get(row)
        .filter_map(|version| {
            let packed = *keys.get(version.row.0)?;
            let key = (if packed >> 32 == 1 { MediaType::Tv } else { MediaType::Movie }, packed as u32);
            if indexes.franchises.shares_primary(seed, key) {
                return None;
            }
            let kind = match version.kind {
                den_store::VersionKind::Remake => "remake",
                den_store::VersionKind::SharedSource => "source",
            };
            let day = released.get(version.row.0).copied().filter(|&d| d != i32::MIN).unwrap_or(i32::MAX);
            Some((day, crate::plotrows::popularity(indexes, export, key), key, kind))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
    found.into_iter().map(|(_, _, key, kind)| (key, kind)).collect()
}

/// `/index/title`'s `otherVersions`: `[{type, id, kind}]` in the served order, or `None` for a title with none,
/// so the field is left out exactly when a client shows no row.
pub fn summary_json(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    seed: Key,
) -> Option<serde_json::Value> {
    let found = of_title(indexes, export, seed);
    (!found.is_empty()).then(|| {
        serde_json::json!(found
            .iter()
            .map(
                |&((media, id), kind)| serde_json::json!({ "type": type_name(media), "id": id, "kind": kind })
            )
            .collect::<Vec<_>>())
    })
}

/// `/index/versions/<type>/<id>.json`: the seed's other versions as cards, each with its `kind`, in the order
/// `of_title` serves them. A version with no card is `{type, id, kind}`, as a franchise member is.
pub fn route_json(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    seed: Key,
) -> serde_json::Value {
    let versions: Vec<serde_json::Value> = of_title(indexes, export, seed)
        .into_iter()
        .map(|(key, kind)| {
            let mut title = indexes
                .cards
                .as_ref()
                .and_then(|cards| cards.get(&key))
                .map(|card| crate::plotrows::title_json(indexes, key, card))
                .unwrap_or_else(|| serde_json::json!({ "type": type_name(key.0), "id": key.1 }));
            title["kind"] = serde_json::json!(kind);
            title
        })
        .collect();
    serde_json::json!({
        "seed": { "type": type_name(seed.0), "id": seed.1 },
        "total": versions.len(),
        "versions": versions,
    })
}

fn type_name(media: MediaType) -> &'static str {
    if media == MediaType::Tv {
        "series"
    } else {
        "movie"
    }
}
