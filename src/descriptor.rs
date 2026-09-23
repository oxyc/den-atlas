//! `dataset.json` descriptor — a byte-identical port of `buildDescriptor` (`src/descriptor.ts`). Field order
//! matches the TS object so `serde_json::to_string` == `JSON.stringify`.
//!
//! It describes the dataset; it no longer points at anything to download. The blob entries (`labels`,
//! `vectors`, `metadata`, `premise`, `facets`) named sidecars nothing fetches any more, and are gone with
//! the routes that served them (#113). What it does carry of the release's files is their hashes: every
//! top-level `…Sha256` of the meta, because the `den.dataset.v2` signature is over them.

use crate::dataset::Dataset;
use serde::Serialize;
use std::collections::BTreeMap;

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
    /// Every top-level `…Sha256` string of the meta, verbatim and in key byte order. The `den.dataset.v2`
    /// signature covers them, so without them the app cannot rebuild the payload it verifies.
    #[serde(flatten)]
    sha256: BTreeMap<String, String>,
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
        sha256: ds.meta.sha256.clone(),
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
                sha256: BTreeMap::new(),
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

    /// A signed release's meta as the publisher writes it: top-level hashes (one in upper case, so byte
    /// order and case-insensitive order disagree), a nested `storeInputs[]` list whose hashes are not
    /// signed, and a key that merely contains "Sha256" without ending in it.
    const SIGNED_META: &str = r#"{"datasetVersion":"v9","taxonomyVersion":"t02","embeddingModel":"m","dims":2,
        "quantization":"int8","storeFile":"s.store","signature":"ed25519:AAAA",
        "storeSha256":"5e1f","labelsSha256":"a0b1","XrefSha256":"77aa","premiseVectorsSha256":"c3d4",
        "storeInputs":[{"name":"plot.bin","sha256":"nested1","inputSha256":"nested2"}],
        "Sha256Note":"not a hash key","builtAt":"now"}"#;

    /// The fixture meta, loaded through `Dataset::load` exactly as the server loads a release.
    fn loaded(name: &str) -> (Dataset, PathBuf) {
        let root = std::env::temp_dir().join(format!("den-atlas-desc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let title = crate::store::fixture::Title {
            media: 0,
            tmdb_id: 1,
            plot: vec![0, 0],
            premise: vec![0, 0],
            ..crate::store::fixture::Title::default()
        };
        crate::store::fixture::write(&root.join("s.store"), "v9", 2, &[title], &[]);
        std::fs::write(root.join("dataset.meta.json"), SIGNED_META).unwrap();
        (Dataset::load(&root).expect("the signed fixture must load"), root)
    }

    /// Every top-level `…Sha256` string of the meta reaches the descriptor under its own name with its own
    /// value — read off the meta generically, so a hash the publisher adds needs no change here.
    #[test]
    fn every_top_level_sha256_of_the_meta_is_served_verbatim() {
        let (ds, root) = loaded("verbatim");
        let served: serde_json::Value = serde_json::from_str(&build_descriptor(&ds, true, true)).unwrap();
        let meta: serde_json::Value = serde_json::from_str(SIGNED_META).unwrap();
        let expected: Vec<_> =
            meta.as_object().unwrap().iter().filter(|(key, _)| key.ends_with("Sha256")).collect();
        assert_eq!(expected.len(), 4, "the fixture must exercise several hashes");
        for (key, value) in expected {
            assert_eq!(served.get(key), Some(value), "{key} was not served verbatim: {served}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only TOP-LEVEL hashes are signed. `storeInputs[]` records what the store was built from; it is
    /// neither served nor lifted to the top level, and a key merely containing "Sha256" is not a hash.
    #[test]
    fn nested_hashes_are_not_served() {
        let (ds, root) = loaded("nested");
        let json = build_descriptor(&ds, false, false);
        for absent in ["storeInputs", "nested1", "nested2", "inputSha256", r#""sha256""#, "Sha256Note"] {
            assert!(!json.contains(absent), "{absent} was served: {json}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The app verifies the signature over a payload it rebuilds from `/dataset.json` alone: the version
    /// header, then one `key=value` line per top-level `…Sha256` string, sorted by key in byte order, joined
    /// by "\n" with no trailing newline. Rebuilt here the way the app does, it must be exactly the bytes
    /// den-dataset signed — written out by hand from the fixture.
    #[test]
    fn the_descriptor_rebuilds_the_exact_signed_v2_payload() {
        let (ds, root) = loaded("payload");
        let served: serde_json::Value = serde_json::from_str(&build_descriptor(&ds, true, true)).unwrap();
        let object = served.as_object().unwrap();
        let mut lines = vec![
            "den.dataset.v2".to_owned(),
            format!("datasetVersion={}", object["datasetVersion"].as_str().unwrap()),
            format!("taxonomyVersion={}", object["taxonomyVersion"].as_str().unwrap()),
        ];
        let mut hashes: Vec<(&String, &str)> = object
            .iter()
            .filter(|(key, _)| key.ends_with("Sha256"))
            .filter_map(|(key, value)| Some((key, value.as_str()?)))
            .collect();
        hashes.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        lines.extend(hashes.into_iter().map(|(key, value)| format!("{key}={value}")));
        let rebuilt = lines.join("\n");

        let signed = "den.dataset.v2\ndatasetVersion=v9\ntaxonomyVersion=t02\nXrefSha256=77aa\n\
                      labelsSha256=a0b1\npremiseVectorsSha256=c3d4\nstoreSha256=5e1f";
        assert_eq!(rebuilt.as_bytes(), signed.as_bytes());
        assert_eq!(object["signature"], "ed25519:AAAA");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/dataset.json` is exempt from the prose guard, but the hash keys would pass it anyway: none of
    /// them normalises to a prohibited key (`premiseVectorsSha256` is not `premise`).
    #[test]
    fn the_hash_keys_pass_the_prose_guard() {
        let (ds, root) = loaded("guard");
        let served: serde_json::Value = serde_json::from_str(&build_descriptor(&ds, true, true)).unwrap();
        assert!(crate::tos::prohibited(&served).is_empty(), "refused: {served}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The descriptor DESCRIBES the dataset; it points at nothing to download. The retired blob entries
    /// (URLs, byte counts) named sidecars the app no longer fetches, behind routes that are gone.
    #[test]
    fn the_descriptor_advertises_no_blob_locations() {
        let (ds, root) = loaded("noblobs");
        let json = build_descriptor(&ds, true, true);
        for gone in ["labels", "vectors", "metadata", "premise", "facets", "url", "sha256", "bytes"] {
            assert!(!json.contains(&format!(r#""{gone}""#)), "{gone} was advertised: {json}");
        }
        assert!(!json.contains("File\"") && !json.contains("Bytes\""), "a file entry was served: {json}");
        assert!(json.contains(r#""quantization":"int8""#), "got {json}");
        assert!(json.contains(r#""embeddingModel":"m""#) && json.contains(r#""dims":2"#), "got {json}");
        assert!(json.contains(r#""queries":true"#) && json.contains(r#""embed":true"#), "got {json}");
        let _ = std::fs::remove_dir_all(&root);
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
