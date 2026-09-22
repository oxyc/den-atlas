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
    /// `agg` so the rail's own constructor can be the thing that is checked — see below.
    pub fn check(&self, agg: &den_index::RailAggregates) -> Result<(), StoreError> {
        let view = self.view();
        view.strings()?;
        // Everything the rail's facets read, checked by BUILDING the rail's view of the store rather than
        // by a second list of section names. The first version of this function listed `makers`, `cast`
        // and `genres` and none of the nouls; `SeedFacets::nouls` swallowed a read error and returned empty, so a
        // store missing them dropped the W_NOUL = 1.60 term on every request with no log line and no
        // health signal. Replacing the list with the constructor is what stops that recurring: there is
        // now no way to add a column the rail needs and forget to require it here.
        //
        // Authorship (`SeedAuthorship`, the credit lists) is not required: the rail ranks without it, as it
        // always has when the credits would not read, and the facts' own load reports their absence.
        //
        // The media type is immaterial — it only packs the search key — so movie stands for both.
        den_index::SeedFacets::new(&view, agg, den_index::MediaType::Movie)?;
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
    pub aggregates: den_index::RailAggregates,
}

impl LoadedStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let store = MappedStore::open(path)?;
        // The aggregates first: `check` proves the rail can be built, and the rail borrows them.
        let aggregates = den_index::RailAggregates::build(&store.view())?;
        store.check(&aggregates).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { store, aggregates })
    }

    pub fn view(&self) -> Store<'_> {
        self.store.view()
    }
}

/// den-spec's three-title store fixture, for every test in this crate that reads a real store.
///
/// `None` means SKIP, and it is returned in exactly one case: `DEN_SPEC_OPTIONAL=1`, set on purpose by
/// someone who knows their checkout has no den-spec. Absent without it, this panics.
///
/// It used to be two copies of a `path.is_file().then_some(path)`, both resolving den-spec as
/// `../../den-spec` — one level too far up from `den-atlas/`, so it landed on `~/Projects/den-spec`,
/// which does not exist. Every test built on the fixture returned before its first assertion and
/// reported a pass. A contract test that cannot find its contract has verified nothing, and the only
/// way a test can say so is to fail.
#[cfg(test)]
pub(crate) fn spec_fixture() -> Option<std::path::PathBuf> {
    let path = std::env::var("DEN_SPEC_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../den-spec"))
        .join("vectors/store-v1.store");
    if path.is_file() {
        return Some(path);
    }
    // `== "1"`, not `is_ok()`. Any-value-skips means `DEN_SPEC_OPTIONAL=0`, set by someone turning
    // skipping OFF, silently turns it on — the exact failure this helper exists to stop.
    if std::env::var("DEN_SPEC_OPTIONAL").as_deref() == Ok("1") {
        eprintln!("SKIP: den-spec absent and DEN_SPEC_OPTIONAL=1");
        return None;
    }
    panic!(
        "{} not found — these tests check den-atlas against the store-v1 contract and cannot do so \
         without it. Check out den-spec beside this repo, set DEN_SPEC_DIR, or set DEN_SPEC_OPTIONAL=1 \
         to skip deliberately.",
        path.display()
    )
}

/// Writes a store-v1 file from test data.
///
/// The store is the only artifact the serving path reads, so every test that needs a dataset needs one
/// written — the route fixture's twelve titles, and the one-row stores `dataset.rs` hangs its manifest
/// tests on. den-spec's `vectors/store-v1.store` cannot stand in: it describes three titles of its own,
/// and rewriting the route tests around them would throw away a fixture built to exercise this server
/// (twelve rows so search's z-score floor has a distribution, four drawable cards, a label with a slash).
///
/// This is a SECOND implementation of the layout — den-dataset's `build_store.py` is the real writer —
/// and den-spec argues against exactly that, because a fixture produced by a second copy only proves the
/// copy agrees with itself. What keeps it honest is that it is checked by the reader we ship: every store
/// it writes goes through `den_store::Store::open`, which verifies the magic, the version, the endianness
/// marker, the content hash over every byte and each section's extent, and then through `MappedStore::check`.
/// A layout mistake here fails the tests rather than passing them. The store-v1 CONTRACT is still tested
/// against the real writer's output, by `maps_and_checks_the_spec_fixture` below.
#[cfg(test)]
pub(crate) mod fixture {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;
    use std::collections::HashMap;

    const HEADER: usize = 64;
    const ENTRY: usize = 32;
    /// Every section is laid out on an 8-byte boundary — the widest element in the format — because
    /// `zerocopy` refuses a misaligned slice, and a mapping starts on a page.
    const ALIGN: usize = 8;

    /// One title, as the store holds it. `Default` so a test names only what it cares about.
    #[derive(Default, Clone)]
    pub(crate) struct Title<'a> {
        /// 0 = movie, 1 = tv. Rows are written in packed-key order whatever order these arrive in.
        pub media: u8,
        pub tmdb_id: u32,
        pub primary_genre: &'a str,
        pub animated: bool,
        /// (label, confidence in hundredths).
        pub subgenres: Vec<(&'a str, u8)>,
        pub moods: Vec<(&'a str, u8)>,
        /// The int8 vector in each space; empty means the row has none (`vec_*_has` = 0).
        pub plot: Vec<i8>,
        pub premise: Vec<i8>,
        /// (axis, value, confidence in hundredths). An axis left out is stored absent.
        pub facets: Vec<(&'a str, &'a str, u8)>,
        /// (title, poster path, year). Absent ⇒ no card, which is how a row with no drawable name is held.
        pub card: Option<(&'a str, Option<&'a str>, Option<i16>)>,
        pub votes: u32,
        pub imdb: Option<&'a str>,
        /// (days since 1970-01-01, precision: 0 day / 1 month / 2 year).
        pub released: Option<(i32, u8)>,
        pub runtime: u16,
        /// TMDB genre ids, as the store keeps them — folded by the reader, not here.
        pub genres: Vec<u32>,
        pub countries: Vec<&'a str>,
        pub languages: Vec<&'a str>,
        /// Q-ids. The store interns these into the entity table; this resolves them on the way in.
        pub makers: Vec<u32>,
        pub cast: Vec<u32>,
        pub broadcasters: Vec<u32>,
        pub based_kind: Vec<&'a str>,
        pub alias_titles: Vec<&'a str>,
        pub franchise: Option<u32>,
    }

    /// One row of the entity table: a person, a franchise, a place.
    #[derive(Clone)]
    pub(crate) struct Entity<'a> {
        pub qid: u32,
        pub name: &'a str,
        pub tmdb: Option<u32>,
        pub aliases: Vec<&'a str>,
    }

    /// The string dictionary and the section table, built up section by section.
    struct Builder {
        strings: Vec<String>,
        ids: HashMap<String, u32>,
        sections: Vec<(&'static str, u32, Vec<u8>)>,
        /// Sections to leave out, so a test can write the store a LATER producer writes. Omission is
        /// the only way to cover a reader's tolerance of a dropped section, and a fixture that always
        /// writes everything quietly stops describing the artifact when the producer drops one.
        omit: &'static [&'static str],
    }

    impl Builder {
        fn intern(&mut self, text: &str) -> u32 {
            if let Some(&id) = self.ids.get(text) {
                return id;
            }
            let id = u32::try_from(self.strings.len()).expect("fixture string table");
            self.strings.push(text.to_owned());
            self.ids.insert(text.to_owned(), id);
            id
        }

        /// An optional string: absent is the sentinel, never an interned empty string, because
        /// `Strings::get` answers `None` for the sentinel and `Some("")` for the other.
        fn optional(&mut self, text: Option<&str>) -> u32 {
            text.map_or(den_store::NONE_U32, |text| self.intern(text))
        }

        fn section(&mut self, name: &'static str, width: u32, bytes: Vec<u8>) {
            if self.omit.contains(&name) {
                return;
            }
            self.sections.push((name, width, bytes));
        }

        fn u8s(&mut self, name: &'static str, values: &[u8]) {
            self.section(name, 1, values.to_vec());
        }

        fn u32s(&mut self, name: &'static str, values: &[u32]) {
            self.section(name, 4, values.iter().flat_map(|v| v.to_le_bytes()).collect());
        }

        fn u64s(&mut self, name: &'static str, values: &[u64]) {
            self.section(name, 8, values.iter().flat_map(|v| v.to_le_bytes()).collect());
        }

        /// The offsets half of a list on its own, for a pair whose values are `u8`.
        fn offsets(&mut self, name: &'static str, lengths: impl Iterator<Item = usize>) {
            let mut offsets: Vec<u32> = vec![0];
            for length in lengths {
                let last = *offsets.last().expect("seeded");
                offsets.push(last + u32::try_from(length).expect("fixture list"));
            }
            self.u32s(name, &offsets);
        }

        /// A `values`/`offsets` pair of u32 ids, one span per row.
        fn list(&mut self, values_name: &'static str, offsets_name: &'static str, rows: &[Vec<u32>]) {
            self.u32s(values_name, &rows.concat());
            self.offsets(offsets_name, rows.iter().map(Vec::len));
        }
    }

    /// Write `titles` (and the entities they credit) to `path` as a store-v1 file.
    ///
    /// Panics on anything a fixture can simply get right — a vector of the wrong width, a title count that
    /// does not fit a `u32`. A test fixture that limped on would be describing something other than the
    /// corpus the test thinks it wrote.
    pub(crate) fn write(
        path: &std::path::Path,
        dataset_version: &str,
        dim: usize,
        titles: &[Title<'_>],
        entities: &[Entity<'_>],
    ) {
        write_omitting(path, dataset_version, dim, titles, entities, &[]);
    }

    /// `write`, without the named sections — for a reader that must tolerate one being dropped.
    pub(crate) fn write_omitting(
        path: &std::path::Path,
        dataset_version: &str,
        dim: usize,
        titles: &[Title<'_>],
        entities: &[Entity<'_>],
        omit: &'static [&'static str],
    ) {
        let mut titles = titles.to_vec();
        titles.sort_by_key(|t| (u64::from(t.media) << 32) | u64::from(t.tmdb_id));
        let rows = titles.len();

        // The entity table: the entities given, then every Q-id a title credits that they do not name —
        // held by its Q-id, exactly as the real writer holds one it has no description for.
        let mut table: Vec<Entity<'_>> = entities.to_vec();
        let mut named: Vec<u32> = table.iter().map(|e| e.qid).collect();
        let mut extra: Vec<String> = Vec::new();
        for title in &titles {
            for &qid in title.makers.iter().chain(&title.cast).chain(&title.broadcasters) {
                if !named.contains(&qid) {
                    named.push(qid);
                    extra.push(format!("Q{qid}"));
                }
            }
        }
        for (qid, name) in named[table.len()..].iter().zip(&extra) {
            table.push(Entity { qid: *qid, name, tmdb: None, aliases: Vec::new() });
        }
        let at = |qid: u32| -> u32 {
            u32::try_from(named.iter().position(|&q| q == qid).expect("entity")).expect("entity index")
        };

        let mut b = Builder { strings: Vec::new(), ids: HashMap::new(), sections: Vec::new(), omit };

        let keys: Vec<u64> =
            titles.iter().map(|t| (u64::from(t.media) << 32) | u64::from(t.tmdb_id)).collect();
        b.u64s("keys", &keys);

        // Labels.
        let genres: Vec<u32> = titles
            .iter()
            .map(|t| b.optional((!t.primary_genre.is_empty()).then_some(t.primary_genre)))
            .collect();
        b.u32s("primary_genre", &genres);
        b.u8s("animated", &titles.iter().map(|t| u8::from(t.animated)).collect::<Vec<u8>>());
        for (values, confidences, offsets, pick) in
            [("subgenre_v", "subgenre_c", "subgenre_o", true), ("mood_v", "mood_c", "mood_o", false)]
        {
            let rows_of: Vec<Vec<(u32, u8)>> = titles
                .iter()
                .map(|t| {
                    let labels = if pick { &t.subgenres } else { &t.moods };
                    labels.iter().map(|&(label, c)| (b.intern(label), c)).collect()
                })
                .collect();
            // Values and confidences share one offsets array, so row i owns the same span in both.
            let ids: Vec<Vec<u32>> =
                rows_of.iter().map(|row| row.iter().map(|&(id, _)| id).collect()).collect();
            b.list(values, offsets, &ids);
            b.u8s(
                confidences,
                &rows_of.iter().flat_map(|row| row.iter().map(|&(_, c)| c)).collect::<Vec<u8>>(),
            );
        }

        // The two vector matrices. A row with no vector is written as zeros with its `_has` byte clear,
        // which is what the real writer does and what `from_store_space` drops.
        for (name, has_name, pick) in
            [("vec_plot", "vec_plot_has", true), ("vec_premise", "vec_premise_has", false)]
        {
            let mut values: Vec<u8> = Vec::with_capacity(rows * dim);
            let mut has: Vec<u8> = Vec::with_capacity(rows);
            for title in &titles {
                let vector = if pick { &title.plot } else { &title.premise };
                assert!(vector.is_empty() || vector.len() == dim, "fixture vector is not {dim} wide");
                values.extend(vector.iter().map(|&v| v as u8));
                values.resize(values.len() + dim - vector.len(), 0);
                has.push(u8::from(!vector.is_empty()));
            }
            b.u8s(name, &values);
            b.u8s(has_name, &has);
        }

        // The twelve facet axes, in the order `facet_v`/`facet_c` store them.
        let mut facet_v: Vec<u32> = Vec::with_capacity(rows * den_store::FACET_AXES.len());
        let mut facet_c: Vec<u8> = Vec::with_capacity(rows * den_store::FACET_AXES.len());
        for title in &titles {
            for axis in den_store::FACET_AXES {
                match title.facets.iter().find(|(name, _, _)| *name == axis) {
                    Some(&(_, value, confidence)) => {
                        let id = b.intern(value);
                        facet_v.push(id);
                        facet_c.push(confidence);
                    }
                    None => {
                        facet_v.push(den_store::NONE_U32);
                        facet_c.push(0);
                    }
                }
            }
        }
        b.u32s("facet_v", &facet_v);
        b.u8s("facet_c", &facet_c);

        // Cards and votes.
        let card_title: Vec<u32> =
            titles.iter().map(|t| b.optional(t.card.map(|(title, _, _)| title))).collect();
        let card_poster: Vec<u32> =
            titles.iter().map(|t| b.optional(t.card.and_then(|(_, poster, _)| poster))).collect();
        b.u32s("card_title", &card_title);
        b.u32s("card_poster", &card_poster);
        b.section(
            "card_year",
            2,
            titles
                .iter()
                .flat_map(|t| t.card.and_then(|(_, _, y)| y).unwrap_or(den_store::NONE_I16).to_le_bytes())
                .collect(),
        );
        b.u32s("votes", &titles.iter().map(|t| t.votes).collect::<Vec<u32>>());

        // The facts.
        let imdb: Vec<u32> = titles.iter().map(|t| b.optional(t.imdb)).collect();
        b.u32s("imdb", &imdb);
        b.section(
            "released",
            4,
            titles.iter().flat_map(|t| t.released.map_or(i32::MIN, |(d, _)| d).to_le_bytes()).collect(),
        );
        b.u8s("released_prec", &titles.iter().map(|t| t.released.map_or(0, |(_, p)| p)).collect::<Vec<u8>>());
        b.section("runtime", 2, titles.iter().flat_map(|t| t.runtime.to_le_bytes()).collect());
        b.u32s(
            "franchise",
            &titles.iter().map(|t| t.franchise.unwrap_or(den_store::NONE_U32)).collect::<Vec<u32>>(),
        );
        let genre_rows: Vec<Vec<u32>> = titles.iter().map(|t| t.genres.clone()).collect();
        b.list("genres_v", "genres_o", &genre_rows);
        for (values, offsets, pick) in [
            ("countries_v", "countries_o", 0),
            ("languages_v", "languages_o", 1),
            ("based_kind_v", "based_kind_o", 2),
            ("alias_titles_v", "alias_titles_o", 3),
        ] {
            let rows_of: Vec<Vec<u32>> = titles
                .iter()
                .map(|t| {
                    let texts = match pick {
                        0 => &t.countries,
                        1 => &t.languages,
                        2 => &t.based_kind,
                        _ => &t.alias_titles,
                    };
                    texts.iter().map(|text| b.intern(text)).collect()
                })
                .collect();
            b.list(values, offsets, &rows_of);
        }
        for (values, offsets, pick) in
            [("makers_v", "makers_o", 0), ("cast_v", "cast_o", 1), ("broadcasters_v", "broadcasters_o", 2)]
        {
            let rows_of: Vec<Vec<u32>> = titles
                .iter()
                .map(|t| {
                    let qids = match pick {
                        0 => &t.makers,
                        1 => &t.cast,
                        _ => &t.broadcasters,
                    };
                    qids.iter().map(|&qid| at(qid)).collect()
                })
                .collect();
            b.list(values, offsets, &rows_of);
        }

        // The entity table. Its lists are keyed by entity, not by title row.
        b.u32s("ent_qid", &table.iter().map(|e| e.qid).collect::<Vec<u32>>());
        let ent_name: Vec<u32> = table.iter().map(|e| b.intern(e.name)).collect();
        b.u32s("ent_name", &ent_name);
        b.u32s(
            "ent_tmdb",
            &table.iter().map(|e| e.tmdb.unwrap_or(den_store::NONE_U32)).collect::<Vec<u32>>(),
        );
        let aliases: Vec<Vec<u32>> =
            table.iter().map(|e| e.aliases.iter().map(|a| b.intern(a)).collect()).collect();
        b.list("ent_alias_v", "ent_alias_o", &aliases);

        // The rail's own columns. The fixture declares no critique axis and no noul, so the pooled scorer
        // reads the facets and the authorship alone — the terms these route tests are about.
        b.u32s("critique_names", &[]);
        b.u8s("critique", &[]);
        b.u8s("world", &vec![0u8; rows]);
        b.u32s("noul_names", &[]);
        b.u8s("noul_k_v", &[]);
        b.offsets("noul_k_o", std::iter::repeat_n(0, rows));
        b.u8s("noul_v_v", &[]);
        b.offsets("noul_v_o", std::iter::repeat_n(0, rows));

        // The string dictionary last, so every `intern` above is in it.
        let mut blob: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = vec![0];
        for text in &b.strings {
            blob.extend_from_slice(text.as_bytes());
            offsets.push(u32::try_from(blob.len()).expect("fixture string blob"));
        }
        b.section("strings", 1, blob);
        b.u32s("str_off", &offsets);

        std::fs::write(path, assemble(dataset_version, rows, &b.sections)).expect("write the fixture store");
    }

    /// Header, section table, then the sections — each on an 8-byte boundary — and the content hash over
    /// everything past the header.
    fn assemble(dataset_version: &str, rows: usize, sections: &[(&'static str, u32, Vec<u8>)]) -> Vec<u8> {
        let table_end = HEADER + sections.len() * ENTRY;
        let mut body_at = table_end.next_multiple_of(ALIGN);
        let mut out = vec![0u8; body_at];
        let mut table = Vec::with_capacity(sections.len() * ENTRY);
        for (name, width, bytes) in sections {
            let mut entry = [0u8; ENTRY];
            let name = name.as_bytes();
            assert!(name.len() <= 16, "section name {name:?} does not fit");
            entry[..name.len()].copy_from_slice(name);
            entry[16..24].copy_from_slice(&(body_at as u64).to_le_bytes());
            entry[24..28].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            entry[28..32].copy_from_slice(&width.to_le_bytes());
            table.extend_from_slice(&entry);
            out.extend_from_slice(bytes);
            body_at = out.len().next_multiple_of(ALIGN);
            out.resize(body_at, 0);
        }
        out[HEADER..table_end].copy_from_slice(&table);

        out[..8].copy_from_slice(b"DENSTOR1");
        out[8..12].copy_from_slice(&1u32.to_le_bytes());
        out[12..16].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        out[24..28].copy_from_slice(&(sections.len() as u32).to_le_bytes());
        out[28..32].copy_from_slice(&(rows as u32).to_le_bytes());
        let version = dataset_version.as_bytes();
        assert!(version.len() <= 16, "datasetVersion does not fit the header");
        out[32..32 + version.len()].copy_from_slice(version);
        let digest = blake2b64(&out[HEADER..]);
        out[16..24].copy_from_slice(&digest.to_le_bytes());
        out
    }

    /// The writer's `hashlib.blake2b(payload, digest_size=8)`, read little-endian.
    fn blake2b64(body: &[u8]) -> u64 {
        let mut hasher = Blake2bVar::new(8).expect("8-byte blake2b");
        hasher.update(body);
        let mut digest = [0u8; 8];
        hasher.finalize_variable(&mut digest).expect("digest");
        u64::from_le_bytes(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::spec_fixture as fixture;

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
        let agg = den_index::RailAggregates::build(&store.view()).expect("aggregates");
        store.check(&agg).expect("every section serving needs");

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
        let agg = den_index::RailAggregates::build(&store.view()).expect("aggregates");
        store.check(&agg).expect("the fixture has every section serving needs");
    }

    // The rail's column reads (`den_index::SeedFacets`), against den-spec's three-title fixture. These
    // exist because the first version of the rail had none, and that is exactly what let three thresholds
    // quietly change the ranking: the store still loaded, every row still returned twelve facets, and the
    // numbers were simply different. They sit here, beside the loader, because this is where the fixture
    // is found; `spec_fixture` fails rather than returns `None` when den-spec is absent, so they cannot go
    // back to reporting a pass over a store they never opened.

    fn rail_fixture() -> Option<LoadedStore> {
        fixture().map(|path| LoadedStore::open(&path).expect("the fixture loads"))
    }

    fn seed(loaded: &LoadedStore, media: den_index::MediaType) -> den_index::SeedFacets<'_> {
        den_index::SeedFacets::new(&loaded.view(), &loaded.aggregates, media)
            .expect("the fixture carries every column the rail reads")
    }

    fn axis(name: &str) -> den_index::Axis {
        den_store::FACET_AXES.iter().position(|a| *a == name).unwrap() as den_index::Axis
    }

    /// The fixture's `movie:1` answers `era` and `tone`, and DECLINES `pacing`. A declined axis must be
    /// absent, not a value: a scorer that counted `does-not-apply` as agreement would pair every
    /// declining title with every other.
    #[test]
    fn facets_carry_confidence_and_declines_are_absent() {
        use den_index::Facets as _;
        let Some(loaded) = rail_fixture() else { return };
        let facets = seed(&loaded, den_index::MediaType::Movie).facets(1);
        let (_, _, conf) = facets.iter().find(|(a, _, _)| *a == axis("era")).expect("era is answered");
        assert!((conf - 0.96).abs() < 1e-9, "confidence is hundredths, got {conf}");
        assert!(!facets.iter().any(|(a, _, _)| *a == axis("pacing")), "a declined axis must not appear");
    }

    /// Prevalence is per media type and per axis. With one movie answering `era`, that value's
    /// prevalence among movies is 1 of 2 movie rows.
    #[test]
    fn prevalence_is_scoped_to_one_media_type() {
        use den_index::Facets as _;
        let Some(loaded) = rail_fixture() else { return };
        let movies = seed(&loaded, den_index::MediaType::Movie);
        let era = axis("era");
        let (_, value, _) = movies.facets(1).into_iter().find(|(a, _, _)| *a == era).unwrap();

        assert!((movies.prevalence(era, value) - 0.5).abs() < 1e-9, "1 of 2 movie rows");
        // The same value id, asked of the other media type, must not read the movie statistic.
        assert!((seed(&loaded, den_index::MediaType::Tv).prevalence(era, value) - 1.0).abs() < 1e-9);
    }

    /// The floors the rail re-applies. `movie:1` holds `theme__vampire` at 0.25 (kept) and the fixture
    /// gives it a `world` of 0.25 from that; a value below the world floor must read as 0.
    #[test]
    fn nouls_and_world_are_floored() {
        use den_index::Facets as _;
        let Some(loaded) = rail_fixture() else { return };
        let movies = seed(&loaded, den_index::MediaType::Movie);

        let floors = den_index::SimilarParams::default();
        assert!(movies.nouls(1).iter().all(|(_, p)| *p >= floors.noul_floor), "every noul clears the floor");
        let world = movies.world(1);
        assert!(world == 0.0 || world >= floors.world_floor, "world is floored, got {world}");
        // A row with nothing at all reads as zero distance, not as a missing value.
        assert_eq!(movies.world(2), 0.0);
    }

    /// "Unknown is not none." A row with no critique must return NOTHING, so the cosine term is skipped
    /// — not a negated mean vector that scores a real, usually negative, similarity.
    #[test]
    fn a_row_without_critique_returns_nothing() {
        use den_index::Facets as _;
        let Some(loaded) = rail_fixture() else { return };
        let movies = seed(&loaded, den_index::MediaType::Movie);

        assert!(!movies.critique(1).is_empty(), "movie:1 argues about something");
        assert!(movies.critique_raw(2).is_empty(), "movie:2 has no critique at all");
        assert!(movies.critique(2).is_empty(), "and centering must not manufacture seventeen values for it");
    }

    /// Centering subtracts the per-media mean, so a title above the corpus on an axis reads positive.
    #[test]
    fn critique_is_centered_on_its_own_media_type() {
        use den_index::Facets as _;
        let Some(loaded) = rail_fixture() else { return };
        let movies = seed(&loaded, den_index::MediaType::Movie);
        let raw = movies.critique_raw(1);
        let centered = movies.critique(1);

        assert_eq!(raw.len(), centered.len());
        for ((name, r), (name2, c)) in raw.iter().zip(&centered) {
            assert_eq!(name, name2, "centering preserves order");
            assert!(c <= r, "centering subtracts a non-negative mean: {r} -> {c}");
        }
        assert!(centered.iter().any(|(_, c)| *c > 0.0), "something must be above its own mean");
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
