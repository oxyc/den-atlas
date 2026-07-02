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
}

pub fn build_descriptor(origin: &str, ds: &Dataset) -> String {
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
    };
    serde_json::to_string(&d).unwrap()
}
