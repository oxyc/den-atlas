//! `dataset.json` descriptor — a byte-identical port of `buildDescriptor` (`src/descriptor.ts`). Field order
//! matches the TS object so `serde_json::to_string` == `JSON.stringify`.
//!
//! It describes the dataset; it no longer points at anything to download. The blob entries (`labels`,
//! `vectors`, `metadata`, `premise`, `facets`) named sidecars nothing fetches any more, and are gone with
//! the routes that served them (#113).

use crate::dataset::Dataset;
use serde::Serialize;

#[derive(Serialize)]
struct Descriptor {
    #[serde(rename = "datasetVersion")]
    dataset_version: String,
    #[serde(rename = "taxonomyVersion")]
    taxonomy_version: String,
    /// The embedding model and its width — the properties of the store's vector sections, and what
    /// `POST /embed` must produce a query vector in. Optional on the wire because the app reads them as
    /// `decodeIfPresent`; the meta requires them, so they are always sent.
    #[serde(rename = "embeddingModel", skip_serializing_if = "Option::is_none")]
    embedding_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dims: Option<u32>,
    /// Titles in the corpus — the STORE's own row count, not the manifest's `count`.
    ///
    /// `count` meant "titles carrying labels" (47,539 against the store's 47,618), nothing validated it,
    /// and the app read it as the size of the corpus. This is the number that has been checked against
    /// the bytes, and it is the one the publisher's shrink guard checks as `storeRecords`.
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<u64>,
    quantization: String,
    /// ADDON-03 — declares this addon can embed a free-text SEARCH query (`POST /embed` → den-embed is
    /// configured). Omitted when disabled, so the disabled descriptor stays byte-identical to before; the app
    /// reads it as `decodeIfPresent`, so absent ⇒ no semantic query search.
    #[serde(skip_serializing_if = "Option::is_none")]
    embed: Option<bool>,
    /// Declares the index query routes (`/index/…`, `INDEX_QUERIES` on). Omitted when off, so the descriptor
    /// stays byte-identical to before; the app reads it as `decodeIfPresent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    queries: Option<bool>,
    /// FP-3 — the producer's signature, verbatim from the meta. Omitted when unsigned, so an unsigned
    /// descriptor stays byte-identical to before and the app reads it as `decodeIfPresent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

pub fn build_descriptor(ds: &Dataset, embed_enabled: bool, queries_enabled: bool) -> String {
    let d = Descriptor {
        dataset_version: ds.meta.dataset_version.clone(),
        taxonomy_version: ds.meta.taxonomy_version.clone(),
        embedding_model: Some(ds.meta.embedding_model.clone()),
        dims: Some(ds.meta.dims),
        count: u64::try_from(ds.store_rows).ok(),
        quantization: ds.meta.quantization.clone(),
        embed: embed_enabled.then_some(true),
        queries: queries_enabled.then_some(true),
        signature: ds.meta.signature.clone(),
    };
    serde_json::to_string(&d).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::Meta;
    use std::path::PathBuf;

    /// The only release shape there is: a store and the facts about it that are not in it.
    fn dataset(signature: Option<&str>) -> Dataset {
        Dataset {
            store: PathBuf::from("den-v1.store"),
            store_rows: 100,
            meta: Meta {
                dataset_version: "v1".into(),
                taxonomy_version: "t02".into(),
                embedding_model: "bge-m3".into(),
                dims: 1024,
                quantization: "int8-symmetric-x127".into(),
                signature: signature.map(Into::into),
                last_modified_http: None,
                store_sha256: None,
                store_file: "den-v1.store".into(),
            },
            last_modified: None,
        }
    }

    /// FP-3 — the signature is passed through verbatim. den-atlas neither mints nor validates it: signing
    /// happens where the dataset is published, and verification happens in the app against a key the user
    /// pinned. An addon able to mint its own signature would prove nothing.
    #[test]
    fn signature_is_passed_through_verbatim() {
        let json = build_descriptor(&dataset(Some("ed25519:AAAA")), false, false);
        assert!(json.contains(r#""signature":"ed25519:AAAA""#), "got {json}");
    }

    /// The query routes are declared only when on; off, the descriptor is byte-identical to before.
    #[test]
    fn queries_are_declared_only_when_on() {
        assert!(build_descriptor(&dataset(None), false, true).contains(r#""queries":true"#));
        assert!(!build_descriptor(&dataset(None), false, false).contains("queries"));
    }

    /// An unsigned dataset must serialize byte-identically to before the field existed, so existing
    /// providers keep decoding unchanged (the app reads it as `decodeIfPresent`).
    #[test]
    fn unsigned_dataset_omits_the_field_entirely() {
        let json = build_descriptor(&dataset(None), false, false);
        assert!(!json.contains("signature"), "got {json}");
    }

    /// The descriptor DESCRIBES the dataset; it points at nothing to download.
    ///
    /// The five blob entries named sidecars the app no longer fetches, and the routes that served them
    /// are gone — so an entry for one would be a URL that 404s, which decodes cleanly in a client and
    /// fails only at fetch time. `quantization`, `datasetVersion`, `taxonomyVersion`, the corpus size,
    /// the embedding space and the two capability flags are the whole body.
    #[test]
    fn the_descriptor_advertises_no_blobs_at_all() {
        let json = build_descriptor(&dataset(None), true, true);
        for gone in ["labels", "vectors", "metadata", "premise", "facets", "url", "sha256", "bytes"] {
            assert!(!json.contains(&format!(r#""{gone}""#)), "{gone} was advertised: {json}");
        }
        assert!(json.contains(r#""datasetVersion":"v1""#), "got {json}");
        assert!(json.contains(r#""taxonomyVersion":"t02""#), "got {json}");
        assert!(json.contains(r#""quantization":"int8-symmetric-x127""#), "got {json}");
        assert!(json.contains(r#""embeddingModel":"bge-m3""#) && json.contains(r#""dims":1024"#), "{json}");
        assert!(json.contains(r#""queries":true"#) && json.contains(r#""embed":true"#), "got {json}");
    }

    /// `count` is the STORE's row count, not the manifest's `count` — which meant the labelled subset,
    /// was validated by nothing, and was read by the app as the size of the corpus.
    #[test]
    fn the_served_count_is_the_stores_own_row_count() {
        let mut ds = dataset(None);
        ds.store_rows = 47_618;
        let json = build_descriptor(&ds, false, false);
        assert!(json.contains(r#""count":47618"#), "got {json}");
    }
}
