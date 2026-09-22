//! The self-describing query schema. Every population count carries both the number of titles for which the
//! field is known and the full corpus denominator; partial classification must never read as a census.

use crate::queries::Indexes;
use den_index::{MediaType, DISPLAY_CONFIDENCE_FLOOR};
use serde_json::{json, Map, Value};

fn coverage(known: usize, population: usize) -> Value {
    json!({
        "count": known,
        "denominator": population,
        "ratio": if population == 0 { 0.0 } else { known as f64 / population as f64 },
    })
}

fn counted_values(values: Vec<(String, usize)>, known: usize, population: usize) -> Value {
    values
        .into_iter()
        .map(|(value, matched)| {
            json!({
                "value": value,
                "count": {
                    "matched": matched,
                    "known": known,
                    "population": population,
                }
            })
        })
        .collect()
}

fn field(
    kind: &str,
    filterable: bool,
    known: usize,
    population: usize,
    values: Vec<(String, usize)>,
) -> Value {
    let mut answer = json!({
        "type": kind,
        "filterable": filterable,
        "coverage": coverage(known, population),
    });
    if !values.is_empty() {
        answer["values"] = counted_values(values, known, population);
    }
    answer
}

fn named(values: Vec<(&str, usize)>) -> Vec<(String, usize)> {
    values.into_iter().map(|(value, count)| (value.to_owned(), count)).collect()
}

/// `/index/schema.json`: what can be queried, its value vocabulary, and how much of the corpus can honestly
/// answer each field. It deliberately says that result totals are retrieval counts, not corpus aggregates;
/// group-by/count tools must not be built by relabelling search output.
pub fn document(indexes: &Indexes) -> Value {
    let population = indexes.population;
    let floor = DISPLAY_CONFIDENCE_FLOOR;
    let mut fields = Map::new();

    let type_values: Vec<(String, usize)> = indexes.facets.as_ref().map_or_else(
        || {
            indexes
                .plot
                .media_type_counts()
                .into_iter()
                .map(|(kind, count)| {
                    (if kind == MediaType::Tv { "series" } else { "movie" }.to_owned(), count)
                })
                .collect()
        },
        |facets| facets.value_counts("mediaType"),
    );
    let type_known = type_values.iter().map(|(_, count)| count).sum();
    fields.insert("mediaType".to_owned(), field("enum", true, type_known, population, type_values));

    let genres = named(indexes.plot.primary_genre_counts());
    let genre_known = genres.iter().map(|(_, count)| count).sum();
    fields.insert("primaryGenre".to_owned(), field("enum", true, genre_known, population, genres));

    let subgenres = named(indexes.plot.subgenre_counts(floor));
    let subgenre_known = indexes.plot.subgenre_coverage(floor);
    fields.insert("subgenre".to_owned(), field("enum", true, subgenre_known, population, subgenres));

    let moods = named(indexes.plot.mood_counts(floor));
    let mood_known = indexes.plot.mood_coverage(floor);
    fields.insert("mood".to_owned(), field("enum", true, mood_known, population, moods));

    for (name, kind, enumerable) in [
        ("country", "countryCode", true),
        ("language", "languageCode", true),
        ("year", "integer", false),
        ("decade", "integer", true),
    ] {
        let known = indexes.facets.as_ref().map_or(0, |facets| facets.coverage(name));
        let values = if enumerable {
            indexes.facets.as_ref().map_or_else(Vec::new, |facets| facets.value_counts(name))
        } else {
            Vec::new()
        };
        fields.insert(name.to_owned(), field(kind, true, known, population, values));
    }

    if let Some(plot_facets) = &indexes.plot_facets {
        for facet in plot_facets.schema() {
            fields.insert(facet.axis, field("enum", true, facet.known, population, facet.values));
        }
    }

    fields.insert(
        "title".to_owned(),
        field(
            "string",
            true,
            indexes.cards.as_ref().map_or(0, std::collections::HashMap::len),
            population,
            vec![],
        ),
    );
    fields.insert("plotSemantic".to_owned(), field("vector", true, indexes.plot.len(), population, vec![]));
    fields.insert(
        "premiseSemantic".to_owned(),
        field("vector", true, indexes.premise.as_ref().map_or(0, den_index::Index::len), population, vec![]),
    );
    fields.insert(
        "people".to_owned(),
        field("entity", true, indexes.facts.as_ref().map_or(0, crate::facts::Facts::len), population, vec![]),
    );

    json!({
        "schemaVersion": 1,
        "datasetVersion": indexes.dataset_version,
        "taxonomyVersion": indexes.plot.taxonomy_version(),
        "population": { "count": population },
        "fields": fields,
        "semantics": {
            "missing": "unknown",
            "resultTotal": "retrievedCandidatesNotCorpusCount",
            "groupBy": false,
        },
    })
}

/// Coverage attached to a browse-row count. Each filtered field is reported separately because the intersection
/// may not be missing at random, and a single blended percentage would hide which axis is thin.
pub fn row_coverage(indexes: &Indexes, media_type: MediaType, constraints: &[(String, String)]) -> Value {
    let mut fields = Map::new();
    let scoped = indexes
        .facets
        .as_ref()
        .and_then(|facets| {
            let wanted = if media_type == MediaType::Tv { "series" } else { "movie" };
            facets.value_counts("mediaType").into_iter().find(|(value, _)| value == wanted).map(|(_, n)| n)
        })
        .or_else(|| {
            indexes.plot.media_type_counts().into_iter().find(|(kind, _)| *kind == media_type).map(|x| x.1)
        })
        .unwrap_or(0);
    for (name, _) in constraints {
        let known = match name.as_str() {
            "subgenre" => indexes.plot.subgenre_coverage_for(Some(media_type), DISPLAY_CONFIDENCE_FLOOR),
            "mood" => indexes.plot.mood_coverage_for(Some(media_type), DISPLAY_CONFIDENCE_FLOOR),
            "primaryGenre" => indexes.plot.primary_genre_coverage_for(Some(media_type)),
            // The facts, which live in the facet index rather than among the plot axes. Without this arm
            // they fell to the axis branch and reported 0 known of 47,618 for a row that had just listed
            // 961 Korean films — a coverage block that is worse than none, since a client is entitled to
            // read it as "this row is guesswork".
            field @ ("country" | "language" | "year" | "decade") => {
                indexes.facets.as_ref().map_or(0, |facets| facets.coverage_for(field, Some(media_type)))
            }
            axis => {
                indexes.plot_facets.as_ref().map_or(0, |facets| facets.coverage_for(axis, Some(media_type)))
            }
        };
        fields.insert(name.clone(), coverage(known, scoped));
    }
    json!({
        "population": indexes.population,
        "mediaType": if media_type == MediaType::Tv { "series" } else { "movie" },
        "denominator": scoped,
        "fields": fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schema_counts_always_name_known_and_population() {
        let dir = std::env::temp_dir().join(format!("den-atlas-schema-{}", std::process::id()));
        let queries = crate::queries::IndexQueries::new(&crate::queries::write_fixture(&dir));
        let (indexes, _) = queries.get(|| ()).await.unwrap();
        let schema = document(&indexes);
        // The fixture carries 12 titles and 8 of them have no labels at all, so `count` and `denominator`
        // differ — which is the whole point of this endpoint. A client asking "how many heist films" must be
        // told 4 OF 12, never 4, or it reports a fraction of the corpus as though it were all of it.
        assert_eq!(schema["population"]["count"], 12);
        assert_eq!(schema["fields"]["subgenre"]["coverage"]["count"], 4);
        assert_eq!(schema["fields"]["subgenre"]["coverage"]["denominator"], 12);
        assert_eq!(schema["fields"]["mood"]["coverage"]["count"], 1);
        assert_eq!(schema["fields"]["tone"]["coverage"]["count"], 3);
        assert_eq!(schema["fields"]["subgenre"]["values"][0]["count"]["population"], 12);
        assert_eq!(schema["semantics"]["groupBy"], false);
    }
}
