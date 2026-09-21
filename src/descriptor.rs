//! `dataset.json` descriptor — a byte-identical port of `buildDescriptor` (`src/descriptor.ts`). Field order
//! matches the TS object so `serde_json::to_string` == `JSON.stringify` (the app reads this; it must be
//! identical to today's bytes). Blob URLs are version-stamped (`?v=<datasetVersion>`) for immutable caching.

use crate::dataset::Dataset;
use serde::Serialize;

#[derive(Serialize)]
struct DescriptorBlob {
    url: String,
    sha256: String,
    bytes: u64,
}

/// DT-H — the second (premise) index block: its own model/dims/count + labels+vectors blobs.
#[derive(Serialize)]
struct PremiseDescriptor {
    #[serde(rename = "embeddingModel")]
    embedding_model: String,
    dims: u32,
    count: u64,
    labels: DescriptorBlob,
    vectors: DescriptorBlob,
}

#[derive(Serialize)]
struct Descriptor {
    #[serde(rename = "datasetVersion")]
    dataset_version: String,
    #[serde(rename = "taxonomyVersion")]
    taxonomy_version: String,
    /// The embedding model and its width. Optional because a release that no longer publishes the vector
    /// blobs need not describe them; when it does, this is what `POST /embed` produces a query vector in.
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
    /// The labels and vectors blobs, when the release still publishes them. Omitted otherwise — never
    /// filled with an empty name or a zero sha, which would parse and lie.
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<DescriptorBlob>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vectors: Option<DescriptorBlob>,
    /// Optional metadata sidecar blob (poster/title cache). Omitted when absent, so the descriptor stays
    /// byte-identical to before; the app reads it as `decodeIfPresent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<DescriptorBlob>,
    /// DT-H — the optional second (premise) index. Omitted when absent; the app reads it as `decodeIfPresent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    premise: Option<PremiseDescriptor>,
    /// DT-I — the optional compact facet blob. Omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    facets: Option<DescriptorBlob>,
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

pub fn build_descriptor(origin: &str, ds: &Dataset, embed_enabled: bool, queries_enabled: bool) -> String {
    // `datasetVersion` is a content-hash hex / safe token, so `encodeURIComponent` is the identity here.
    let v = &ds.meta.dataset_version;
    let blob = |b: &crate::dataset::Blob| DescriptorBlob {
        url: format!("{origin}/{}?v={v}", b.name),
        sha256: b.sha256.clone(),
        bytes: b.size,
    };
    let count = u64::try_from(ds.store_rows).ok();
    let d = Descriptor {
        dataset_version: ds.meta.dataset_version.clone(),
        taxonomy_version: ds.meta.taxonomy_version.clone(),
        embedding_model: Some(ds.meta.embedding_model.clone()),
        dims: Some(ds.meta.dims),
        count,
        quantization: ds.meta.quantization.clone(),
        labels: ds.labels.as_ref().map(&blob),
        vectors: ds.vectors.as_ref().map(&blob),
        metadata: ds.metadata.as_ref().map(&blob),
        premise: match (&ds.premise_labels, &ds.premise_vectors) {
            (Some(pl), Some(pv)) => Some(PremiseDescriptor {
                embedding_model: ds
                    .meta
                    .premise_embedding_model
                    .clone()
                    .unwrap_or_else(|| ds.meta.embedding_model.clone()),
                dims: ds.meta.premise_dims.unwrap_or(ds.meta.dims),
                count: ds.meta.premise_count.or(count).unwrap_or(0),
                labels: blob(pl),
                vectors: blob(pv),
            }),
            _ => None,
        },
        facets: ds.facets.as_ref().map(&blob),
        embed: embed_enabled.then_some(true),
        queries: queries_enabled.then_some(true),
        signature: ds.meta.signature.clone(),
    };
    serde_json::to_string(&d).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Blob, Meta};
    use std::path::PathBuf;

    fn blob(name: &str) -> Blob {
        Blob {
            name: name.into(),
            path: PathBuf::from(name),
            size: 10,
            // Descriptor tests never open the payload; only its advertised fields matter here.
            identity: crate::http::FileIdentity::from_metadata(&std::fs::metadata(".").unwrap()).unwrap(),
            sha256: format!("sha-{name}"),
            content_type: "application/octet-stream",
            gz: None,
        }
    }

    /// A dataset that still publishes the labels and vectors blobs beside its store.
    fn dataset(signature: Option<&str>) -> Dataset {
        let mut ds = store_only(signature);
        ds.labels = Some(blob("labels-t02.json"));
        ds.vectors = Some(blob("vectors-bge-m3.bin"));
        ds.meta.labels_file = Some("labels-t02.json".into());
        ds.meta.vectors_file = Some("vectors-bge-m3.bin".into());
        ds.meta.labels_sha256 = Some("l".into());
        ds.meta.labels_bytes = Some(10);
        ds.meta.vectors_sha256 = Some("v".into());
        ds.meta.vectors_bytes = Some(10);
        ds
    }

    /// The pruned release: a store and nothing else to serve.
    fn store_only(signature: Option<&str>) -> Dataset {
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
                labels_file: None,
                vectors_file: None,
                labels_gz_file: None,
                metadata_gz_file: None,
                labels_sha256: None,
                labels_bytes: None,
                vectors_sha256: None,
                vectors_bytes: None,
                last_modified_http: None,
                metadata_file: None,
                metadata_sha256: None,
                metadata_bytes: None,
                premise_embedding_model: None,
                premise_dims: None,
                premise_count: None,
                premise_labels_gz_file: None,
                premise_labels_file: None,
                premise_labels_sha256: None,
                premise_labels_bytes: None,
                premise_vectors_file: None,
                premise_vectors_sha256: None,
                premise_vectors_bytes: None,
                facets_file: None,
                facets_sha256: None,
                facets_bytes: None,
                store_file: "den-v1.store".into(),
            },
            labels: None,
            vectors: None,
            metadata: None,
            premise_labels: None,
            premise_vectors: None,
            facets: None,
            last_modified: None,
        }
    }

    /// FP-3 — the signature is passed through verbatim. den-atlas neither mints nor validates it: signing
    /// happens where the dataset is published, and verification happens in the app against a key the user
    /// pinned. An addon able to mint its own signature would prove nothing.
    #[test]
    fn signature_is_passed_through_verbatim() {
        let json = build_descriptor("https://atlas.test", &dataset(Some("ed25519:AAAA")), false, false);
        assert!(json.contains(r#""signature":"ed25519:AAAA""#), "got {json}");
    }

    /// The query routes are declared only when on; off, the descriptor is byte-identical to before.
    #[test]
    fn queries_are_declared_only_when_on() {
        assert!(
            build_descriptor("https://atlas.test", &dataset(None), false, true).contains(r#""queries":true"#)
        );
        assert!(!build_descriptor("https://atlas.test", &dataset(None), false, false).contains("queries"));
    }

    /// An unsigned dataset must serialize byte-identically to before the field existed, so existing
    /// providers keep decoding unchanged (the app reads it as `decodeIfPresent`).
    #[test]
    fn unsigned_dataset_omits_the_field_entirely() {
        let json = build_descriptor("https://atlas.test", &dataset(None), false, false);
        assert!(!json.contains("signature"), "got {json}");
    }

    /// A store-only release OMITS the blobs it no longer publishes rather than advertising empty ones.
    ///
    /// A placeholder would be worse than an absence: `"labels":{"url":"…/?v=v1","sha256":"","bytes":0}`
    /// decodes cleanly in the app and then fails at fetch time, or worse, verifies a zero-byte body
    /// against an empty hash. The app reads all five as optional, so leaving them out is the honest
    /// answer. `quantization`, `datasetVersion`, `taxonomyVersion` and the two capability flags stay.
    #[test]
    fn a_store_only_dataset_omits_the_blobs_it_does_not_publish() {
        let json = build_descriptor("https://atlas.test", &store_only(None), true, true);
        for gone in ["labels", "vectors", "metadata", "premise", "facets"] {
            assert!(!json.contains(&format!(r#""{gone}""#)), "{gone} was advertised: {json}");
        }
        assert!(json.contains(r#""datasetVersion":"v1""#), "got {json}");
        assert!(json.contains(r#""taxonomyVersion":"t02""#), "got {json}");
        assert!(json.contains(r#""quantization":"int8-symmetric-x127""#), "got {json}");
        assert!(json.contains(r#""queries":true"#) && json.contains(r#""embed":true"#), "got {json}");
    }

    /// `count` is the STORE's row count, not the manifest's `count` — which meant the labelled subset,
    /// was validated by nothing, and was read by the app as the size of the corpus.
    #[test]
    fn the_served_count_is_the_stores_own_row_count() {
        let mut ds = store_only(None);
        ds.store_rows = 47_618;
        let json = build_descriptor("https://atlas.test", &ds, false, false);
        assert!(json.contains(r#""count":47618"#), "got {json}");
    }
}
