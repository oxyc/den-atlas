//! Corpus acceptance for oxyc/den-atlas#92.
//!
//! `den-atlas franchise-check <dataset dir>` reads the staged store through the same index contract serving
//! uses. It is deliberately separate from `check`: old stores without optional curated-franchise sections
//! remain valid, while the rebuilt generation intended to complete #92 must pass these stronger assertions.

use crate::queries::Indexes;
use den_index::MediaType;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

type Key = (MediaType, u32);
const ACCEPTANCE: &str = include_str!("../judged/franchise.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct File {
    schema_version: u32,
    about: serde_json::Value,
    cases: Vec<Case>,
    #[serde(default)]
    apart: Vec<[String; 2]>,
    #[serde(default)]
    none: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Case {
    name: String,
    seed: String,
    required_members: Vec<String>,
    #[serde(default)]
    exact_member_count: Option<usize>,
    #[serde(default)]
    eras: Vec<Vec<String>>,
    #[serde(default = "default_similar")]
    minimum_similar: usize,
}

fn default_similar() -> usize {
    crate::handler::SIMILAR_PAGE
}

fn parse_key(raw: &str) -> Result<Key, String> {
    let (kind, id) = raw.split_once(':').ok_or_else(|| format!("bad title key {raw:?}"))?;
    let media = match kind {
        "movie" => MediaType::Movie,
        "tv" | "series" => MediaType::Tv,
        _ => return Err(format!("bad title type in {raw:?}")),
    };
    Ok((media, id.parse().map_err(|_| format!("bad title id in {raw:?}"))?))
}

fn key_name((media, id): Key) -> String {
    format!("{}:{id}", if media == MediaType::Movie { "movie" } else { "tv" })
}

fn read() -> Result<File, String> {
    let file: File = serde_json::from_str(ACCEPTANCE).map_err(|e| format!("judged/franchise.json: {e}"))?;
    if file.schema_version != 1 {
        return Err(format!("judged/franchise.json: schemaVersion must be 1, got {}", file.schema_version));
    }
    if !file.about.is_object() {
        return Err("judged/franchise.json: about must be an object".to_owned());
    }
    let mut names = HashSet::new();
    for case in &file.cases {
        if case.name.trim().is_empty() || !names.insert(&case.name) {
            return Err(format!("judged/franchise.json: empty or duplicate case name {:?}", case.name));
        }
        let seed = parse_key(&case.seed)?;
        let required: HashSet<Key> =
            case.required_members.iter().map(|key| parse_key(key)).collect::<Result<_, _>>()?;
        if !required.contains(&seed) {
            return Err(format!("{}: requiredMembers must include seed {}", case.name, case.seed));
        }
        if required.len() != case.required_members.len() {
            return Err(format!("{}: requiredMembers contains a duplicate", case.name));
        }
        let mut era_members = HashSet::new();
        for era in &case.eras {
            if era.is_empty() {
                return Err(format!("{}: an era is empty", case.name));
            }
            for raw in era {
                let key = parse_key(raw)?;
                if !required.contains(&key) || !era_members.insert(key) {
                    return Err(format!("{}: era member {raw} is absent or repeated", case.name));
                }
            }
        }
    }
    for pair in &file.apart {
        parse_key(&pair[0])?;
        parse_key(&pair[1])?;
    }
    for key in &file.none {
        parse_key(key)?;
    }
    Ok(file)
}

fn audit_case(indexes: &Indexes, case: &Case) -> Vec<String> {
    let mut failures = Vec::new();
    let seed = parse_key(&case.seed).expect("validated fixture");
    let Some(primary) = indexes.franchises.primary(seed) else {
        return vec![format!("{}: seed {} has no curated primary franchise", case.name, case.seed)];
    };
    let required: Vec<Key> =
        case.required_members.iter().map(|key| parse_key(key).expect("validated fixture")).collect();
    for &key in &required {
        if !indexes.franchises.shares_primary(seed, key) {
            failures.push(format!(
                "{}: {} is not in seed primary {:?}",
                case.name,
                key_name(key),
                primary.id
            ));
        }
    }
    if let Some(expected) = case.exact_member_count {
        if primary.members.len() != expected {
            failures.push(format!(
                "{}: primary {:?} has {} members, expected exactly {expected}",
                case.name,
                primary.id,
                primary.members.len()
            ));
        }
    }

    let positions: std::collections::HashMap<Key, usize> =
        primary.members.iter().enumerate().map(|(at, member)| (member.key, at)).collect();
    let mut era_ids = HashSet::new();
    for era in &case.eras {
        let keys: Vec<Key> = era.iter().map(|key| parse_key(key).expect("validated fixture")).collect();
        let Some(membership) = indexes.franchises.membership(keys[0]) else { continue };
        if !era_ids.insert(membership.era) {
            failures.push(format!("{}: two expected eras resolve to era {}", case.name, membership.era));
        }
        for &key in &keys {
            if indexes.franchises.membership(key).map(|m| m.era) != Some(membership.era) {
                failures.push(format!("{}: expected era does not contain {}", case.name, key_name(key)));
            }
        }
        let order: Vec<usize> = keys.iter().filter_map(|key| positions.get(key).copied()).collect();
        if !order.windows(2).all(|pair| pair[0] < pair[1]) {
            failures.push(format!("{}: expected era is not in release order", case.name));
        }
    }

    let Some(row) = indexes.franchises.members_for(seed, |key| crate::franchises::title_facts(indexes, key))
    else {
        return failures;
    };
    let row_keys: HashSet<Key> = row.iter().map(|member| member.key).collect();
    if row.len() != primary.members.len() || row_keys.len() != primary.members.len() {
        failures.push(format!("{}: franchise row loses or repeats a primary member", case.name));
    }
    if row.first().map(|member| member.era) != indexes.franchises.membership(seed).map(|m| m.era) {
        failures.push(format!("{}: franchise row does not lead with the seed era", case.name));
    }
    let mut row_eras = HashSet::new();
    for same_era in row.chunk_by(|a, b| a.era == b.era) {
        if !row_eras.insert(same_era[0].era) {
            failures.push(format!("{}: franchise row splits one era into multiple runs", case.name));
        }
        if !same_era.windows(2).all(|pair| pair[0].order < pair[1].order) {
            failures.push(format!("{}: franchise row is not release ordered within an era", case.name));
        }
    }

    let similar = indexes.more_like_this_mixed(seed.1, seed.0);
    if similar.len() < case.minimum_similar {
        failures.push(format!(
            "{}: More Like This has {} titles, expected at least {}",
            case.name,
            similar.len(),
            case.minimum_similar
        ));
    }
    if let Some(key) = similar.iter().find(|&&key| indexes.franchises.shares_primary(seed, key)) {
        failures.push(format!(
            "{}: More Like This still contains primary member {}",
            case.name,
            key_name(*key)
        ));
    }
    failures
}

fn audit(indexes: &Indexes, file: &File) -> Vec<String> {
    let mut failures: Vec<String> = file.cases.iter().flat_map(|case| audit_case(indexes, case)).collect();
    for [left, right] in &file.apart {
        let (a, b) =
            (parse_key(left).expect("validated fixture"), parse_key(right).expect("validated fixture"));
        if indexes.franchises.shares_primary(a, b) {
            failures.push(format!("control: {left} and {right} incorrectly share a primary franchise"));
        }
    }
    for raw in &file.none {
        let key = parse_key(raw).expect("validated fixture");
        if let Some(primary) = indexes.franchises.primary(key) {
            failures.push(format!("control: {raw} incorrectly has primary {:?}", primary.id));
        }
    }
    failures
}

pub async fn run(dir: &Path) -> i32 {
    let file = match read() {
        Ok(file) => file,
        Err(error) => {
            eprintln!("franchise-check: {error}");
            return 1;
        }
    };
    let dataset = match crate::dataset::Dataset::load(dir) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("franchise-check: dataset at {} will not load: {error}", dir.display());
            return 1;
        }
    };
    let indexes = match crate::queries::IndexQueries::new(&dataset).get(|| ()).await {
        Ok((indexes, _)) => indexes,
        Err(error) => {
            eprintln!("franchise-check: {error}");
            return 1;
        }
    };
    let failures = audit(&indexes, &file);
    if failures.is_empty() {
        println!(
            "franchise-check: ok (dataset {}, {} acceptance groups, {} curated franchises)",
            indexes.dataset_version,
            file.cases.len(),
            indexes.franchises.len()
        );
        0
    } else {
        for failure in &failures {
            eprintln!("franchise-check: {failure}");
        }
        eprintln!("franchise-check: FAILED ({} assertions)", failures.len());
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_acceptance_is_well_formed_and_pins_the_named_controls() {
        let file = read().unwrap();
        let beck = file.cases.iter().find(|case| case.name == "Beck").unwrap();
        assert_eq!(beck.seed, "movie:270043");
        assert_eq!(beck.exact_member_count, Some(46));
        assert_eq!(beck.required_members.len(), 46);
        for name in ["Wallander", "James Bond", "Spider-Man", "Star Wars"] {
            assert!(file.cases.iter().any(|case| case.name == name), "missing {name}");
        }
        assert!(!file.apart.is_empty() && !file.none.is_empty());
    }

    #[tokio::test]
    async fn the_reader_fixture_exercises_grouping_era_order_and_exclusion() {
        let Some(path) = crate::store::spec_fixture() else { return };
        let dir = std::env::temp_dir().join(format!("den-atlas-franchise-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(path, dir.join("s.store")).unwrap();
        std::fs::write(
            dir.join("dataset.meta.json"),
            br#"{"datasetVersion":"fixture","taxonomyVersion":"fixture","embeddingModel":"m","dims":1024,
                 "quantization":"int8","storeFile":"s.store"}"#,
        )
        .unwrap();
        let dataset = crate::dataset::Dataset::load(&dir).unwrap();
        let (indexes, _) = crate::queries::IndexQueries::new(&dataset).get(|| ()).await.unwrap();
        let case = Case {
            name: "fixture".into(),
            seed: "movie:1".into(),
            required_members: vec!["movie:1".into(), "tv:10".into()],
            exact_member_count: Some(2),
            eras: vec![vec!["movie:1".into()], vec!["tv:10".into()]],
            minimum_similar: 0,
        };
        assert!(audit_case(&indexes, &case).is_empty());
    }
}
