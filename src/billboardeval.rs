//! Hand-judged billboard relevance, beside `billboardcheck`'s slate-shape metrics.
//!
//! The ranker is replayed by `billboardcheck`; this module is pure: validated grades and ranked ids in, nDCG@10,
//! bad@10, judged coverage and holdout recall@40 out. Unknown titles are not bad. They are exported, in rank order,
//! for a human to label later rather than guessed by code or a model.

use den_index::eval::{mean, score, Grade, Scores};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};

const SCHEMA_VERSION: u32 = 1;
const K: usize = 10;
const HOLDOUT_K: usize = 40;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Candidate {
    /// One-based position in the complete billboard row. Kept through filtering so the labelling export does not
    /// make an unknown at rank 9 look like rank 1 merely because ranks 1–8 were already judged.
    pub(crate) rank: usize,
    pub(crate) id: String,
    pub(crate) title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Ranked {
    pub(crate) fixture: String,
    pub(crate) candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct File {
    schema_version: u32,
    about: serde_json::Value,
    cases: Vec<CaseFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    fixture: String,
    #[serde(default)]
    judged: Vec<Judgement>,
    #[serde(default)]
    holdout: Vec<Holdout>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Judgement {
    id: String,
    title: String,
    #[serde(deserialize_with = "grade")]
    grade: Grade,
    basis: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Holdout {
    id: String,
    title: String,
    basis: String,
    #[serde(default)]
    note: Option<String>,
}

fn grade<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Grade, D::Error> {
    let name = String::deserialize(deserializer)?;
    Grade::parse(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown grade {name:?}")))
}

#[derive(Debug)]
pub(crate) struct Judged {
    cases: HashMap<String, Case>,
}

#[derive(Debug)]
struct Case {
    grades: HashMap<String, Grade>,
    holdout: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Metrics {
    pub(crate) relevance: Option<Scores>,
    pub(crate) coverage_numerator: usize,
    pub(crate) coverage_denominator: usize,
    pub(crate) holdout_found: usize,
    pub(crate) holdout_denominator: usize,
}

impl Metrics {
    fn holdout_recall(&self) -> Option<f64> {
        (self.holdout_denominator > 0).then(|| self.holdout_found as f64 / self.holdout_denominator as f64)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CaseResult {
    pub(crate) fixture: String,
    pub(crate) metrics: Metrics,
    pub(crate) unjudged: Vec<Candidate>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Evaluation {
    pub(crate) cases: Vec<CaseResult>,
}

impl Evaluation {
    fn total(&self) -> Metrics {
        let relevance: Vec<Scores> = self.cases.iter().filter_map(|case| case.metrics.relevance).collect();
        Metrics {
            relevance: (!relevance.is_empty()).then(|| mean(&relevance)),
            coverage_numerator: self.cases.iter().map(|case| case.metrics.coverage_numerator).sum(),
            coverage_denominator: self.cases.iter().map(|case| case.metrics.coverage_denominator).sum(),
            holdout_found: self.cases.iter().map(|case| case.metrics.holdout_found).sum(),
            holdout_denominator: self.cases.iter().map(|case| case.metrics.holdout_denominator).sum(),
        }
    }
}

pub(crate) fn read<'a>(path: &str, fixtures: impl Iterator<Item = &'a str>) -> Result<Judged, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let file: File = serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))?;
    resolve(file, fixtures).map_err(|e| format!("{path}: {e}"))
}

fn resolve<'a>(file: File, fixtures: impl Iterator<Item = &'a str>) -> Result<Judged, String> {
    if file.schema_version != SCHEMA_VERSION {
        return Err(format!("schemaVersion must be {SCHEMA_VERSION}, got {}", file.schema_version));
    }
    if !file.about.is_object() {
        return Err("about must be an object".to_owned());
    }
    let expected: HashSet<String> = fixtures.map(str::to_owned).collect();
    let mut cases = HashMap::new();
    for case in file.cases {
        if case.fixture.trim().is_empty() {
            return Err("a case has no fixture".to_owned());
        }
        let mut grades = HashMap::new();
        for judged in case.judged {
            validate_entry(&case.fixture, &judged.id, &judged.title, &judged.basis, judged.note.as_deref())?;
            if grades.insert(judged.id.clone(), judged.grade).is_some() {
                return Err(format!("{}: duplicate judgement {}", case.fixture, judged.id));
            }
        }
        let mut holdout = HashSet::new();
        for positive in case.holdout {
            validate_entry(
                &case.fixture,
                &positive.id,
                &positive.title,
                &positive.basis,
                positive.note.as_deref(),
            )?;
            if grades.contains_key(&positive.id) {
                return Err(format!("{}: {} is both judged and held out", case.fixture, positive.id));
            }
            if !holdout.insert(positive.id.clone()) {
                return Err(format!("{}: duplicate holdout {}", case.fixture, positive.id));
            }
        }
        if cases.insert(case.fixture.clone(), Case { grades, holdout }).is_some() {
            return Err(format!("duplicate fixture {:?}", case.fixture));
        }
    }
    let got: HashSet<String> = cases.keys().cloned().collect();
    if got != expected {
        let mut missing: Vec<_> = expected.difference(&got).cloned().collect();
        let mut extra: Vec<_> = got.difference(&expected).cloned().collect();
        missing.sort();
        extra.sort();
        return Err(format!(
            "fixture cases differ: missing [{}], extra [{}]",
            missing.join(", "),
            extra.join(", ")
        ));
    }
    Ok(Judged { cases })
}

fn validate_entry(
    fixture: &str,
    id: &str,
    title: &str,
    basis: &str,
    note: Option<&str>,
) -> Result<(), String> {
    let valid_id = id.split_once(':').is_some_and(|(kind, id)| {
        matches!(kind, "movie" | "series") && id.parse::<u32>().is_ok_and(|id| id > 0)
    });
    if !valid_id {
        return Err(format!("{fixture}: invalid title id {id:?}"));
    }
    if title.trim().is_empty() || basis.trim().is_empty() {
        return Err(format!("{fixture}: {id} needs a title and basis"));
    }
    if note.is_some_and(|note| note.trim().is_empty()) {
        return Err(format!("{fixture}: {id} has an empty note"));
    }
    Ok(())
}

pub(crate) fn evaluate(judged: &Judged, ranked: &[Ranked]) -> Evaluation {
    let cases = ranked
        .iter()
        .map(|ranked| {
            let judged = &judged.cases[&ranked.fixture];
            let row: Vec<String> = ranked.candidates.iter().map(|candidate| candidate.id.clone()).collect();
            let top = &row[..row.len().min(K)];
            let coverage_numerator = top.iter().filter(|id| judged.grades.contains_key(*id)).count();
            let coverage_denominator = top.len();
            let holdout_found = row.iter().take(HOLDOUT_K).filter(|id| judged.holdout.contains(*id)).count();
            let unjudged = ranked
                .candidates
                .iter()
                .filter(|candidate| {
                    !judged.grades.contains_key(&candidate.id) && !judged.holdout.contains(&candidate.id)
                })
                .cloned()
                .collect();
            CaseResult {
                fixture: ranked.fixture.clone(),
                metrics: Metrics {
                    relevance: (!judged.grades.is_empty()).then(|| score(&row, &judged.grades, K)),
                    coverage_numerator,
                    coverage_denominator,
                    holdout_found,
                    holdout_denominator: judged.holdout.len(),
                },
                unjudged,
            }
        })
        .collect();
    Evaluation { cases }
}

pub(crate) fn print(evaluation: &Evaluation) {
    println!("\nrelevance (hand judgements only; slate metrics are above):");
    println!("{:<12} {:>7} {:>6} {:>10} {:>12}", "household", "nDCG@10", "bad@10", "judged@10", "holdout@40");
    for case in &evaluation.cases {
        print_metrics(&case.fixture, &case.metrics);
    }
    print_metrics("all", &evaluation.total());
    println!(
        "nDCG/bad are unavailable until a human adds grades; coverage is judged titles out of the row actually shown."
    );
}

fn print_metrics(label: &str, metrics: &Metrics) {
    let (ndcg, bad) = metrics.relevance.map_or_else(
        || ("-".to_owned(), "-".to_owned()),
        |scores| (format!("{:.3}", scores.ndcg), scores.bad.to_string()),
    );
    let recall = metrics.holdout_recall().map_or_else(|| "-".to_owned(), |recall| format!("{recall:.3}"));
    println!(
        "{label:<12} {ndcg:>7} {bad:>6} {:>4}/{:<5} {:>5}/{:<5} ({recall})",
        metrics.coverage_numerator,
        metrics.coverage_denominator,
        metrics.holdout_found,
        metrics.holdout_denominator,
    );
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Export<'a> {
    schema_version: u32,
    generated_by: &'static str,
    cases: Vec<ExportCase<'a>>,
}

#[derive(Serialize)]
struct ExportCase<'a> {
    fixture: &'a str,
    candidates: Vec<ExportCandidate<'a>>,
}

#[derive(Serialize)]
struct ExportCandidate<'a> {
    rank: usize,
    id: &'a str,
    title: &'a str,
    grade: &'static str,
    basis: &'static str,
}

pub(crate) fn unjudged_json(evaluation: &Evaluation) -> serde_json::Value {
    let export = Export {
        schema_version: SCHEMA_VERSION,
        generated_by: "den-atlas billboard-eval",
        cases: evaluation
            .cases
            .iter()
            .map(|case| ExportCase {
                fixture: &case.fixture,
                candidates: case
                    .unjudged
                    .iter()
                    .map(|candidate| ExportCandidate {
                        rank: candidate.rank,
                        id: &candidate.id,
                        title: &candidate.title,
                        grade: "",
                        basis: "",
                    })
                    .collect(),
            })
            .collect(),
    };
    serde_json::to_value(export).expect("the export contains only JSON values")
}

pub(crate) fn write_unjudged(path: &str, evaluation: &Evaluation) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(&unjudged_json(evaluation)).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(cases: &str) -> File {
        serde_json::from_str(&format!(r#"{{"schemaVersion":1,"about":{{}},"cases":{cases}}}"#)).unwrap()
    }

    fn ranked() -> Vec<Ranked> {
        vec![Ranked {
            fixture: "crime".to_owned(),
            candidates: [1, 9, 2, 3, 8, 4]
                .into_iter()
                .enumerate()
                .map(|(rank, id)| Candidate {
                    rank: rank + 1,
                    id: format!("movie:{id}"),
                    title: format!("Title {id}"),
                })
                .collect(),
        }]
    }

    #[test]
    fn grades_holdout_recall_coverage_and_unknown_export_are_distinct() {
        let judged = resolve(
            file(
                r#"[{"fixture":"crime","judged":[
                    {"id":"movie:1","title":"One","grade":"good","basis":"human"},
                    {"id":"movie:2","title":"Two","grade":"ok","basis":"human"},
                    {"id":"movie:3","title":"Three","grade":"bad","basis":"human"}],
                    "holdout":[
                    {"id":"movie:8","title":"Eight","basis":"human"},
                    {"id":"movie:7","title":"Seven","basis":"human"}]}]"#,
            ),
            ["crime"].into_iter(),
        )
        .unwrap();
        let result = evaluate(&judged, &ranked()).cases.remove(0);
        let scores = result.metrics.relevance.unwrap();
        assert!(scores.ndcg > 0.0 && scores.ndcg < 1.0);
        assert_eq!(scores.bad, 1);
        assert_eq!((result.metrics.coverage_numerator, result.metrics.coverage_denominator), (3, 6));
        assert_eq!((result.metrics.holdout_found, result.metrics.holdout_denominator), (1, 2));
        assert_eq!(result.metrics.holdout_recall(), Some(0.5));
        assert_eq!(result.unjudged.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["movie:9", "movie:4"]);

        let evaluation = Evaluation { cases: vec![result] };
        let export = unjudged_json(&evaluation);
        assert_eq!(export["cases"][0]["candidates"][0]["rank"], 2);
        assert_eq!(export["cases"][0]["candidates"][0]["grade"], "");

        let total = evaluation.total();
        assert_eq!((total.coverage_numerator, total.coverage_denominator), (3, 6));
        assert_eq!((total.holdout_found, total.holdout_denominator), (1, 2));
    }

    #[test]
    fn an_empty_human_set_is_explicitly_unscored_not_zero_quality() {
        let judged =
            resolve(file(r#"[{"fixture":"crime","judged":[],"holdout":[]}]"#), ["crime"].into_iter())
                .unwrap();
        let result = evaluate(&judged, &ranked()).cases.remove(0);
        assert_eq!(result.metrics.relevance, None);
        assert_eq!((result.metrics.coverage_numerator, result.metrics.coverage_denominator), (0, 6));
        assert_eq!(result.metrics.holdout_recall(), None);
        assert_eq!(result.unjudged.len(), 6);
    }

    #[test]
    fn the_schema_refuses_guesses_duplicates_overlap_and_fixture_drift() {
        let bad = [
            r#"[{"fixture":"crime","judged":[{"id":"movie:1","title":"One","grade":"maybe","basis":"x"}]}]"#,
            r#"[{"fixture":"crime","judged":[{"id":"movie:1","title":"One","grade":"good","basis":"x"},{"id":"movie:1","title":"One","grade":"bad","basis":"x"}]}]"#,
            r#"[{"fixture":"crime","judged":[{"id":"movie:1","title":"One","grade":"good","basis":"x"}],"holdout":[{"id":"movie:1","title":"One","basis":"x"}]}]"#,
            r#"[{"fixture":"other","judged":[]}]"#,
        ];
        for cases in bad {
            let parsed = serde_json::from_str::<File>(&format!(
                r#"{{"schemaVersion":1,"about":{{}},"cases":{cases}}}"#
            ));
            assert!(parsed.is_err() || resolve(parsed.unwrap(), ["crime"].into_iter()).is_err(), "{cases}");
        }
    }

    #[test]
    fn the_committed_file_names_every_fixture_without_fabricating_a_grade() {
        let mut fixtures: Vec<String> = std::fs::read_dir("fixtures/billboard")
            .unwrap()
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension()?.to_str()? == "json")
                    .then(|| path.file_stem().unwrap().to_string_lossy().into_owned())
            })
            .collect();
        fixtures.sort();
        let judged = read("judged/billboard.json", fixtures.iter().map(String::as_str)).unwrap();
        assert_eq!(judged.cases.len(), fixtures.len());
        assert!(judged.cases.values().all(|case| case.grades.is_empty() && case.holdout.is_empty()));
    }
}
