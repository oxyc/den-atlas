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
        let last_modified = meta.last_modified_http.clone();
        Ok(Dataset {
            meta,
            labels,
            vectors,
            last_modified,
        })
    }
}

fn resolve_blob(
    dir: &Path,
    name: &str,
    size: u64,
    sha256: &str,
    content_type: &'static str,
    gz_file: Option<&str>,
) -> Result<Blob, String> {
    let path = dir.join(name);
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
            let gzpath = dir.join(gzname);
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
