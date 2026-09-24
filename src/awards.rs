//! Awards (oxyc/den#135): the ceremonies a title won or was nominated at — the Academy Awards, the Cannes Film
//! Festival — for a title's detail and the filter's `award` and `won` kinds.
//!
//! The store carries them as its optional `award_*` and `ceremony_*` sections (den-spec `wire/store-v2.md`
//! "Awards"), read through `den_store::Store::awards`: per title, one entry per ceremony, won or nominated
//! only, and the categories themselves are not stored. They are Wikidata's P166 and P1411, each award filed
//! under its ceremony by den-dataset. A store without them has no awards: `/index/awards.json` answers empty, a
//! title carries none, and the two kinds are not offered.

use crate::queries::Indexes;
use den_index::MediaType;
use den_store::{Row, Store, StoreError};
use serde_json::{json, Value};

pub struct Ceremony {
    /// The ceremony's Wikidata item, as a raw Q-id number.
    pub qid: u32,
    /// Its English label, or `Q…` when it has none.
    pub name: String,
}

impl Ceremony {
    /// `Q…`: the id the filter kinds name it by.
    pub fn id(&self) -> String {
        format!("Q{}", self.qid)
    }
}

/// The ceremony table, in the store's order: sorted by Q-id, the index a title's awards point at.
#[derive(Default)]
pub struct Ceremonies {
    ceremonies: Vec<Ceremony>,
}

impl Ceremonies {
    /// The store's ceremonies; empty for a store without the award sections, an error for malformed ones.
    pub fn from_store(view: &Store<'_>) -> Result<Ceremonies, StoreError> {
        let table = view.awards()?;
        let strings = view.strings()?;
        let ceremonies = (0..table.len() as u32)
            .filter_map(|i| table.ceremony(i))
            .map(|c| Ceremony {
                qid: c.qid,
                name: strings.get(c.name).map_or_else(|| format!("Q{}", c.qid), str::to_owned),
            })
            .collect();
        Ok(Ceremonies { ceremonies })
    }

    pub fn is_empty(&self) -> bool {
        self.ceremonies.is_empty()
    }

    /// Ceremony *i* of the table, as a title's awards index it.
    pub fn at(&self, i: u32) -> Option<&Ceremony> {
        self.ceremonies.get(i as usize)
    }

    /// A ceremony by its `Q…` id.
    pub fn get(&self, id: &str) -> Option<&Ceremony> {
        let qid: u32 = id.strip_prefix('Q')?.parse().ok()?;
        self.ceremonies.binary_search_by_key(&qid, |c| c.qid).ok().map(|i| &self.ceremonies[i])
    }
}

/// `/index/awards.json`: every ceremony a title here was recognised at, with how many films and series it
/// recognised and how many of them won there, most titles first, then by name. Each one's row is
/// `/index/filter/{type}/titles.json?sel=award:<id>` (or `won:<id>`).
pub fn list_json(indexes: &Indexes) -> Value {
    let filter = indexes.filter();
    let mut counted: Vec<(&Ceremony, [usize; 2], [usize; 2])> = indexes
        .ceremonies
        .ceremonies
        .iter()
        .map(|c| (c, filter.typed_count("award", &c.id()), filter.typed_count("won", &c.id())))
        .filter(|(_, [movies, series], _)| movies + series > 0)
        .collect();
    counted.sort_by(|a, b| (b.1[0] + b.1[1]).cmp(&(a.1[0] + a.1[1])).then(a.0.name.cmp(&b.0.name)));
    let ceremonies: Vec<Value> = counted
        .into_iter()
        .map(|(c, [movies, series], [won_movies, won_series])| {
            json!({ "id": c.id(), "name": c.name, "movies": movies, "series": series,
                    "won": { "movies": won_movies, "series": won_series } })
        })
        .collect();
    json!({ "ceremonies": ceremonies })
}

/// A title's awards, `[{id, name, won}]` in ceremony-table order; empty for a title with none, or not in the
/// corpus, or a store without the sections.
pub fn of_title(indexes: &Indexes, media_type: MediaType, tmdb_id: u32) -> Vec<Value> {
    let view = indexes.store.view();
    let (Ok(Some(row)), Ok(awards)) =
        (view.row_of(u8::from(media_type == MediaType::Tv), tmdb_id), view.awards())
    else {
        return Vec::new();
    };
    awards_of(indexes, &awards, row)
}

fn awards_of(indexes: &Indexes, awards: &den_store::Awards<'_>, row: Row) -> Vec<Value> {
    awards
        .get(row)
        .filter_map(|a| {
            let c = indexes.ceremonies.at(a.ceremony)?;
            Some(json!({ "id": c.id(), "name": c.name, "won": a.won }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{spec, Context, Request, Route, Scope};
    use crate::store::fixture::{Entity, Title};
    use den_index::MediaType::Movie;

    const OSCARS: u32 = 19_020;
    const CANNES: u32 = 42_369;
    const BERLIN: u32 = 130_871;

    /// Film 1 won an Oscar and was nominated at Cannes; film 2 was nominated for an Oscar; film 3 won at
    /// Berlin, a ceremony with no entity entry; film 4 has no award on record.
    fn store(name: &str, awards: bool) -> Indexes {
        let recognised =
            [vec![(OSCARS, true), (CANNES, false)], vec![(OSCARS, false)], vec![(BERLIN, true)], vec![]];
        let titles: Vec<Title> = recognised
            .iter()
            .enumerate()
            .map(|(i, list)| Title {
                media: 0,
                tmdb_id: i as u32 + 1,
                primary_genre: "Drama",
                plot: vec![100, 0, 0],
                premise: vec![100, 0, 0],
                card: Some(("A title", None, Some(2000))),
                votes: 100 - i as u32,
                awards: if awards { list.clone() } else { vec![] },
                ..Title::default()
            })
            .collect();
        let entities = [
            Entity { qid: OSCARS, name: "Academy Awards", ..Entity::default() },
            Entity { qid: CANNES, name: "Cannes Film Festival", ..Entity::default() },
        ];
        let dir = std::env::temp_dir().join(format!("den-atlas-awards-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &titles, &entities);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the store loads");
        crate::queries::load_for_tools(&ds).expect("its indexes load")
    }

    fn titles(indexes: &Indexes, query: &str) -> (Vec<u64>, Value) {
        let request = Request::parse(Route::Titles, Scope::Type(Movie), query).unwrap();
        let answer = Context::new(indexes, Movie, None).titles(&request).0;
        let mut ids: Vec<u64> =
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect();
        ids.sort_unstable();
        (ids, answer)
    }

    #[test]
    fn award_is_recognised_at_a_ceremony_and_won_is_winning_there() {
        let indexes = store("kinds", true);
        assert_eq!(titles(&indexes, "sel=award:Q19020").0, [1, 2], "won or nominated");
        assert_eq!(titles(&indexes, "sel=won:Q19020").0, [1]);
        assert_eq!(titles(&indexes, "sel=won:Q19020|Q130871").0, [1, 3], "won at either");
        assert_eq!(titles(&indexes, "sel=award:Q19020,award:Q42369").0, [1], "recognised at both");
        assert_eq!(
            titles(&indexes, "sel=-won:Q19020").0,
            [2, 3],
            "a title with no award on record is not known to have lost"
        );
        let request = Request::parse(Route::Counts, Scope::Type(Movie), "").unwrap();
        let counts = Context::new(&indexes, Movie, None).counts(&request).0;
        assert_eq!(counts["kinds"]["award"]["values"], json!({ "Q19020": 2, "Q42369": 1, "Q130871": 1 }));
        assert_eq!(counts["kinds"]["won"]["values"], json!({ "Q19020": 1, "Q130871": 1 }));
        assert_eq!(counts["kinds"]["award"]["labels"]["Q19020"], "Academy Awards");
        assert_eq!(counts["kinds"]["won"]["labels"]["Q130871"], "Q130871", "no entry: named by its Q-id");
        let (_, won) = titles(&indexes, "sel=won:Q19020");
        assert_eq!(won["coverage"]["won"], json!({ "count": 3, "denominator": 4 }));

        let context = Context::new(&indexes, Movie, None);
        let award = spec("award").unwrap();
        let found =
            context.values(award, &Request::parse(Route::Values(award), Movie.into(), "q=cannes").unwrap()).0;
        assert_eq!(found["values"][0]["id"], "Q42369", "{found}");
        assert_eq!(found["values"][0]["name"], "Cannes Film Festival");
    }

    #[test]
    fn a_titles_awards_and_the_ceremony_list() {
        let indexes = store("list", true);
        assert_eq!(
            of_title(&indexes, Movie, 1),
            [
                json!({ "id": "Q19020", "name": "Academy Awards", "won": true }),
                json!({ "id": "Q42369", "name": "Cannes Film Festival", "won": false }),
            ]
        );
        assert!(of_title(&indexes, Movie, 4).is_empty());
        assert!(of_title(&indexes, Movie, 99).is_empty());
        let list = list_json(&indexes);
        assert_eq!(
            list["ceremonies"][0],
            json!({ "id": "Q19020", "name": "Academy Awards", "movies": 2, "series": 0,
                    "won": { "movies": 1, "series": 0 } })
        );
        assert_eq!(list["ceremonies"].as_array().unwrap().len(), 3);
    }

    /// A store written before the award sections serves without them.
    #[test]
    fn a_store_without_awards_serves_without_them() {
        let indexes = store("none", false);
        let (ids, answer) = titles(&indexes, "sel=won:Q19020");
        assert_eq!(answer["ignored"], json!(["won"]), "{answer}");
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(list_json(&indexes), json!({ "ceremonies": [] }));
        assert!(of_title(&indexes, Movie, 1).is_empty());
    }
}
