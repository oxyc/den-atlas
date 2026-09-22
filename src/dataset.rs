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

use serde::Deserialize;
use std::path::{Path, PathBuf};

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
}

pub struct Dataset {
    pub meta: Meta,
    /// The one artifact the serving path reads (`den-spec wire/store-v1`; never served).
    pub store: PathBuf,
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
        let meta: Meta = serde_json::from_slice(&raw).map_err(|e| format!("parse dataset.meta.json: {e}"))?;

        // Verified here and the mapping dropped, so the row count below is the store's OWN — the one
        // number about this dataset that has been checked against the bytes rather than claimed by the
        // manifest.
        let store = safe_blob_path(dir, &meta.store_file)?;
        let store_rows = crate::store::MappedStore::open(&store)
            .map_err(|e| format!("store {} is unusable: {e}", meta.store_file))?
            .rows();
        let last_modified = meta.last_modified_http.clone();
        // Writers withdraw the descriptor before replacing the store and publish it last. A load
        // that overlaps that interval must not bind a new store to a descriptor read before it.
        let current_meta =
            std::fs::metadata(&meta_path).and_then(|m| crate::http::FileIdentity::from_metadata(&m));
        if current_meta.as_ref().ok() != Some(&meta_identity) {
            return Err("dataset changed while loading; retry after the refresh completes".into());
        }
        Ok(Dataset { meta, store, store_rows, last_modified })
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
