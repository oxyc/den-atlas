//! Score More Like This against the hand-judged set in `judged/rail.json`.
//!
//!   den-atlas rail-eval <dataset dir>
//!   RAIL_JUDGED=<file>        den-atlas rail-eval <dataset dir>   # another judged file
//!   RAIL_EVAL_UNJUDGED=10     den-atlas rail-eval <dataset dir>   # …and list what is not judged yet
//!   RAIL_KNOBS='w_maker=1'    den-atlas rail-eval <dataset dir>   # knobs moved off production
//!   RAIL_EVAL_SHOW=movie:11   den-atlas rail-eval <dataset dir>   # …and print that seed's first ten
//!   CACHE_DIR=<dir>           den-atlas rail-eval <dataset dir>   # with the character links kept there
//!
//! The character links come from TMDB's credits, which are kept on the box only (`tmdb.rs`): without
//! `CACHE_DIR` naming a directory of them the row is ranked with no links, which is not production's row.
//!
//! Each case is a seed and a set of candidates judged `good` / `ok` / `bad` as recommendations for it. The
//! row scored is `Indexes::more_like_this`, the function `/index/similar` serves, over the same store load.
//! Beside it runs a control arm: the plot index's nearest neighbours, which is what `/index/neighbours`
//! answers — a weight change that does not beat the control has not earned its complexity.
//!
//! # The metrics, at k = 10
//!
//! - **nDCG@10** — graded, gain 2 / 1 / 0 for good / ok / bad, an unjudged title scoring 0. The ideal is
//!   the case's own judgements sorted best first, so a case with three goods is out of three goods.
//! - **nDCG'@10** — the same over the row with unjudged titles removed first (Sakai's condensed list).
//!   The judgements are a sample, not the corpus, so a change that surfaces a good title nobody judged yet
//!   reads as a loss on plain nDCG and not on this one. Read the two together: plain nDCG falling while
//!   condensed holds means the row moved onto unjudged ground, and the set needs judging, not the weights.
//! - **P@10** — the share of the first ten judged good or ok.
//! - **bad@10** — judged-bad titles in the first ten, summed over the cases. The Bates-Motel-in-The-Wire
//!   count: the defect a viewer actually notices.
//! - **judged@10** — how many of the first ten carry a judgement at all. Low means the numbers above are
//!   measuring the sample more than the ranking; `RAIL_EVAL_UNJUDGED` lists the gap, ready to paste.
//!
//! Deterministic: the store, the scorer and `Index::nearest` break every tie by id, and the cases are read
//! in file order. Two runs over one dataset print the same bytes.
//!
//! # dev and test
//!
//! Every case carries the half den-dataset's `scripts/v2/split.py` assigns its seed, so a title is in the
//! same half here as in the premise triplets and the co-rating ruler. Tune on dev; read test once, to
//! confirm. The test in this file re-derives each split, so a case cannot be moved between halves by hand.
//!
//! The metrics themselves are `den_index::eval`, which the tuning playground scores with too.

use crate::queries::Indexes;
use den_index::eval::{mean, score, Grade, Scores};
use den_index::MediaType;
use serde::{Deserialize, Deserializer};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub(crate) const K: usize = 10;

/// The judged set as committed, compiled in so the playground can score against it with no file on the box.
const EMBEDDED: &str = include_str!("../judged/rail.json");

#[derive(Deserialize)]
struct Judged {
    cases: Vec<Case>,
}

/// Only the fields scoring reads. The file carries more for people — `covers`, `note` — and serde skips them.
#[derive(Deserialize)]
pub(crate) struct Case {
    pub(crate) seed: String,
    pub(crate) title: String,
    pub(crate) split: String,
    judged: Vec<Judgement>,
}

#[derive(Deserialize)]
struct Judgement {
    id: String,
    title: String,
    #[serde(deserialize_with = "grade")]
    grade: Grade,
    /// Why the grade is what it is, by source — see `judged/rail.json`'s `about`.
    basis: String,
}

fn grade<'de, D: Deserializer<'de>>(d: D) -> Result<Grade, D::Error> {
    let name = String::deserialize(d)?;
    Grade::parse(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown grade {name:?}")))
}

/// `movie:137` / `series:1438` — the type names atlas emits, as `judged/queries.json` uses them.
fn parse_key(key: &str) -> Option<(MediaType, u32)> {
    let (kind, id) = key.split_once(':')?;
    let media = match kind {
        "movie" => MediaType::Movie,
        "series" => MediaType::Tv,
        _ => return None,
    };
    Some((media, id.parse().ok()?))
}

/// The dataset key form split.py hashes: `tv:1438`, not `series:1438`.
#[cfg(test)]
fn dataset_key(media: MediaType, id: u32) -> String {
    match media {
        MediaType::Movie => format!("movie:{id}"),
        MediaType::Tv => format!("tv:{id}"),
    }
}

/// A case, resolved: the seed and its grades by id.
pub(crate) struct Resolved<'a> {
    pub(crate) case: &'a Case,
    pub(crate) media: MediaType,
    pub(crate) id: u32,
    pub(crate) grades: HashMap<u32, Grade>,
}

/// The compiled-in judged set, resolved once. It cannot fail at runtime in a built binary: the test below
/// resolves the same bytes, so a set that would not resolve fails the build's tests instead.
pub(crate) fn embedded() -> &'static [Resolved<'static>] {
    static JUDGED: OnceLock<Judged> = OnceLock::new();
    static CASES: OnceLock<Vec<Resolved<'static>>> = OnceLock::new();
    CASES.get_or_init(|| {
        let judged =
            JUDGED.get_or_init(|| serde_json::from_str(EMBEDDED).expect("judged/rail.json parses (tested)"));
        resolve(judged).expect("judged/rail.json resolves (tested)")
    })
}

/// Every case's keys parsed and checked. A malformed file is refused whole: scoring the cases that happen
/// to parse would report a number over a different set than the one on disk.
fn resolve(judged: &Judged) -> Result<Vec<Resolved<'_>>, String> {
    let mut seeds = HashSet::new();
    judged
        .cases
        .iter()
        .map(|case| {
            let (media, id) = parse_key(&case.seed).ok_or_else(|| format!("bad seed key {:?}", case.seed))?;
            if !seeds.insert((media, id)) {
                return Err(format!("{} is a seed twice", case.seed));
            }
            if case.split != "dev" && case.split != "test" {
                return Err(format!("{}: split must be dev or test, got {:?}", case.seed, case.split));
            }
            let mut grades = HashMap::new();
            for j in &case.judged {
                let (m, other) =
                    parse_key(&j.id).ok_or_else(|| format!("{}: bad key {:?}", case.seed, j.id))?;
                // More Like This never crosses media types, so a judgement that does can never be scored.
                if m != media || other == id {
                    return Err(format!("{}: {} can never appear in this row", case.seed, j.id));
                }
                // A grade nobody can trace back to a reason cannot be argued with, only deleted.
                if j.basis.trim().is_empty() || j.title.trim().is_empty() {
                    return Err(format!("{}: {} needs a title and a basis", case.seed, j.id));
                }
                if grades.insert(other, j.grade).is_some() {
                    return Err(format!("{}: {} is judged twice", case.seed, j.id));
                }
            }
            Ok(Resolved { case, media, id, grades })
        })
        .collect()
}

fn title(indexes: &Indexes, media: MediaType, id: u32) -> String {
    let card = indexes.cards.as_ref().and_then(|c| c.get(&(media, id)));
    card.map_or_else(
        || id.to_string(),
        |c| c.year.map_or_else(|| c.title.clone(), |y| format!("{} ({y})", c.title)),
    )
}

fn line(label: &str, s: &Scores, plot: &Scores) {
    println!(
        "{label:<34} {:>6.3} {:>6.3} {:>5.2} {:>4} {:>4}   {:>6.3} {:>6.3}",
        s.ndcg, s.condensed, s.precision, s.bad, s.judged, plot.ndcg, plot.condensed
    );
}

/// Exit code, as the other subcommands return one.
pub async fn run(dir: &std::path::Path) -> i32 {
    let path = std::env::var("RAIL_JUDGED").unwrap_or_else(|_| "judged/rail.json".to_owned());
    let judged: Judged = match std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(j) => j,
        Err(e) => {
            eprintln!("rail-eval: {path}: {e}");
            return 1;
        }
    };
    let cases = match resolve(&judged) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rail-eval: {path}: {e}");
            return 1;
        }
    };
    let dataset = match crate::dataset::Dataset::load(dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("rail-eval: dataset at {} will not load: {e}", dir.display());
            return 1;
        }
    };
    let params = match crate::playground::parse(&std::env::var("RAIL_KNOBS").unwrap_or_default()) {
        Ok(tuning) => tuning.params,
        Err(e) => {
            eprintln!("rail-eval: RAIL_KNOBS: {e}");
            return 1;
        }
    };
    let cache_dir = std::env::var("CACHE_DIR").ok().filter(|d| !d.is_empty()).map(std::path::PathBuf::from);
    let characters = match cache_dir {
        Some(cache_dir) => match crate::tmdb::Tmdb::new(dataset.store.clone(), Some(cache_dir), None, 0) {
            Ok(tmdb) => {
                eprintln!("{}", tmdb.load().await);
                Some(tmdb.characters())
            }
            Err(e) => {
                eprintln!("rail-eval: {e}");
                return 1;
            }
        },
        None => {
            eprintln!("rail-eval: no CACHE_DIR, so no character links");
            None
        }
    };
    let queries = crate::queries::IndexQueries::new(&dataset).with_characters(characters);
    let indexes = match queries.get(|| ()).await {
        Ok((i, _)) => i,
        Err(e) => {
            eprintln!("rail-eval: {e}");
            return 1;
        }
    };
    let unjudged: usize = std::env::var("RAIL_EVAL_UNJUDGED").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let production = params == den_index::SimilarParams::default();
    let show = std::env::var("RAIL_EVAL_SHOW").ok();

    println!("dataset {}  ·  {} cases from {path}  ·  k = {K}", indexes.dataset_version, cases.len());
    if !production {
        println!("knobs {}", std::env::var("RAIL_KNOBS").unwrap_or_default());
    }
    println!(
        "{:<34} {:>6} {:>6} {:>5} {:>4} {:>4}   {:>6} {:>6}",
        "seed", "nDCG", "nDCG'", "P", "bad", "jdg", "plot", "plot'"
    );
    println!("{}", "-".repeat(84));
    let mut by_split: HashMap<&str, (Vec<Scores>, Vec<Scores>)> = HashMap::new();
    let mut gaps: Vec<String> = Vec::new();
    for c in &cases {
        let row: Vec<u32> = if production {
            indexes.more_like_this(c.id, c.media).to_vec()
        } else {
            indexes.more_like_this_scored(c.id, c.media, &params).iter().map(|s| s.tmdb_id).collect()
        };
        let plot: Vec<u32> =
            indexes.plot.nearest(c.id, c.media, den_index::MAX_ROW).into_iter().map(|n| n.tmdb_id).collect();
        let (s, p) = (score(&row, &c.grades, K), score(&plot, &c.grades, K));
        let name: String = c.case.title.chars().take(26).collect();
        line(&format!("{name} [{}]", c.case.split), &s, &p);
        if show.as_deref() == Some(c.case.seed.as_str()) {
            for (at, id) in row.iter().take(K).enumerate() {
                let grade = match c.grades.get(id) {
                    Some(Grade::Good) => "good",
                    Some(Grade::Ok) => "ok",
                    Some(Grade::Bad) => "bad",
                    None => "-",
                };
                println!("    {:>2}. {grade:<4} {}", at + 1, title(&indexes, c.media, *id));
            }
        }
        for half in [c.case.split.as_str(), "all"] {
            let entry = by_split.entry(half).or_default();
            entry.0.push(s);
            entry.1.push(p);
        }
        if unjudged > 0 {
            let mut seen = HashSet::new();
            for (arm, list) in [("rail", &row), ("plot", &plot)] {
                for (at, id) in list.iter().take(unjudged).enumerate() {
                    if !c.grades.contains_key(id) && seen.insert(*id) {
                        let key = match c.media {
                            MediaType::Movie => format!("movie:{id}"),
                            MediaType::Tv => format!("series:{id}"),
                        };
                        gaps.push(format!(
                            "{} <- {arm} #{}: {{\"id\": \"{key}\", \"title\": {:?}, \"grade\": \"\", \"basis\": \"\"}}",
                            c.case.seed,
                            at + 1,
                            title(&indexes, c.media, *id)
                        ));
                    }
                }
            }
        }
    }
    println!("{}", "-".repeat(84));
    for half in ["dev", "test", "all"] {
        if let Some((rail, plot)) = by_split.get(half) {
            line(&format!("MEAN {half} (n={})", rail.len()), &mean(rail), &mean(plot));
        }
    }
    println!("\nnDCG' ignores unjudged titles; bad and jdg are totals over the cases, of {K} per case.");
    if !gaps.is_empty() {
        println!("\nunjudged, in the first {unjudged} of either arm:");
        for g in &gaps {
            println!("  {g}");
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped set parses, every key is one the rail can return, and every case sits in the half
    /// den-dataset's split.py gives its seed — `sha256(SALT|key)[0] & 1`, 1 is test.
    #[test]
    fn the_judged_set_is_well_formed_and_its_split_is_derived() {
        use sha2::{Digest, Sha256};
        let cases = embedded();
        assert!(cases.len() >= 30, "the set is meant to hold 30-50 seeds, has {}", cases.len());
        for c in cases {
            let digest = Sha256::digest(format!("den-v2-2026-09-04|{}", dataset_key(c.media, c.id)));
            let half = if digest[0] & 1 == 1 { "test" } else { "dev" };
            assert_eq!(c.case.split, half, "{} is in the wrong half", c.case.seed);
            assert!(!c.case.judged.is_empty(), "{} has no judgements", c.case.seed);
        }
    }
}
