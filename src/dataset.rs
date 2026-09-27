//! Loads the dataset from `data/` — the `dataset.meta.json` sidecar the producer/import writes.
//!
//! It hashes the store once, at load. `store-v1` puts a content hash in the header and den-spec's rule
//! is to verify it before any cast, so taking the row count off an unverified header would be precisely
//! the silent misread the format was designed to make impossible. ~130 ms on a 131 MB store, once per
//! process.
//!
//! # One artifact
//!
//! `storeFile` is the dataset — the only file this reads and the only one it describes. Everything the
//! serving path needs — labels, both vector matrices, facts, cards, facet rows, votes — is a section of
//! it, so a dataset without a readable store does not load. The sidecar blobs it replaced (`labelsFile`,
//! `vectorsFile`, `metadataFile`, the premise pair, `facetsFile`) were served to an app that no longer
//! fetches them and are gone: no field describes them and no route streams them (#113).

use crate::store::MappedStore;
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Deserialize, Clone)]
pub struct Meta {
    #[serde(rename = "datasetVersion")]
    pub dataset_version: String,
    #[serde(rename = "taxonomyVersion")]
    pub taxonomy_version: String,
    /// The embedding model, its width and its quantisation: properties of the store's vector sections,
    /// and what `POST /embed` must produce a query vector in. Read by `handler`'s embed route (the memo
    /// key and the expected dimension) and served to the app, so they stay required.
    #[serde(rename = "embeddingModel")]
    pub embedding_model: String,
    pub dims: u32,
    pub quantization: String,
    /// The raw embedder space named by the producer's canary. Old manifests predate this field; a
    /// transformed plot matrix may not, because its transform explicitly names the raw space it accepts.
    #[serde(rename = "embeddingSpace", default)]
    pub embedding_space: Option<String>,
    /// Plot rows already have the measured length direction removed. The decoded direction is kept so raw
    /// den-embed query vectors can undergo the same operation before scanning the plot matrix. Premise rows
    /// remain in `embeddingSpace` and never read this.
    #[serde(rename = "plotVectorTransform", default)]
    pub plot_vector_transform: Option<PlotVectorTransform>,
    #[serde(rename = "plotVectorTransformSha256", default)]
    pub plot_vector_transform_sha256: Option<String>,
    /// FP-3 — the producer's Ed25519 signature over the canonical descriptor payload ("ed25519:<base64>").
    /// Passed through verbatim to `/dataset.json`; den-atlas neither creates nor validates it. Signing
    /// happens where the dataset is published (`den/scripts/sign-dataset.swift`) and verification happens in
    /// the app against a key the user pinned — an addon that could mint its own signature would prove nothing.
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(rename = "lastModifiedHttp")]
    pub last_modified_http: Option<String>,
    /// The store's sha256, as the publisher recorded it. The store has been rebuilt under an unchanged
    /// `datasetVersion`, so this — not the version — says which bytes a playground export was ranked on.
    #[serde(rename = "storeSha256", default)]
    pub store_sha256: Option<String>,
    // The store (den-spec wire/store-v1) — every per-title signal the serving path reads, plus both
    // vector matrices, in one mmap'd file. Read from disk, NEVER served: it is an implementation detail
    // of this server, not an artifact a client fetches.
    //
    // MANDATORY, and the only file the meta names. A dataset with no readable store has nothing behind
    // any of its query routes, so it does not load.
    #[serde(rename = "storeFile")]
    pub store_file: String,
    /// Every top-level string field of the meta whose key ends in `Sha256`, key → value verbatim.
    ///
    /// The `den.dataset.v2` signature covers these lines (sorted by key), so `/dataset.json` must carry
    /// every one for the app to rebuild the signed payload. Collected by key suffix rather than named, so
    /// an artifact the publisher adds is covered without a change here. Nested hashes (`storeInputs[]`)
    /// are not signed and not collected.
    #[serde(skip)]
    pub sha256: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PlotVectorTransform {
    pub schema: u32,
    pub algorithm: String,
    pub dims: u32,
    #[serde(rename = "inputEmbeddingSpace")]
    pub input_embedding_space: String,
    #[serde(rename = "directionEncoding")]
    pub direction_encoding: String,
    #[serde(rename = "directionBase64")]
    direction_base64: String,
    #[serde(rename = "directionSha256")]
    pub direction_sha256: String,
    #[serde(rename = "fitMethod")]
    pub fit_method: String,
    #[serde(rename = "fitArtifactSha256")]
    pub fit_artifact_sha256: String,
    /// Decoded only after every manifest checksum and shape check passes.
    #[serde(skip)]
    pub direction: Box<[f32]>,
}

impl Meta {
    /// Identity for anything memoising a semantic query. `embeddingSpace` names the raw result from
    /// den-embed; the suffix names the plot-space operation Atlas applies before one of the two scans.
    /// Keeping it in the key prevents a hot process from reusing a vector under a changed transform.
    pub fn semantic_query_space(&self) -> String {
        let raw = self
            .embedding_space
            .clone()
            .unwrap_or_else(|| format!("{}:{}:{}", self.embedding_model, self.dims, self.quantization));
        self.plot_vector_transform_sha256
            .as_ref()
            .map_or(raw.clone(), |digest| format!("{raw}:plot-transform:{digest}"))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_sha256(name: &str, digest: &str) -> Result<(), String> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err(format!("{name} must be 64 lowercase hexadecimal characters"));
    }
    Ok(())
}

fn parse_plot_vector_transform(value: &serde_json::Value, meta: &mut Meta) -> Result<(), String> {
    let object = value.get("plotVectorTransform");
    let digest = value.get("plotVectorTransformSha256");
    match (object, digest) {
        (None, None) => return Ok(()),
        (None, Some(_)) | (Some(_), None) => {
            return Err("plotVectorTransform and plotVectorTransformSha256 must appear together".into());
        }
        (Some(_), Some(_)) => {}
    }
    let object = object.expect("matched above");
    if !object.is_object() {
        return Err("plotVectorTransform must be an object".into());
    }
    let digest =
        digest.and_then(serde_json::Value::as_str).ok_or("plotVectorTransformSha256 must be a string")?;
    validate_sha256("plotVectorTransformSha256", digest)?;
    let canonical =
        serde_json::to_vec(object).map_err(|e| format!("canonicalise plotVectorTransform: {e}"))?;
    if sha256_hex(&canonical) != digest {
        return Err("plotVectorTransformSha256 does not match the canonical transform object".into());
    }

    let transform =
        meta.plot_vector_transform.as_mut().ok_or("plotVectorTransform did not parse as an object")?;
    if transform.schema != 1 {
        return Err(format!("unsupported plotVectorTransform schema {}", transform.schema));
    }
    if transform.algorithm != "unit-orthogonal-projection-v1" {
        return Err(format!("unsupported plotVectorTransform algorithm {:?}", transform.algorithm));
    }
    if transform.direction_encoding != "base64-f32-le" {
        return Err(format!(
            "unsupported plotVectorTransform directionEncoding {:?}",
            transform.direction_encoding
        ));
    }
    if transform.fit_method != "ols-unit-int8-on-ln-english-plot-chars-v1" {
        return Err(format!("unsupported plotVectorTransform fitMethod {:?}", transform.fit_method));
    }
    if transform.dims != meta.dims {
        return Err(format!(
            "plotVectorTransform dims {} do not match manifest dims {}",
            transform.dims, meta.dims
        ));
    }
    let embedding_space =
        meta.embedding_space.as_deref().ok_or("plotVectorTransform requires manifest embeddingSpace")?;
    if transform.input_embedding_space != embedding_space {
        return Err("plotVectorTransform inputEmbeddingSpace does not match manifest embeddingSpace".into());
    }
    validate_sha256("plotVectorTransform directionSha256", &transform.direction_sha256)?;
    validate_sha256("plotVectorTransform fitArtifactSha256", &transform.fit_artifact_sha256)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&transform.direction_base64)
        .map_err(|e| format!("plotVectorTransform directionBase64 is invalid: {e}"))?;
    let expected = transform.dims as usize * std::mem::size_of::<f32>();
    if bytes.len() != expected {
        return Err(format!("plotVectorTransform direction is {} bytes, expected {expected}", bytes.len()));
    }
    if sha256_hex(&bytes) != transform.direction_sha256 {
        return Err("plotVectorTransform directionSha256 does not match directionBase64".into());
    }
    let direction: Box<[f32]> =
        bytes.as_chunks::<4>().0.iter().map(|&chunk| f32::from_le_bytes(chunk)).collect();
    if direction.iter().any(|value| !value.is_finite()) {
        return Err("plotVectorTransform direction contains a non-finite value".into());
    }
    let norm = direction.iter().map(|&value| f64::from(value).powi(2)).sum::<f64>().sqrt();
    if (norm - 1.0).abs() > 1e-3 {
        return Err(format!("plotVectorTransform direction is not unit length (norm {norm})"));
    }
    transform.direction = direction;
    Ok(())
}

/// The top-level `…Sha256` string fields of a parsed `dataset.meta.json`, in key byte order.
fn top_level_sha256(meta: &serde_json::Value) -> BTreeMap<String, String> {
    let Some(object) = meta.as_object() else { return BTreeMap::new() };
    object
        .iter()
        .filter(|(key, _)| key.ends_with("Sha256"))
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
        .collect()
}

pub struct Dataset {
    pub meta: Meta,
    /// The one artifact the serving path reads (`den-spec wire/store-v1`; never served).
    pub store: PathBuf,
    /// That store, mapped and verified here, once, for everything in the process that reads it.
    pub mapped: Arc<MappedStore>,
    /// Titles in that store, from its verified header.
    ///
    /// The only title count anything can check. The manifest's `count` was the LABELLED subset (47,539
    /// where the store holds 47,618), nothing validated it, and it was served to the app as the corpus
    /// size — so it is gone and this is what `/dataset.json` and the query routes count with.
    pub store_rows: usize,
    /// HTTP-date for `Last-Modified` (verbatim from the meta sidecar).
    pub last_modified: Option<String>,
}

impl Dataset {
    /// Read `dir/dataset.meta.json` and open the store it declares.
    ///
    /// Fails loudly on either: the store is the only artifact this reads, so a dataset without a
    /// readable one has nothing behind any query route.
    pub fn load(dir: &Path) -> Result<Dataset, String> {
        use std::io::Read;
        let meta_path = dir.join("dataset.meta.json");
        let mut file =
            std::fs::File::open(&meta_path).map_err(|e| format!("read {}: {e}", meta_path.display()))?;
        let meta_identity = file
            .metadata()
            .and_then(|m| crate::http::FileIdentity::from_metadata(&m))
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        let value: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| format!("parse dataset.meta.json: {e}"))?;
        let sha256 = top_level_sha256(&value);
        let mut meta: Meta =
            serde_json::from_value(value.clone()).map_err(|e| format!("parse dataset.meta.json: {e}"))?;
        meta.sha256 = sha256;
        parse_plot_vector_transform(&value, &mut meta)?;

        // Verified here, once for the process, so the row count below is the store's OWN — the one number
        // about this dataset that has been checked against the bytes rather than claimed by the manifest —
        // and every later reader shares this mapping rather than hashing the file again.
        let store = safe_blob_path(dir, &meta.store_file)?;
        let mapped =
            MappedStore::open(&store).map_err(|e| format!("store {} is unusable: {e}", meta.store_file))?;
        let store_rows = mapped.rows();
        let last_modified = meta.last_modified_http.clone();
        // Writers withdraw the descriptor before replacing the store and publish it last. A load
        // that overlaps that interval must not bind a new store to a descriptor read before it.
        let current_meta =
            std::fs::metadata(&meta_path).and_then(|m| crate::http::FileIdentity::from_metadata(&m));
        if current_meta.as_ref().ok() != Some(&meta_identity) {
            return Err("dataset changed while loading; retry after the refresh completes".into());
        }
        Ok(Dataset { meta, store, mapped: Arc::new(mapped), store_rows, last_modified })
    }
}

/// `dir/name` where `name` must be a plain file name. Rejects anything with a separator, a parent
/// component, or a root — the three ways `Path::join` stops meaning "inside dir".
///
/// The name comes from dataset.meta.json, which scripts/fetch-dataset.sh pulls from a GitHub release
/// over the network. `dir.join` on "../secret" walks out, and on an absolute path discards `dir`
/// entirely. FP-3's own rationale names a compromised dataset host as the adversary, and den-atlas
/// loads that meta without checking the signature it passes through, so this was arbitrary file read
/// over HTTP.
fn safe_blob_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(name);
    let mut parts = candidate.components();
    let only = matches!((parts.next(), parts.next()), (Some(std::path::Component::Normal(_)), None));
    if !only {
        return Err(format!("blob name {name:?} is not a plain file name"));
    }
    Ok(dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform_object() -> serde_json::Value {
        let direction: Vec<u8> = [1.0f32, 0.0].into_iter().flat_map(f32::to_le_bytes).collect();
        serde_json::json!({
            "schema": 1,
            "algorithm": "unit-orthogonal-projection-v1",
            "dims": 2,
            "inputEmbeddingSpace": "canary-v1:raw",
            "directionEncoding": "base64-f32-le",
            "directionBase64": base64::engine::general_purpose::STANDARD.encode(&direction),
            "directionSha256": sha256_hex(&direction),
            "fitMethod": "ols-unit-int8-on-ln-english-plot-chars-v1",
            "fitArtifactSha256": "11".repeat(32),
        })
    }

    fn transformed_manifest(transform: serde_json::Value) -> serde_json::Value {
        let digest = sha256_hex(&serde_json::to_vec(&transform).unwrap());
        serde_json::json!({
            "datasetVersion": "v9", "taxonomyVersion": "t", "embeddingModel": "m", "dims": 2,
            "quantization": "int8", "embeddingSpace": "canary-v1:raw", "storeFile": "s.store",
            "plotVectorTransform": transform, "plotVectorTransformSha256": digest,
        })
    }

    /// A one-title store in `dir`, named as the manifests below declare it. Every `Dataset::load` test
    /// that is meant to succeed needs one, because the store is the only artifact a dataset has.
    fn store_in(dir: &Path) {
        let title = crate::store::fixture::Title {
            media: 0,
            tmdb_id: 1,
            plot: vec![0, 0],
            premise: vec![0, 0],
            ..crate::store::fixture::Title::default()
        };
        crate::store::fixture::write(&dir.join("s.store"), "v9", 2, &[title], &[]);
    }

    /// The manifest fields every test below shares, the store included.
    const HEAD: &str = r#""datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,
                          "quantization":"int8","storeFile":"s.store","#;

    /// File names come from dataset.meta.json, which the refresh script pulls from a GitHub release
    /// over the network — and FP-3's rationale names a compromised dataset host as the adversary.
    /// `dir.join` walks out on "../x" and discards `dir` outright on an absolute path, so this was
    /// arbitrary file read over HTTP, bounded only by what the container process can read.
    #[test]
    fn a_blob_name_cannot_escape_the_dataset_directory() {
        let dir = Path::new("/data/den-atlas");
        for bad in ["../secret.txt", "/etc/passwd", "a/b.json", "..", "./x.json", ""] {
            assert!(safe_blob_path(dir, bad).is_err(), "{bad:?} was accepted as a blob name");
        }
        assert_eq!(safe_blob_path(dir, "den-v1.store").unwrap(), dir.join("den-v1.store"));
    }

    /// ...and through the CALL SITE, because a helper's own test cannot see `load` going back to
    /// `dir.join`. `storeFile` is now the only name the meta hands to a path, so it is the only
    /// place the traversal can come back. A real secret outside the dataset dir, reachable by it.
    #[test]
    fn the_store_name_cannot_escape_the_dataset_directory() {
        let root = std::env::temp_dir().join(format!("den-atlas-trav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let secret = root.join("secret.store");
        std::fs::write(&secret, b"a credential the dataset dir must not reach").unwrap();

        for escape in ["../secret.store", secret.to_string_lossy().as_ref()] {
            std::fs::write(
                data.join("dataset.meta.json"),
                format!(
                    r#"{{"datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,
                         "quantization":"int8","storeFile":"{escape}"}}"#
                ),
            )
            .unwrap();
            match Dataset::load(&data) {
                Err(err) => assert!(err.contains("plain file name"), "refused for the wrong reason: {err}"),
                Ok(ds) => panic!("{escape:?} resolved to {}", ds.store.display()),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The PRUNED manifest — a store and the five facts about it that are not in it — is the only shape
    /// the publisher writes, and the only one this has to load. A manifest that still declares the
    /// retired sidecars loads the same way: the extra keys are simply not read.
    #[test]
    fn a_manifest_that_declares_only_a_store_loads() {
        let root = std::env::temp_dir().join(format!("den-atlas-pruned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        store_in(&root);
        std::fs::write(root.join("dataset.meta.json"), format!("{{{HEAD}\"builtAt\":\"now\"}}")).unwrap();

        let ds = Dataset::load(&root).expect("the pruned manifest must load");
        assert_eq!(ds.store, root.join("s.store"));
        assert_eq!(ds.store_rows, 1, "the row count comes from the store's verified header");

        // ...and so does a manifest from the generation that still declares the retired sidecars,
        // whether or not their files are on disk. They are unknown keys now, not a contract.
        std::fs::write(
            root.join("dataset.meta.json"),
            format!(
                r#"{{{HEAD}"labelsFile":"labels.json","labelsBytes":6,"labelsSha256":"a",
                     "vectorsFile":"gone.bin","vectorsBytes":8,"vectorsSha256":"b",
                     "metadataFile":"meta.json","metadataBytes":2,"metadataSha256":"c"}}"#
            ),
        )
        .unwrap();
        assert_eq!(Dataset::load(&root).expect("an old manifest must still load").store_rows, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_plot_transform_is_verified_and_becomes_part_of_semantic_space_identity() {
        let root = std::env::temp_dir().join(format!("den-atlas-transform-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        store_in(&root);
        let manifest = transformed_manifest(transform_object());
        assert_eq!(
            manifest["plotVectorTransformSha256"],
            "4cfbf2bf1ab60d49d19cd09b69751568d4cb0846b9cf2a6a4a85bc1cb784bb99",
            "must equal Python json.dumps(sort_keys=True, separators=(',', ':'), ensure_ascii=False)"
        );
        std::fs::write(root.join("dataset.meta.json"), manifest.to_string()).unwrap();

        let dataset = Dataset::load(&root).expect("a complete signed transform must load");
        let transform = dataset.meta.plot_vector_transform.as_ref().expect("the transform is retained");
        assert_eq!(&*transform.direction, &[1.0, 0.0]);
        let digest = manifest["plotVectorTransformSha256"].as_str().unwrap();
        assert_eq!(dataset.meta.semantic_query_space(), format!("canary-v1:raw:plot-transform:{digest}"));
        assert_eq!(dataset.meta.sha256.get("plotVectorTransformSha256").map(String::as_str), Some(digest));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_or_misbinding_plot_transforms_are_refused() {
        let root = std::env::temp_dir().join(format!("den-atlas-transform-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        store_in(&root);
        let mut cases = Vec::new();

        let mut wrong_object_digest = transformed_manifest(transform_object());
        wrong_object_digest["plotVectorTransformSha256"] = serde_json::Value::String("00".repeat(32));
        cases.push((wrong_object_digest, "canonical transform object"));

        let mut wrong_algorithm = transform_object();
        wrong_algorithm["algorithm"] = serde_json::Value::String("something-else".into());
        cases.push((transformed_manifest(wrong_algorithm), "unsupported plotVectorTransform algorithm"));

        let mut wrong_space = transform_object();
        wrong_space["inputEmbeddingSpace"] = serde_json::Value::String("another-space".into());
        cases.push((transformed_manifest(wrong_space), "inputEmbeddingSpace"));

        let mut wrong_direction = transform_object();
        wrong_direction["directionBase64"] = serde_json::Value::String(
            base64::engine::general_purpose::STANDARD
                .encode([0.0f32, 1.0].into_iter().flat_map(f32::to_le_bytes).collect::<Vec<_>>()),
        );
        cases.push((transformed_manifest(wrong_direction), "directionSha256"));

        let mut missing_space = transformed_manifest(transform_object());
        missing_space.as_object_mut().unwrap().remove("embeddingSpace");
        cases.push((missing_space, "requires manifest embeddingSpace"));

        for (manifest, reason) in cases {
            std::fs::write(root.join("dataset.meta.json"), manifest.to_string()).unwrap();
            let error = match Dataset::load(&root) {
                Err(error) => error,
                Ok(_) => panic!("bad transform loaded; expected {reason}"),
            };
            assert!(error.contains(reason), "{error:?} did not name {reason:?}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// ...and the store is the one blob whose absence IS fatal, because nothing else is read. A dataset
    /// that will not produce one has nothing behind `/index/…`, `/recommend` or search, so it must fail
    /// where an operator sees it rather than serve every route empty.
    #[test]
    fn a_dataset_without_a_readable_store_does_not_load() {
        let root = std::env::temp_dir().join(format!("den-atlas-nostore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Declared, never written.
        std::fs::write(root.join("dataset.meta.json"), format!("{{{HEAD}\"builtAt\":\"now\"}}")).unwrap();
        assert!(Dataset::load(&root).is_err(), "a missing store was served anyway");

        // Present, but not a store.
        std::fs::write(root.join("s.store"), b"not a store").unwrap();
        let Err(err) = Dataset::load(&root) else { panic!("a file that is not a store was accepted") };
        assert!(err.contains("s.store"), "the error must name the file: {err}");

        // Not declared at all.
        std::fs::write(
            root.join("dataset.meta.json"),
            br#"{"datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,
                 "quantization":"int8"}"#,
        )
        .unwrap();
        assert!(Dataset::load(&root).is_err(), "a manifest with no storeFile was accepted");
        let _ = std::fs::remove_dir_all(&root);
    }
}
