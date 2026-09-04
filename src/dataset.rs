//! Loads the dataset from `data/` — the `dataset.meta.json` sidecar (which the producer/import writes with
//! per-blob sha256 + size + gzip + the HTTP-date), so the server does ZERO startup hashing or compression.
//! Blob bodies are never read into memory here; they're streamed from disk per request (see `http.rs`).

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone)]
pub struct Meta {
    #[serde(rename = "datasetVersion")]
    pub dataset_version: String,
    #[serde(rename = "taxonomyVersion")]
    pub taxonomy_version: String,
    #[serde(rename = "embeddingModel")]
    pub embedding_model: String,
    pub dims: u32,
    pub count: u64,
    pub quantization: String,
    /// FP-3 — the producer's Ed25519 signature over the canonical descriptor payload ("ed25519:<base64>").
    /// Passed through verbatim to `/dataset.json`; den-atlas neither creates nor validates it. Signing
    /// happens where the dataset is published (`den/scripts/sign-dataset.swift`) and verification happens in
    /// the app against a key the user pinned — an addon that could mint its own signature would prove nothing.
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(rename = "labelsFile")]
    pub labels_file: String,
    #[serde(rename = "vectorsFile")]
    pub vectors_file: String,
    #[serde(rename = "labelsGzFile")]
    pub labels_gz_file: Option<String>,
    #[serde(rename = "labelsSha256")]
    pub labels_sha256: String,
    #[serde(rename = "labelsBytes")]
    pub labels_bytes: u64,
    #[serde(rename = "vectorsSha256")]
    pub vectors_sha256: String,
    #[serde(rename = "vectorsBytes")]
    pub vectors_bytes: u64,
    #[serde(rename = "lastModifiedHttp")]
    pub last_modified_http: Option<String>,
    // Metadata sidecar (optional) — a ≤6-month synced cache of tmdbId→{title,poster_path,year} so the app
    // renders semantic/ANN neighbour cards without a per-result TMDB call. Absent ⇒ served labels+vectors only.
    #[serde(rename = "metadataFile")]
    pub metadata_file: Option<String>,
    #[serde(rename = "metadataSha256")]
    pub metadata_sha256: Option<String>,
    #[serde(rename = "metadataBytes")]
    pub metadata_bytes: Option<u64>,
    // DT-H premise index (optional) — a SECOND labels+vectors index in a DIFFERENT embedding space (tag-string
    // embeddings) served alongside the plot index, so the app clusters "More Like This" by premise. Absent ⇒
    // omitted from the descriptor; the app runs plot-only.
    #[serde(rename = "premiseEmbeddingModel")]
    pub premise_embedding_model: Option<String>,
    #[serde(rename = "premiseDims")]
    pub premise_dims: Option<u32>,
    #[serde(rename = "premiseCount")]
    pub premise_count: Option<u64>,
    #[serde(rename = "premiseLabelsFile")]
    pub premise_labels_file: Option<String>,
    #[serde(rename = "premiseLabelsSha256")]
    pub premise_labels_sha256: Option<String>,
    #[serde(rename = "premiseLabelsBytes")]
    pub premise_labels_bytes: Option<u64>,
    #[serde(rename = "premiseVectorsFile")]
    pub premise_vectors_file: Option<String>,
    #[serde(rename = "premiseVectorsSha256")]
    pub premise_vectors_sha256: Option<String>,
    #[serde(rename = "premiseVectorsBytes")]
    pub premise_vectors_bytes: Option<u64>,
    // DT-I facet blob (optional) — compact per-film country/language/year/media-type/popularity for on-device
    // attribute search ("spanish series"). Absent ⇒ omitted from the descriptor.
    #[serde(rename = "facetsFile")]
    pub facets_file: Option<String>,
    #[serde(rename = "facetsSha256")]
    pub facets_sha256: Option<String>,
    #[serde(rename = "facetsBytes")]
    pub facets_bytes: Option<u64>,
}

pub struct Gz {
    pub path: PathBuf,
    pub size: u64,
}

pub struct Blob {
    /// Served path + descriptor filename, e.g. `labels-t01.json`.
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub content_type: &'static str,
    pub gz: Option<Gz>,
}

pub struct Dataset {
    pub meta: Meta,
    pub labels: Blob,
    pub vectors: Blob,
    /// Optional metadata sidecar blob (poster/title cache); None when the meta declares no sidecar.
    pub metadata: Option<Blob>,
    /// DT-H premise index blobs (optional): a second labels+vectors pair in the tag-embedding space. Both
    /// present or both None (the meta must declare the pair fully).
    pub premise_labels: Option<Blob>,
    pub premise_vectors: Option<Blob>,
    /// DT-I compact facet blob (optional).
    pub facets: Option<Blob>,
    /// HTTP-date for `Last-Modified` (verbatim from the meta sidecar).
    pub last_modified: Option<String>,
}

impl Dataset {
    /// Read `dir/dataset.meta.json` + resolve the two blobs. Fails loudly if the meta or a blob is missing —
    /// a misconfigured deploy should not serve half a dataset.
    pub fn load(dir: &Path) -> Result<Dataset, String> {
        let meta_path = dir.join("dataset.meta.json");
        let raw = std::fs::read(&meta_path)
            .map_err(|e| format!("read {}: {e}", meta_path.display()))?;
        let meta: Meta = serde_json::from_slice(&raw)
            .map_err(|e| format!("parse dataset.meta.json: {e}"))?;

        let labels = resolve_blob(
            dir,
            &meta.labels_file,
            meta.labels_bytes,
            &meta.labels_sha256,
            "application/json",
            meta.labels_gz_file.as_deref(),
        )?;
        let vectors = resolve_blob(
            dir,
            &meta.vectors_file,
            meta.vectors_bytes,
            &meta.vectors_sha256,
            "application/octet-stream",
            None,
        )?;
        // Resolve the optional sidecar only when the meta fully declares it (file + sha + bytes).
        let metadata = match (&meta.metadata_file, &meta.metadata_sha256, meta.metadata_bytes) {
            (Some(file), Some(sha), Some(bytes)) => {
                Some(resolve_blob(dir, file, bytes, sha, "application/json", None)?)
            }
            _ => None,
        };
        // DT-H premise index — resolved only when the meta fully declares BOTH blobs (labels + vectors).
        let (premise_labels, premise_vectors) = match (
            &meta.premise_labels_file, &meta.premise_labels_sha256, meta.premise_labels_bytes,
            &meta.premise_vectors_file, &meta.premise_vectors_sha256, meta.premise_vectors_bytes,
        ) {
            (Some(lf), Some(ls), Some(lb), Some(vf), Some(vs), Some(vb)) => (
                Some(resolve_blob(dir, lf, lb, ls, "application/json", None)?),
                Some(resolve_blob(dir, vf, vb, vs, "application/octet-stream", None)?),
            ),
            _ => (None, None),
        };
        // DT-I facet blob — resolved only when the meta fully declares it.
        let facets = match (&meta.facets_file, &meta.facets_sha256, meta.facets_bytes) {
            (Some(file), Some(sha), Some(bytes)) => {
                Some(resolve_blob(dir, file, bytes, sha, "application/octet-stream", None)?)
            }
            _ => None,
        };
        let last_modified = meta.last_modified_http.clone();
        Ok(Dataset {
            meta,
            labels,
            vectors,
            metadata,
            premise_labels,
            premise_vectors,
            facets,
            last_modified,
        })
    }
}

/// `dir/name` where `name` must be a plain file name. Rejects anything with a separator, a parent
/// component, or a root — the three ways `Path::join` stops meaning "inside dir".
fn safe_blob_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(name);
    let mut parts = candidate.components();
    let only = matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !only {
        return Err(format!("blob name {name:?} is not a plain file name"));
    }
    Ok(dir.join(name))
}

fn resolve_blob(
    dir: &Path,
    name: &str,
    size: u64,
    sha256: &str,
    content_type: &'static str,
    gz_file: Option<&str>,
) -> Result<Blob, String> {
    // A blob name is a FILE NAME, not a path. `dir.join` on "../secret" walks out, and on an
    // absolute path discards `dir` entirely — and these names come from dataset.meta.json, which
    // scripts/fetch-dataset.sh pulls from a GitHub release over the network. FP-3's own rationale
    // names a compromised dataset host as the adversary, and den-atlas loads that meta without
    // checking the signature it passes through, so this was arbitrary file read over HTTP.
    let path = safe_blob_path(dir, name)?;
    // Use the on-disk length, not the meta's declared size: if a refreshed/stale meta disagrees with the
    // actual file, trusting the meta makes Content-Length/Range framing hang or desync the connection.
    let actual = std::fs::metadata(&path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if actual != size {
        eprintln!(
            "den-atlas: {} is {actual} bytes but meta declares {size} — using the on-disk size",
            path.display()
        );
    }
    let gz = match gz_file {
        Some(gzname) => {
            let gzpath = safe_blob_path(dir, gzname)?;
            let sz = std::fs::metadata(&gzpath)
                .map_err(|e| format!("stat {}: {e}", gzpath.display()))?
                .len();
            Some(Gz { path: gzpath, size: sz })
        }
        None => None,
    };
    Ok(Blob {
        name: name.to_owned(),
        path,
        size: actual,
        sha256: sha256.to_owned(),
        content_type,
        gz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blob names come from dataset.meta.json, which the refresh script pulls from a GitHub release
    /// over the network — and FP-3's rationale names a compromised dataset host as the adversary.
    /// `dir.join` walks out on "../x" and discards `dir` outright on an absolute path, so this was
    /// arbitrary file read over HTTP, bounded only by what the container process can read.
    #[test]
    fn a_blob_name_cannot_escape_the_dataset_directory() {
        let dir = Path::new("/data/den-atlas");
        for bad in ["../secret.txt", "/etc/passwd", "a/b.json", "..", "./x.json", ""] {
            assert!(safe_blob_path(dir, bad).is_err(), "{bad:?} was accepted as a blob name");
        }
        assert_eq!(safe_blob_path(dir, "labels-t02.json").unwrap(), dir.join("labels-t02.json"));
    }

    /// ...and through the CALL SITE, because a helper's own test cannot see `resolve_blob` going
    /// back to `dir.join`. A real secret outside the dataset dir, reachable by the traversal.
    #[test]
    fn resolve_blob_refuses_to_read_outside_the_dataset_directory() {
        let root = std::env::temp_dir().join(format!("den-atlas-trav-{}", std::process::id()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"a credential the dataset dir must not reach").unwrap();
        let len = std::fs::metadata(&secret).unwrap().len();

        for escape in ["../secret.txt", secret.to_string_lossy().as_ref()] {
            let r = resolve_blob(&data, escape, len, "sha", "application/json", None);
            assert!(r.is_err(), "{escape:?} resolved to a readable blob outside the dataset dir");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
