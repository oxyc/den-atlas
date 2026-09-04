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
    /// The metadata sidecar's precompressed variant. The sidecar is the only other JSON blob, so it
    /// is the only other one compression pays for — vectors, premise vectors and facets are packed
    /// binary and gzip buys nothing on them. Absent unless the producer publishes it, in which case
    /// this serves it exactly like the labels variant; the server never compresses anything itself.
    #[serde(rename = "metadataGzFile")]
    pub metadata_gz_file: Option<String>,
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
        let raw = std::fs::read(&meta_path).map_err(|e| format!("read {}: {e}", meta_path.display()))?;
        let meta: Meta = serde_json::from_slice(&raw).map_err(|e| format!("parse dataset.meta.json: {e}"))?;

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
        // The OPTIONAL blobs degrade; they do not take the dataset with them.
        //
        // These used to propagate with `?`, so one missing or unreadable sidecar failed the whole
        // load: the addon then served no labels, no vectors, 503 on /dataset.json and 404 on every
        // blob route, because a secondary index was absent. The mandatory pair above still fails
        // hard — without those there is no dataset — but the difference between "no premise index"
        // and "no dataset at all" is the difference between plot-only More Like This and an addon
        // that does nothing. Each is reported so an absent feature is never silent.
        let metadata = optional_blob(
            dir,
            "metadata",
            &meta.metadata_file,
            &meta.metadata_sha256,
            meta.metadata_bytes,
            "application/json",
            meta.metadata_gz_file.as_deref(),
        );
        // DT-H premise index — both halves or neither; one without the other is not a usable index.
        let premise_labels = optional_blob(
            dir,
            "premiseLabels",
            &meta.premise_labels_file,
            &meta.premise_labels_sha256,
            meta.premise_labels_bytes,
            "application/json",
            None,
        );
        let premise_vectors = optional_blob(
            dir,
            "premiseVectors",
            &meta.premise_vectors_file,
            &meta.premise_vectors_sha256,
            meta.premise_vectors_bytes,
            "application/octet-stream",
            None,
        );
        let (premise_labels, premise_vectors) = match (premise_labels, premise_vectors) {
            (Some(l), Some(v)) => (Some(l), Some(v)),
            (l, v) => {
                if l.is_some() || v.is_some() {
                    eprintln!("den-atlas: premise index incomplete (one of its two blobs is unusable) — serving without it");
                }
                (None, None)
            }
        };
        // DT-I facet blob.
        let facets = optional_blob(
            dir,
            "facets",
            &meta.facets_file,
            &meta.facets_sha256,
            meta.facets_bytes,
            "application/octet-stream",
            None,
        );
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
    let only = matches!((parts.next(), parts.next()), (Some(std::path::Component::Normal(_)), None));
    if !only {
        return Err(format!("blob name {name:?} is not a plain file name"));
    }
    Ok(dir.join(name))
}

/// An optional blob: resolved only when the meta fully declares it (file + sha + bytes), and
/// reported-then-dropped when it is declared but unusable. Never fatal — see the call site.
fn optional_blob(
    dir: &Path,
    label: &str,
    file: &Option<String>,
    sha256: &Option<String>,
    bytes: Option<u64>,
    content_type: &'static str,
    gz_file: Option<&str>,
) -> Option<Blob> {
    let (file, sha256, bytes) = match (file, sha256, bytes) {
        (Some(f), Some(s), Some(b)) => (f, s, b),
        // Not declared, or declared incompletely. Absent by design, so nothing to report.
        _ => return None,
    };
    match resolve_blob(dir, file, bytes, sha256, content_type, gz_file) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("den-atlas: optional blob {label} ({file}) is unusable ({e}) — serving without it");
            None
        }
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
    // A blob name is a FILE NAME, not a path. `dir.join` on "../secret" walks out, and on an
    // absolute path discards `dir` entirely — and these names come from dataset.meta.json, which
    // scripts/fetch-dataset.sh pulls from a GitHub release over the network. FP-3's own rationale
    // names a compromised dataset host as the adversary, and den-atlas loads that meta without
    // checking the signature it passes through, so this was arbitrary file read over HTTP.
    let path = safe_blob_path(dir, name)?;
    // Use the on-disk length, not the meta's declared size: if a refreshed/stale meta disagrees with the
    // actual file, trusting the meta makes Content-Length/Range framing hang or desync the connection.
    let actual = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?.len();
    if actual != size {
        eprintln!(
            "den-atlas: {} is {actual} bytes but meta declares {size} — using the on-disk size",
            path.display()
        );
    }
    // The gz variant is an OPTIMISATION, so an unusable one drops the variant — it does not take the
    // dataset down. Propagating here made a single bad optional name fatal for labels, vectors,
    // premise and facets alike: `"labelsGzFile": ""` served 503 on /dataset.json and 404 on every
    // blob, where before it merely produced a junk variant nobody could select. The sync script
    // treats an empty *File value as an ordinary shape (`[ -n "$f" ] || continue`), so this is a
    // release away, and main.rs's stated posture is to degrade rather than refuse.
    let gz = match gz_file {
        Some(gzname) => match safe_blob_path(dir, gzname).and_then(|p| {
            std::fs::metadata(&p)
                .map(|m| Gz { path: p.clone(), size: m.len() })
                .map_err(|e| format!("stat {}: {e}", p.display()))
        }) {
            Ok(gz) => Some(gz),
            Err(why) => {
                eprintln!("den-atlas: no gzip variant for {name} ({why}) — serving identity only");
                None
            }
        },
        None => None,
    };
    Ok(Blob { name: name.to_owned(), path, size: actual, sha256: sha256.to_owned(), content_type, gz })
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

    /// The gz variant is the SECOND call site, and the first version of this test passed `None` for
    /// it — so reverting that one line to `dir.join` left the suite green while
    /// `"labelsGzFile": "../../../etc/shadow"` was served to anyone sending `Accept-Encoding: gzip`.
    #[test]
    fn a_gz_blob_name_cannot_escape_either() {
        let root = std::env::temp_dir().join(format!("den-atlas-gz-{}", std::process::id()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("labels.json"), b"{}").unwrap();
        let secret = root.join("secret.gz");
        std::fs::write(&secret, b"not for the wire").unwrap();

        // An escaping gz name must not resolve to the file outside the data dir...
        let blob = resolve_blob(&data, "labels.json", 2, "sha", "application/json", Some("../secret.gz"))
            .expect("a bad OPTIONAL name must not take the whole dataset down");
        assert!(blob.gz.is_none(), "a gz variant resolved outside the dataset directory");

        // ...and neither must an absolute one.
        let blob = resolve_blob(
            &data,
            "labels.json",
            2,
            "sha",
            "application/json",
            Some(secret.to_string_lossy().as_ref()),
        )
        .expect("still not fatal");
        assert!(blob.gz.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unusable OPTIONAL blob drops the variant; it does not refuse the dataset. Propagating the
    /// error made `"labelsGzFile": ""` serve 503 on /dataset.json and 404 on every blob — and the
    /// sync script treats an empty *File value as an ordinary shape, so that is one release away.
    #[test]
    fn an_unusable_gzip_variant_does_not_take_the_dataset_down() {
        let root = std::env::temp_dir().join(format!("den-atlas-optgz-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("labels.json"), b"{}").unwrap();

        for bad in ["", "missing.json.gz", "../x.gz"] {
            let blob = resolve_blob(&root, "labels.json", 2, "sha", "application/json", Some(bad))
                .unwrap_or_else(|e| panic!("{bad:?} took the whole dataset down: {e}"));
            assert!(blob.gz.is_none(), "{bad:?} produced a gz variant");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An optional blob that is declared but unusable must cost that ONE feature, not the dataset.
    /// These propagated with `?`, so a missing premise index — a secondary "More Like This" signal —
    /// meant no labels, no vectors, 503 on /dataset.json and 404 on every blob route.
    #[test]
    fn an_unusable_optional_blob_does_not_take_the_dataset_down() {
        let root = std::env::temp_dir().join(format!("den-atlas-optblob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("labels.json"), b"LABELS").unwrap();
        std::fs::write(root.join("vectors.bin"), b"VECTORS!").unwrap();
        std::fs::write(root.join("facets.bin"), b"FACETS").unwrap();
        // metadata, and both premise blobs, are DECLARED but never written.
        std::fs::write(
            root.join("dataset.meta.json"),
            br#"{"datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,"count":1,
                 "quantization":"int8",
                 "labelsFile":"labels.json","labelsBytes":6,"labelsSha256":"a",
                 "vectorsFile":"vectors.bin","vectorsBytes":8,"vectorsSha256":"b",
                 "metadataFile":"gone.json","metadataBytes":8,"metadataSha256":"c",
                 "facetsFile":"facets.bin","facetsBytes":6,"facetsSha256":"d",
                 "premiseLabelsFile":"gone-pl.json","premiseLabelsBytes":7,"premiseLabelsSha256":"e",
                 "premiseVectorsFile":"gone-pv.bin","premiseVectorsBytes":8,"premiseVectorsSha256":"f"}"#,
        )
        .unwrap();

        let ds = Dataset::load(&root).expect("a missing optional blob took the whole dataset down");
        assert_eq!(ds.labels.name, "labels.json", "the mandatory blobs must still be there");
        assert_eq!(ds.vectors.size, 8);
        assert!(ds.metadata.is_none(), "an unreadable metadata sidecar was resolved anyway");
        assert!(ds.premise_labels.is_none() && ds.premise_vectors.is_none());
        // ...and a usable optional blob is still served.
        assert!(ds.facets.is_some(), "a perfectly good facets blob was dropped with the broken ones");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Half a premise index is not an index: the app needs both blobs to cluster by premise, and
    /// advertising one of them would have it fetch a pair it cannot use.
    #[test]
    fn half_a_premise_index_is_dropped_whole() {
        let root = std::env::temp_dir().join(format!("den-atlas-halfpremise-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("labels.json"), b"LABELS").unwrap();
        std::fs::write(root.join("vectors.bin"), b"VECTORS!").unwrap();
        std::fs::write(root.join("premise-labels.json"), b"PLABELS").unwrap();
        std::fs::write(
            root.join("dataset.meta.json"),
            br#"{"datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,"count":1,
                 "quantization":"int8",
                 "labelsFile":"labels.json","labelsBytes":6,"labelsSha256":"a",
                 "vectorsFile":"vectors.bin","vectorsBytes":8,"vectorsSha256":"b",
                 "premiseLabelsFile":"premise-labels.json","premiseLabelsBytes":7,"premiseLabelsSha256":"e",
                 "premiseVectorsFile":"gone-pv.bin","premiseVectorsBytes":8,"premiseVectorsSha256":"f"}"#,
        )
        .unwrap();

        let ds = Dataset::load(&root).expect("dataset must still load");
        assert!(ds.premise_labels.is_none(), "the premise labels were kept without their vectors");
        assert!(ds.premise_vectors.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
