//! The store on disk: the one impure step, and the shapes the serving path reads out of it.
//!
//! The format and its decoder are not here. `den-spec wire/store-v1.md` is the contract and
//! `den_store` is the reader, in den-core, because that crate must also compile for the browser and for
//! tvOS where `memmap2` does not. What belongs here is the part that touches the filesystem: map the
//! file, verify it once, and keep the mapping alive for the life of the process.
//!
//! # Why mmap and not `read`
//!
//! Loading the dataset used to be ~210 MB of JSON parsed into owned Rust structures: 800 ms before the
//! first query could be answered, and a peak of 552 MB against a 1 GB container limit — of which ~290 MB
//! was a `serde_json` arena that never went back to the OS. A mapped store is addressed in place: no
//! parse, no copy, and the pages are clean and reclaimable rather than anonymous and pinned.
//!
//! One caveat, because it decides where the dataset should live: mapping a file on **tmpfs** buys none of
//! that. tmpfs pages *are* RAM and are charged to the cgroup, so the store would simply be resident.
//! `/var/lib/den/atlas-data` is tmpfs today; moving it to disk is what makes this pay (oxyc/den#113).

use den_store::{Store, StoreError, StoreTable};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

/// A mapped store, and the table proving it was verified.
///
/// `Debug` names the store, never its contents — 131 MB in a test failure helps nobody.
pub struct MappedStore {
    map: Mmap,
    table: StoreTable,
}

impl std::fmt::Debug for MappedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedStore")
            .field("dataset_version", &self.table.dataset_version())
            .field("rows", &self.table.rows())
            .field("bytes", &self.map.len())
            .finish()
    }
}

impl MappedStore {
    /// Map and verify. The content hash is checked over the whole file here, once — about 1 ms per MB,
    /// paid at load so that no query pays it.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: the store is a file we published and the box treats as read-only. A concurrent writer
        // truncating it would be undefined behaviour, which is why `atlas-dataset-sync` stages a new
        // generation beside the old one and renames, rather than writing in place.
        let map = unsafe { Mmap::map(&file) }.map_err(|e| format!("{}: {e}", path.display()))?;
        let table = StoreTable::open(&map).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { map, table })
    }

    /// A view for reading. Free — the hash was checked when this was opened.
    pub fn view(&self) -> Store<'_> {
        self.table.view(&self.map)
    }

    pub fn rows(&self) -> usize {
        self.table.rows()
    }

    /// The `datasetVersion` stamped into the store, for checking it against the manifest that named it.
    pub fn dataset_version(&self) -> &str {
        self.table.dataset_version()
    }

    pub fn bytes(&self) -> usize {
        self.map.len()
    }

    /// Read every section the serving path needs, once, so a store that is missing one fails at LOAD
    /// rather than on the first request that happens to want it.
    ///
    /// Atlas's previous failure mode was the opposite: a facts file that would not parse was discovered
    /// per-feature, so `/recommend`, people search and `imdbId` went quiet one at a time while `/health`
    /// stayed green. A store either has what serving needs or it is not a store we can serve.
    pub fn check(&self) -> Result<(), StoreError> {
        let view = self.view();
        for name in REQUIRED_COLUMNS_U32 {
            view.per_row::<u32>(name)?;
        }
        view.per_row::<u64>("keys")?;
        view.per_row::<u16>("score_intensity")?;
        view.per_row::<u8>("world")?;
        view.strings()?;
        for (values, offsets) in REQUIRED_LISTS {
            view.list::<u32>(values, offsets)?;
        }
        view.column::<i8>("vec_plot")?;
        Ok(())
    }
}

/// A mapped store together with the corpus-wide statistics derived from it.
///
/// The aggregates (prevalence, critique idf and means) are owned rather than borrowed, so this can hold
/// both without a self-referential type: they are computed once, here, by scanning the store, because
/// each is a property of the whole corpus and recomputing one per request would mean scanning per
/// request.
pub struct LoadedStore {
    pub store: MappedStore,
    pub aggregates: crate::rail::RailAggregates,
}

impl LoadedStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let store = MappedStore::open(path)?;
        store.check().map_err(|e| format!("{}: {e}", path.display()))?;
        let aggregates = crate::rail::RailAggregates::build(&store.view())?;
        Ok(Self { store, aggregates })
    }

    pub fn view(&self) -> Store<'_> {
        self.store.view()
    }
}

/// Per-row `u32` columns without which the serving path cannot answer.
const REQUIRED_COLUMNS_U32: &[&str] = &["card_title", "primary_genre", "imdb"];

/// Lists the serving path reads on every More Like This.
const REQUIRED_LISTS: &[(&str, &str)] = &[
    ("makers_v", "makers_o"),
    ("cast_v", "cast_o"),
    ("genres_v", "genres_o"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture den-spec publishes, when it is checked out. Skips rather than passes without it:
    /// a contract test that reports success when the contract is absent is worse than no test.
    fn fixture() -> Option<std::path::PathBuf> {
        let path = std::env::var("DEN_SPEC_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../den-spec")
            })
            .join("vectors/store-v1.store");
        path.is_file().then_some(path)
    }

    /// The real store, when a dataset happens to be on this machine. Opt-in via `DEN_STORE`, because the
    /// fixture proves the format and only a real store proves the scale: 131 MB, 89 sections, 47,618 rows,
    /// and a content hash over every byte of it.
    #[test]
    fn maps_a_real_store_when_one_is_present() {
        let Ok(path) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to exercise this");
            return;
        };
        let store = MappedStore::open(std::path::Path::new(&path)).expect("the store maps");
        assert!(store.rows() > 40_000, "a real store has the corpus in it, got {}", store.rows());
        store.check().expect("every section serving needs");

        // The mapping is lazy, so reading a column is what proves the offsets address real pages.
        let view = store.view();
        let keys = view.per_row::<u64>("keys").expect("keys");
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys ascend");
        let strings = view.strings().expect("strings");
        let titles = view.per_row::<u32>("card_title").expect("card_title");
        let named = titles.iter().filter(|&&id| strings.get(id).is_some()).count();
        assert!(named > store.rows() / 2, "most rows should resolve a title, got {named}");
    }

    #[test]
    fn maps_and_checks_the_spec_fixture() {
        let Some(path) = fixture() else {
            eprintln!("SKIP: den-spec/vectors/store-v1.store not found");
            return;
        };
        let store = MappedStore::open(&path).expect("the fixture maps");
        assert_eq!(store.dataset_version(), "fixture");
        assert_eq!(store.rows(), 3);
        store.check().expect("the fixture has every section serving needs");
    }

    /// Both ways a file can fail to be a store, and each must say WHICH — "could not load the dataset"
    /// is the message that cost nineteen minutes of quiet degradation the last time a blob went bad.
    #[test]
    fn refuses_a_file_that_is_not_a_store() {
        let dir = std::env::temp_dir().join("atlas-store-test");
        std::fs::create_dir_all(&dir).unwrap();

        let short = dir.join("short.bin");
        std::fs::write(&short, b"too short to be a header").unwrap();
        let err = MappedStore::open(&short).unwrap_err();
        assert!(err.contains("needs at least"), "should name the length, got: {err}");

        // Long enough to reach the magic check, so it fails on identity rather than on size.
        let wrong = dir.join("wrong-magic.bin");
        std::fs::write(&wrong, vec![0u8; 128]).unwrap();
        let err = MappedStore::open(&wrong).unwrap_err();
        assert!(err.contains("magic"), "should name the magic, got: {err}");
    }
}
