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
    #[serde(rename = "embeddingModel")]
    embedding_model: String,
    dims: u32,
    count: u64,
    quantization: String,
    labels: DescriptorBlob,
    vectors: DescriptorBlob,
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
    /// FP-3 — the producer's signature, verbatim from the meta. Omitted when unsigned, so an unsigned
    /// descriptor stays byte-identical to before and the app reads it as `decodeIfPresent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

pub fn build_descriptor(origin: &str, ds: &Dataset, embed_enabled: bool) -> String {
    // `datasetVersion` is a content-hash hex / safe token, so `encodeURIComponent` is the identity here.
    let v = &ds.meta.dataset_version;
    let d = Descriptor {
        dataset_version: ds.meta.dataset_version.clone(),
        taxonomy_version: ds.meta.taxonomy_version.clone(),
        embedding_model: ds.meta.embedding_model.clone(),
        dims: ds.meta.dims,
        count: ds.meta.count,
        quantization: ds.meta.quantization.clone(),
        labels: DescriptorBlob {
            url: format!("{origin}/{}?v={v}", ds.labels.name),
            sha256: ds.labels.sha256.clone(),
            bytes: ds.labels.size,
        },
        vectors: DescriptorBlob {
            url: format!("{origin}/{}?v={v}", ds.vectors.name),
            sha256: ds.vectors.sha256.clone(),
            bytes: ds.vectors.size,
        },
        metadata: ds.metadata.as_ref().map(|m| DescriptorBlob {
            url: format!("{origin}/{}?v={v}", m.name),
            sha256: m.sha256.clone(),
            bytes: m.size,
        }),
        premise: match (&ds.premise_labels, &ds.premise_vectors) {
            (Some(pl), Some(pv)) => Some(PremiseDescriptor {
                embedding_model: ds
                    .meta
                    .premise_embedding_model
                    .clone()
                    .unwrap_or_else(|| ds.meta.embedding_model.clone()),
                dims: ds.meta.premise_dims.unwrap_or(ds.meta.dims),
                count: ds.meta.premise_count.unwrap_or(ds.meta.count),
                labels: DescriptorBlob {
                    url: format!("{origin}/{}?v={v}", pl.name),
                    sha256: pl.sha256.clone(),
                    bytes: pl.size,
                },
                vectors: DescriptorBlob {
                    url: format!("{origin}/{}?v={v}", pv.name),
                    sha256: pv.sha256.clone(),
                    bytes: pv.size,
                },
            }),
            _ => None,
        },
        facets: ds.facets.as_ref().map(|f| DescriptorBlob {
            url: format!("{origin}/{}?v={v}", f.name),
            sha256: f.sha256.clone(),
            bytes: f.size,
        }),
        embed: embed_enabled.then_some(true),
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
            sha256: format!("sha-{name}"),
            content_type: "application/octet-stream",
            gz: None,
        }
    }

    fn dataset(signature: Option<&str>) -> Dataset {
        Dataset {
            meta: Meta {
                dataset_version: "v1".into(),
                taxonomy_version: "t02".into(),
                embedding_model: "bge-m3".into(),
                dims: 1024,
                count: 100,
                quantization: "int8-symmetric-x127".into(),
                signature: signature.map(Into::into),
                labels_file: "labels-t02.json".into(),
                vectors_file: "vectors-bge-m3.bin".into(),
                labels_gz_file: None,
                metadata_gz_file: None,
                labels_sha256: "l".into(),
                labels_bytes: 10,
                vectors_sha256: "v".into(),
                vectors_bytes: 10,
                last_modified_http: None,
                metadata_file: None,
                metadata_sha256: None,
                metadata_bytes: None,
                premise_embedding_model: None,
                premise_dims: None,
                premise_count: None,
                premise_labels_file: None,
                premise_labels_sha256: None,
                premise_labels_bytes: None,
                premise_vectors_file: None,
                premise_vectors_sha256: None,
                premise_vectors_bytes: None,
                facets_file: None,
                facets_sha256: None,
                facets_bytes: None,
            },
            labels: blob("labels-t02.json"),
            vectors: blob("vectors-bge-m3.bin"),
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
        let json = build_descriptor("https://atlas.test", &dataset(Some("ed25519:AAAA")), false);
        assert!(json.contains(r#""signature":"ed25519:AAAA""#), "got {json}");
    }

    /// An unsigned dataset must serialize byte-identically to before the field existed, so existing
    /// providers keep decoding unchanged (the app reads it as `decodeIfPresent`).
    #[test]
    fn unsigned_dataset_omits_the_field_entirely() {
        let json = build_descriptor("https://atlas.test", &dataset(None), false);
        assert!(!json.contains("signature"), "got {json}");
    }
}
