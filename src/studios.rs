//! The iconic studios (oxyc/den#132): the production companies a viewer browses by, for a studio link in a
//! title's detail header and a row of the studio's titles.
//!
//! Which studios they are is a hand-kept list in den-dataset (`data/iconic-studios.json`), because no measure
//! over plots or credits finds the curatorial labels (A24, Neon, Searchlight score like Universal). The store
//! carries it as its optional `studio_*` sections (den-spec `wire/store-v2.md`), read through
//! `den_store::Store::iconic_studios`. A store without them has no iconic studios: `/index/studios.json` and a
//! title's studios answer empty, and the `studio` filter kind is not offered.
//!
//! One studio is often several Wikidata items — Toho and Toho Animation, HBO and HBO Films — so a title is the
//! studio's when it credits any of them, and a studio is always named by its own item. Its row is the filter's
//! `studio` kind (`/index/filter/{type}/titles.json?sel=studio:Q…`), which unions those items; `company:Q…`
//! selects the one item it names.

use crate::queries::Indexes;
use den_index::MediaType;
use den_store::{Store, StoreError};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct Studio {
    /// The studio's own Wikidata item, as a raw Q-id number.
    pub qid: u32,
    /// What a viewer calls it, which is not always Wikidata's label.
    pub name: String,
}

impl Studio {
    /// `Q…`: the id its row and page are addressed by.
    pub fn id(&self) -> String {
        format!("Q{}", self.qid)
    }
}

#[derive(Default)]
pub struct Studios {
    /// Sorted by `qid`, as the store keeps them.
    studios: Vec<Studio>,
    /// The entity id of every item credited as a studio, and that studio's index in `studios`.
    by_entity: HashMap<u32, usize>,
}

impl Studios {
    /// The store's iconic studios; empty for a store without the sections, an error for malformed ones.
    pub fn from_store(view: &Store<'_>) -> Result<Studios, StoreError> {
        let table = view.iconic_studios()?;
        let mut out = Studios::default();
        if table.is_empty() {
            return Ok(out);
        }
        let strings = view.strings()?;
        for (i, studio) in table.iter().enumerate() {
            let name = strings.get(studio.name).map_or_else(|| format!("Q{}", studio.qid), str::to_owned);
            out.studios.push(Studio { qid: studio.qid, name });
            for &entity in studio.entities {
                out.by_entity.insert(entity, i);
            }
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.studios.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Studio> {
        self.studios.iter()
    }

    /// A studio by its `Q…` id.
    pub fn get(&self, id: &str) -> Option<&Studio> {
        let qid: u32 = id.strip_prefix('Q')?.parse().ok()?;
        self.studios.binary_search_by_key(&qid, |s| s.qid).ok().map(|i| &self.studios[i])
    }

    /// The studios among a title's credited companies (entity ids), in the order it credits them, each once.
    pub fn credited(&self, companies: &[u32]) -> Vec<&Studio> {
        let mut found: Vec<usize> = Vec::new();
        for i in companies.iter().filter_map(|e| self.by_entity.get(e)) {
            if !found.contains(i) {
                found.push(*i);
            }
        }
        found.into_iter().map(|i| &self.studios[i]).collect()
    }
}

/// `/index/studios.json`: every iconic studio with a title here, with how many films and series it has, most
/// titles first, then by name. What a client draws a shelf of studios from; each one's row is
/// `/index/filter/{type}/titles.json?sel=studio:<id>`.
pub fn list_json(indexes: &Indexes) -> Value {
    let filter = indexes.filter();
    let mut counted: Vec<(&Studio, [usize; 2])> = indexes
        .studios
        .iter()
        .map(|studio| (studio, filter.typed_count("studio", &studio.id())))
        .filter(|(_, [movies, series])| movies + series > 0)
        .collect();
    counted.sort_by(|a, b| (b.1[0] + b.1[1]).cmp(&(a.1[0] + a.1[1])).then(a.0.name.cmp(&b.0.name)));
    let studios: Vec<Value> = counted
        .into_iter()
        .map(|(studio, [movies, series])| {
            json!({ "id": studio.id(), "name": studio.name, "movies": movies, "series": series })
        })
        .collect();
    json!({ "studios": studios })
}

/// `/index/studios/{type}/{tmdbId}.json`: the title's iconic studios, in the order it credits them — what a
/// detail header links. Empty for a title with none, or one the corpus does not hold.
pub fn title_json(indexes: &Indexes, media_type: MediaType, tmdb_id: u32) -> Value {
    let view = indexes.store.view();
    let studios: Vec<Value> = view
        .row_of(u8::from(media_type == MediaType::Tv), tmdb_id)
        .ok()
        .flatten()
        .zip(view.list::<u32>("companies_v", "companies_o").ok())
        .map(|(row, companies)| indexes.studios.credited(companies.get(row)))
        .unwrap_or_default()
        .into_iter()
        .map(|studio| json!({ "id": studio.id(), "name": studio.name }))
        .collect();
    json!({ "studios": studios })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{spec, Context, Request, Route, Scope};
    use crate::store::fixture::{Entity, Studio as FixtureStudio, Title};
    use den_index::MediaType::{Movie, Tv};

    /// The indexes of a store holding these titles, and these studios' sections when there are any.
    fn store_of(name: &str, titles: &[Title<'_>], studios: &[FixtureStudio<'_>]) -> Indexes {
        let dir = std::env::temp_dir().join(format!("den-atlas-studios-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entities = [
            Entity { qid: 61, name: "Iconic Pictures", ..Entity::default() },
            Entity { qid: 62, name: "Iconic Animation", ..Entity::default() },
            Entity { qid: 63, name: "A Distributor", ..Entity::default() },
        ];
        crate::store::fixture::write_with_studios(
            &dir.join("den-v1.store"),
            "v1",
            3,
            titles,
            &entities,
            studios,
        );
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the store loads");
        crate::queries::load_for_tools(&ds).expect("its indexes load")
    }

    /// Movie 1 credits the studio's own item, movie 2 only its animation arm, movie 3 a distributor and a
    /// second studio, series 4 both of the first studio's items. More votes first: 2, 1, 3; the series alone.
    fn titles() -> Vec<Title<'static>> {
        let title = |media, tmdb_id, votes, companies: Vec<u32>| Title {
            media,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(2000))),
            votes,
            companies,
            ..Title::default()
        };
        vec![
            title(0, 1, 100, vec![61]),
            title(0, 2, 500, vec![62]),
            title(0, 3, 50, vec![63, 70]),
            title(1, 4, 300, vec![62, 61]),
            title(0, 5, 10, vec![]),
        ]
    }

    fn studios() -> Vec<FixtureStudio<'static>> {
        vec![
            FixtureStudio { qid: 61, name: "Iconic", items: vec![61, 62] },
            // A studio whose own item is not the one credited: the store names it anyway.
            FixtureStudio { qid: 69, name: "Second Studio", items: vec![70] },
        ]
    }

    fn titles_of(indexes: &Indexes, scope: Scope, query: &str) -> Value {
        let request = Request::parse(Route::Titles, scope, query).expect("a well-formed request");
        Context::new(indexes, scope, None).titles(&request).0
    }

    fn ids(answer: &Value) -> Vec<(String, u64)> {
        answer["titles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| (t["type"].as_str().unwrap().to_owned(), t["id"].as_u64().unwrap()))
            .collect()
    }

    fn pair(media: &str, id: u64) -> (String, u64) {
        (media.to_owned(), id)
    }

    /// A studio's row is every title crediting any of its items, most voted first — where `company:` selects
    /// the one item it names and would miss the titles credited to the studio's other arm.
    #[test]
    fn a_studio_row_is_every_item_it_is_credited_as() {
        let indexes = store_of("row", &titles(), &studios());
        let films = titles_of(&indexes, Scope::Type(Movie), "sel=studio:Q61");
        assert_eq!(ids(&films), [pair("movie", 2), pair("movie", 1)], "{films}");
        assert_eq!(films["total"], 2);
        assert_eq!(ids(&titles_of(&indexes, Scope::Type(Movie), "sel=company:Q61")), [pair("movie", 1)]);

        // Under `all`, the series once though it credits both items.
        let all = titles_of(&indexes, Scope::All, "sel=studio:Q61");
        assert_eq!(all["total"], 3);
        assert_eq!(all["titles"].as_array().unwrap().iter().filter(|t| t["type"] == "series").count(), 1);

        // Paged as every row: `skip` a multiple of `limit`.
        let second = titles_of(&indexes, Scope::Type(Movie), "sel=studio:Q61&skip=1&limit=1");
        assert_eq!(ids(&second), [pair("movie", 1)]);

        // Named by its own item even when only another is credited.
        assert_eq!(ids(&titles_of(&indexes, Scope::Type(Movie), "sel=studio:Q69")), [pair("movie", 3)]);
        // An exclusion keeps the titles crediting some company, and none of the studio's.
        let without = titles_of(&indexes, Scope::Type(Movie), "sel=-studio:Q61");
        assert_eq!(ids(&without), [pair("movie", 3)], "movie 5 credits no company, so it is unknown");
    }

    /// counts.json lists every studio with titles under the selection, named; values/studio.json finds one by
    /// its name.
    #[test]
    fn the_studio_kind_is_counted_and_named() {
        let indexes = store_of("counts", &titles(), &studios());
        let context = Context::new(&indexes, Scope::Type(Movie), None);
        let counts = context.counts(&Request::parse(Route::Counts, Scope::Type(Movie), "").unwrap()).0;
        let studio = &counts["kinds"]["studio"];
        assert_eq!(studio["values"], json!({ "Q61": 2, "Q69": 1 }));
        assert_eq!(studio["labels"], json!({ "Q61": "Iconic", "Q69": "Second Studio" }));
        assert_eq!(studio["complete"], true);

        let spec = spec("studio").unwrap();
        let found = context
            .values(spec, &Request::parse(Route::Values(spec), Scope::Type(Movie), "q=seco").unwrap())
            .0;
        assert_eq!(found["values"], json!([{ "id": "Q69", "name": "Second Studio", "count": 1 }]));
    }

    /// The list: most titles first, films and series apart. A title's studios: in credit order, each once,
    /// never an item that is no studio's.
    #[test]
    fn the_list_and_a_title_s_studios() {
        let indexes = store_of("list", &titles(), &studios());
        assert_eq!(
            list_json(&indexes),
            json!({ "studios": [
                { "id": "Q61", "name": "Iconic", "movies": 2, "series": 1 },
                { "id": "Q69", "name": "Second Studio", "movies": 1, "series": 0 },
            ] })
        );
        let named = json!({ "studios": [{ "id": "Q61", "name": "Iconic" }] });
        assert_eq!(title_json(&indexes, Tv, 4), named, "two of its items, one studio");
        assert_eq!(title_json(&indexes, Movie, 2), named, "the animation arm is the studio");
        assert_eq!(
            title_json(&indexes, Movie, 3),
            json!({ "studios": [{ "id": "Q69", "name": "Second Studio" }] })
        );
        assert_eq!(title_json(&indexes, Movie, 5), json!({ "studios": [] }));
        assert_eq!(title_json(&indexes, Movie, 999), json!({ "studios": [] }), "not in the corpus");
    }

    /// A store written before the studio sections loads and serves; the feature is simply absent: no `studio`
    /// kind offered (a selection naming it is ignored, not an empty row), and both studio answers empty.
    #[test]
    fn a_store_without_the_studio_sections_serves_without_them() {
        let indexes = store_of("absent", &titles(), &[]);
        assert!(indexes.studios.is_empty());
        let context = Context::new(&indexes, Scope::Type(Movie), None);
        let counts = context.counts(&Request::parse(Route::Counts, Scope::Type(Movie), "").unwrap()).0;
        assert!(counts["kinds"].get("studio").is_none(), "not offered");
        assert!(counts.get("kindsUnavailable").is_none(), "absent is not a failure");
        let row = titles_of(&indexes, Scope::Type(Movie), "sel=studio:Q61");
        assert_eq!(row["ignored"], json!(["studio"]));
        assert_eq!(row["total"], 4, "answered around the kind it does not have");
        assert_eq!(list_json(&indexes), json!({ "studios": [] }));
        assert_eq!(title_json(&indexes, Tv, 4), json!({ "studios": [] }));
        // The companies are still there to select one by one.
        assert_eq!(titles_of(&indexes, Scope::Type(Movie), "sel=company:Q62")["total"], 1);
    }

    /// The studio rows on the REAL corpus, for judging their order. Opt-in, as `filter.rs`'s real-corpus test:
    /// `DEN_STORE` names a store whose directory holds its `dataset.meta.json`; `CACHE_DIR`, a directory of
    /// kept TMDB numbers (`tmdb-votes.tsv`), orders by TMDB's vote counts as production does, and without it
    /// the store's own `votes` column orders.
    #[test]
    fn real_corpus_studio_rows() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to see this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let tmdb = std::env::var("CACHE_DIR").ok().map(|kept| {
            let tmdb = crate::tmdb::Tmdb::new(ds.mapped.clone(), Some(kept.into()), None, 0).unwrap();
            eprintln!("{}", runtime.block_on(tmdb.load()));
            tmdb
        });
        let (indexes, _) = runtime
            .block_on(
                crate::queries::IndexQueries::new(&ds)
                    .with_ratings(tmdb.as_ref().map(|t| t.ratings()))
                    .get(|| ()),
            )
            .expect("the indexes load");
        let list = list_json(&indexes);
        let studios = list["studios"].as_array().unwrap();
        eprintln!("{} studios with titles", studios.len());
        for studio in studios {
            eprintln!(
                "  {} {} — {} films, {} series",
                studio["id"], studio["name"], studio["movies"], studio["series"]
            );
        }
        for name in ["Studio Ghibli", "Pixar", "A24"] {
            let id = indexes.studios.iter().find(|s| s.name == name).map(Studio::id).expect(name);
            let row = titles_of(&indexes, Scope::All, &format!("sel=studio:{id}&limit=10"));
            eprintln!("{name} ({id}), {} titles:", row["total"]);
            for (i, t) in row["titles"].as_array().unwrap().iter().enumerate() {
                eprintln!("  {:>2}. {} ({}) {}", i + 1, t["title"].as_str().unwrap(), t["year"], t["type"]);
            }
        }
        assert!(!studios.is_empty(), "a store from den-dataset#92 on carries the studios");
    }
}
