//! Score More Like This against the hand-judged set in `judged/rail.json`, with the other type's grades from
//! `judged/rail-cross.json` added to each case.
//!
//!   den-atlas rail-eval <dataset dir>
//!   RAIL_JUDGED=<file>        den-atlas rail-eval <dataset dir>   # another judged file
//!   RAIL_JUDGED_CROSS=<file>  den-atlas rail-eval <dataset dir>   # another cross-type file; empty: none
//!   RAIL_EVAL_UNJUDGED=10     den-atlas rail-eval <dataset dir>   # …and list what is not judged yet
//!   RAIL_KNOBS='w_maker=1'    den-atlas rail-eval <dataset dir>   # knobs moved off production
//!   RAIL_EVAL_SHOW=movie:11   den-atlas rail-eval <dataset dir>   # …and print that seed's first ten
//!   CACHE_DIR=<dir>           den-atlas rail-eval <dataset dir>   # with the character links kept there
//!
//! The character links come from TMDB's credits, which are kept on the box only (`tmdb.rs`): without
//! `CACHE_DIR` naming a directory of them the row is ranked with no links, which is not production's row.
//!
//! Each case is a seed and a set of candidates judged `good` / `ok` / `bad` as recommendations for it. The
//! row scored is `Indexes::more_like_this_mixed`, what `/index/similar` serves as `mixed`, over the same
//! store load (with `RAIL_KNOBS=mix_types=0`, the one-type row its `ids` carry).
//! Beside it runs a control arm: the plot index's nearest neighbours, which is what `/index/neighbours`
//! answers — a weight change that does not beat the control has not earned its complexity.
//!
//! # The metrics, at k = 10
//!
//! - **nDCG@10** — graded, gain 2 / 1 / 0 for good / ok / bad, an unjudged title scoring 0. The ideal is
//!   the case's own judgements sorted best first, so a case with three goods is out of three goods — both
//!   types' goods, so a row of one type is read against the same ideal as a mixed one and the two compare.
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
//! # Row shape, at the first 20
//!
//! nDCG judges members, and a genre shelf is made of individually defensible members. So beside it:
//!
//! - **genre** — the share of the row carrying the seed's own primary genre, per case and averaged, for the
//!   rail and the control alike (oxyc/den-atlas#24's single number; 1.00 is a shelf). A change that lowers it
//!   while nDCG falls has added noise, not variety.
//! - **shape** — a case's own assertions (`max_share`, `min_distinct`, `mutual`, `beats_plot`; see the
//!   file's `about.shape`), each against the `observed` outcome the file records. `(known)` is a recorded
//!   defect; `CHANGED` is an outcome that moved, either way.
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
use den_index::eval::{distinct, mean, score, share, Grade, Scores};
use den_index::MediaType;
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

pub(crate) const K: usize = 10;

/// The judged set as committed, compiled in so the playground can score against it with no file on the box:
/// the seed type's grades, and the other type's (`judged/rail-cross.json`, oxyc/den-atlas#49).
const EMBEDDED: &str = include_str!("../judged/rail.json");
const EMBEDDED_CROSS: &str = include_str!("../judged/rail-cross.json");

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
    #[serde(default)]
    shape: Option<ShapeSpec>,
}

/// A case's row-shape assertions, as the file spells them (`about.shape` in `judged/rail.json`). Unknown keys
/// are refused, so a misspelt assertion cannot sit in the file passing by never being read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShapeSpec {
    #[serde(default)]
    max_share: BTreeMap<String, BTreeMap<String, f64>>,
    #[serde(default)]
    min_distinct: BTreeMap<String, usize>,
    #[serde(default)]
    mutual: Option<usize>,
    #[serde(default)]
    beats_plot: bool,
    observed: String,
    #[serde(default, rename = "note")]
    _note: Option<String>,
}

/// A title property a shape assertion reads: its primary genre, or a subgenre or mood it carries confidently.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    PrimaryGenre,
    Subgenre,
    Mood,
}

impl Field {
    fn parse(name: &str) -> Option<Field> {
        match name {
            "primaryGenre" => Some(Field::PrimaryGenre),
            "subgenre" => Some(Field::Subgenre),
            "mood" => Some(Field::Mood),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Field::PrimaryGenre => "primaryGenre",
            Field::Subgenre => "subgenre",
            Field::Mood => "mood",
        }
    }
}

/// A case's shape assertions, parsed. Each is read over the row's first `SHAPE_K`.
///
/// - `max_share` — at most this fraction of the row may carry this value.
/// - `min_distinct` — the row carries at least this many values of the field between its titles.
/// - `mutual` — the row's first title has the seed in ITS first n: if the index thinks A is B's best
///   neighbour, B should be near A. A consistency check, not a taste judgement.
/// - `beats_plot` — the row scores a higher nDCG@10 than the plot-only control does on this case.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Shape {
    max_share: Vec<(Field, String, f64)>,
    min_distinct: Vec<(Field, usize)>,
    mutual: Option<usize>,
    beats_plot: bool,
    /// Whether the assertions held on the dataset `about.shape.observedOn` names.
    pub(crate) observed_pass: bool,
}

impl ShapeSpec {
    fn parse(&self, seed: &str) -> Result<Shape, String> {
        let field = |name: &str| {
            Field::parse(name).ok_or_else(|| format!("{seed}: shape names an unknown field {name:?}"))
        };
        let mut max_share = Vec::new();
        for (name, values) in &self.max_share {
            for (value, &fraction) in values {
                if !(0.0..=1.0).contains(&fraction) {
                    return Err(format!(
                        "{seed}: max_share {name} {value} must be a fraction, got {fraction}"
                    ));
                }
                max_share.push((field(name)?, value.clone(), fraction));
            }
        }
        let min_distinct = self
            .min_distinct
            .iter()
            .map(|(name, &n)| Ok((field(name)?, n)))
            .collect::<Result<_, String>>()?;
        let observed_pass = match self.observed.as_str() {
            "pass" => true,
            "FAIL" => false,
            other => return Err(format!("{seed}: shape observed must be pass or FAIL, got {other:?}")),
        };
        let shape = Shape {
            max_share,
            min_distinct,
            mutual: self.mutual,
            beats_plot: self.beats_plot,
            observed_pass,
        };
        if shape.max_share.is_empty()
            && shape.min_distinct.is_empty()
            && shape.mutual.is_none()
            && !shape.beats_plot
        {
            return Err(format!("{seed}: a shape with no assertion in it"));
        }
        Ok(shape)
    }
}

/// How much of a row its shape is read over: the first screenful `/index/similar` serves.
pub(crate) const SHAPE_K: usize = crate::handler::SIMILAR_PAGE;

/// What a case's shape is read from.
pub(crate) struct Seen<'a> {
    pub(crate) seed: Key,
    /// The row under test, best first.
    pub(crate) row: &'a [Key],
    /// The row of `row`'s first title, for `mutual`.
    pub(crate) best_row: &'a [Key],
    pub(crate) rail: Scores,
    pub(crate) plot: Scores,
}

/// The assertions of `shape` that do not hold, one line each; empty when the row has the shape asked for.
/// `values` is a title's values of a field.
pub(crate) fn shape_failures(
    shape: &Shape,
    seen: &Seen<'_>,
    values: &dyn Fn(Key, Field) -> Vec<String>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for (field, value, most) in &shape.max_share {
        let got = share(seen.row, SHAPE_K, |&key| values(key, *field).contains(value));
        if got > *most {
            failures.push(format!("max_share {} {value}: {got:.2} > {most:.2}", field.name()));
        }
    }
    for &(field, least) in &shape.min_distinct {
        let got = distinct(seen.row, SHAPE_K, |&key| values(key, field));
        if got < least {
            failures.push(format!("min_distinct {}: {got} < {least}", field.name()));
        }
    }
    if let Some(n) = shape.mutual {
        if seen.row.is_empty() {
            failures.push("mutual: the row is empty".to_owned());
        } else {
            match seen.best_row.iter().position(|&k| k == seen.seed) {
                Some(at) if at < n => {}
                at => failures.push(format!(
                    "mutual {n}: the seed is {} in its first title's row",
                    at.map_or_else(|| "absent".to_owned(), |at| format!("#{}", at + 1))
                )),
            }
        }
    }
    if shape.beats_plot && seen.rail.ndcg <= seen.plot.ndcg {
        failures.push(format!("beats_plot: nDCG {:.3} <= plot {:.3}", seen.rail.ndcg, seen.plot.ndcg));
    }
    failures
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

/// A case, resolved: the seed and its grades by title, of either type.
pub(crate) struct Resolved<'a> {
    pub(crate) case: &'a Case,
    pub(crate) media: MediaType,
    pub(crate) id: u32,
    pub(crate) grades: HashMap<Key, Grade>,
    pub(crate) shape: Option<Shape>,
}

type Key = (MediaType, u32);

/// The compiled-in judged set — both files — resolved once. It cannot fail at runtime in a built binary: the
/// test below resolves the same bytes, so a set that would not resolve fails the build's tests instead.
pub(crate) fn embedded() -> &'static [Resolved<'static>] {
    static JUDGED: OnceLock<(Judged, Judged)> = OnceLock::new();
    static CASES: OnceLock<Vec<Resolved<'static>>> = OnceLock::new();
    CASES.get_or_init(|| {
        let (judged, cross) = JUDGED.get_or_init(|| {
            (
                serde_json::from_str(EMBEDDED).expect("judged/rail.json parses (tested)"),
                serde_json::from_str(EMBEDDED_CROSS).expect("judged/rail-cross.json parses (tested)"),
            )
        });
        resolve(judged, Some(cross)).expect("the judged files resolve (tested)")
    })
}

/// Every case's keys parsed and checked, with the cross-type file's grades (films for a series seed, series
/// for a film) added to its seed's case. A malformed file is refused whole: scoring the cases that happen to
/// parse would report a number over a different set than the one on disk.
fn resolve<'a>(judged: &'a Judged, cross: Option<&'a Judged>) -> Result<Vec<Resolved<'a>>, String> {
    let mut seeds = HashSet::new();
    let mut resolved: Vec<Resolved<'a>> = judged
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
            add_grades(&mut grades, (media, id), case, |_| true)?;
            let shape = case.shape.as_ref().map(|s| s.parse(&case.seed)).transpose()?;
            Ok(Resolved { case, media, id, grades, shape })
        })
        .collect::<Result<_, String>>()?;
    for case in cross.map_or(&[][..], |c| c.cases.as_slice()) {
        let Some(at) = resolved.iter_mut().find(|r| r.case.seed == case.seed) else {
            return Err(format!("{}: a cross-type case for a seed the judged set lacks", case.seed));
        };
        if at.case.split != case.split {
            return Err(format!(
                "{}: split {:?} here, {:?} in the judged set",
                case.seed, case.split, at.case.split
            ));
        }
        let seed = (at.media, at.id);
        // The cross-type file holds only the other type, as its `about` says.
        add_grades(&mut at.grades, seed, case, |(media, _)| media != seed.0)?;
    }
    Ok(resolved)
}

/// A case's judgements into `grades`: refused when a key will not parse, names the seed itself, is not what
/// `allowed` admits, lacks a title or a basis, or is judged twice.
fn add_grades(
    grades: &mut HashMap<Key, Grade>,
    seed: Key,
    case: &Case,
    allowed: impl Fn(Key) -> bool,
) -> Result<(), String> {
    for j in &case.judged {
        let key = parse_key(&j.id).ok_or_else(|| format!("{}: bad key {:?}", case.seed, j.id))?;
        if key == seed || !allowed(key) {
            return Err(format!("{}: {} does not belong in this case", case.seed, j.id));
        }
        // A grade nobody can trace back to a reason cannot be argued with, only deleted.
        if j.basis.trim().is_empty() || j.title.trim().is_empty() {
            return Err(format!("{}: {} needs a title and a basis", case.seed, j.id));
        }
        if grades.insert(key, j.grade).is_some() {
            return Err(format!("{}: {} is judged twice", case.seed, j.id));
        }
    }
    Ok(())
}

fn title(indexes: &Indexes, media: MediaType, id: u32) -> String {
    let card = indexes.cards.as_ref().and_then(|c| c.get(&(media, id)));
    card.map_or_else(
        || id.to_string(),
        |c| c.year.map_or_else(|| c.title.clone(), |y| format!("{} ({y})", c.title)),
    )
}

/// The row scored for a seed: production's mixed row, as `/index/similar` serves it, or the one `params` ranks.
fn row_for(indexes: &Indexes, params: &den_index::SimilarParams, (media, id): Key) -> Vec<Key> {
    if *params == den_index::SimilarParams::default() {
        indexes.more_like_this_mixed(id, media).to_vec()
    } else {
        indexes.more_like_this_scored(id, media, params).iter().map(|s| s.key()).collect()
    }
}

/// The control arm: the plot index's nearest, which is what `/index/neighbours` answers.
fn plot_row(indexes: &Indexes, (media, id): Key) -> Vec<Key> {
    indexes
        .plot
        .nearest(id, media, den_index::MAX_ROW)
        .into_iter()
        .map(|n| (n.media_type, n.tmdb_id))
        .collect()
}

/// A title's values of a field, from its labels: a subgenre or mood counts only at the confidence the scorer
/// counts it at (`SimilarParams::min_confidence`). Nothing for a title neither index labels.
pub(crate) fn values_of(indexes: &Indexes, (media, id): Key, field: Field) -> Vec<String> {
    let labels =
        indexes.premise.as_ref().and_then(|p| p.labels(id, media)).or_else(|| indexes.plot.labels(id, media));
    let Some(labels) = labels else { return Vec::new() };
    let floor = den_index::SimilarParams::default().min_confidence;
    let confident = |pairs: &[(&str, f64)]| {
        pairs.iter().filter(|(_, c)| *c >= floor).map(|(n, _)| (*n).to_owned()).collect()
    };
    match field {
        Field::PrimaryGenre if labels.primary_genre.is_empty() => Vec::new(),
        Field::PrimaryGenre => vec![labels.primary_genre.to_owned()],
        Field::Subgenre => confident(&labels.subgenres),
        Field::Mood => confident(&labels.moods),
    }
}

/// The share of a row's first `SHAPE_K` carrying the seed's own primary genre (oxyc/den-atlas#24's single
/// number: 1.00 is a genre shelf). `None` for a seed with no primary genre.
fn genre_share(indexes: &Indexes, seed: Key, row: &[Key]) -> Option<f64> {
    let genre = values_of(indexes, seed, Field::PrimaryGenre).pop()?;
    Some(share(row, SHAPE_K, |&key| values_of(indexes, key, Field::PrimaryGenre).contains(&genre)))
}

/// A case's shape assertions that fail on this load, at `params`.
pub(crate) fn case_shape_failures(
    indexes: &Indexes,
    params: &den_index::SimilarParams,
    c: &Resolved<'_>,
    shape: &Shape,
) -> Vec<String> {
    let seed = (c.media, c.id);
    let row = row_for(indexes, params, seed);
    let best_row = row.first().map_or_else(Vec::new, |&best| row_for(indexes, params, best));
    let seen = Seen {
        seed,
        row: &row,
        best_row: &best_row,
        rail: score(&row, &c.grades, K),
        plot: score(&plot_row(indexes, seed), &c.grades, K),
    };
    shape_failures(shape, &seen, &|key, field| values_of(indexes, key, field))
}

fn line(label: &str, s: &Scores, plot: &Scores, shares: (Option<f64>, Option<f64>), shape: &str) {
    let share = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"));
    println!(
        "{label:<34} {:>6.3} {:>6.3} {:>5.2} {:>4} {:>4} {:>5}   {:>6.3} {:>6.3} {:>5}  {shape}",
        s.ndcg,
        s.condensed,
        s.precision,
        s.bad,
        s.judged,
        share(shares.0),
        plot.ndcg,
        plot.condensed,
        share(shares.1)
    );
}

/// Exit code, as the other subcommands return one.
pub async fn run(dir: &std::path::Path) -> i32 {
    let path = std::env::var("RAIL_JUDGED").unwrap_or_else(|_| "judged/rail.json".to_owned());
    let cross_path =
        std::env::var("RAIL_JUDGED_CROSS").unwrap_or_else(|_| "judged/rail-cross.json".to_owned());
    let read = |path: &str| -> Result<Judged, String> {
        std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            .map_err(|e| format!("{path}: {e}"))
    };
    // An empty RAIL_JUDGED_CROSS scores the seed type's grades alone.
    let loaded =
        read(&path).and_then(|j| Ok((j, (!cross_path.is_empty()).then(|| read(&cross_path)).transpose()?)));
    let (judged, cross) = match loaded {
        Ok(both) => both,
        Err(e) => {
            eprintln!("rail-eval: {e}");
            return 1;
        }
    };
    let cases = match resolve(&judged, cross.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rail-eval: {path} / {cross_path}: {e}");
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
        Some(cache_dir) => match crate::tmdb::Tmdb::new(dataset.mapped.clone(), Some(cache_dir), None, 0) {
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
        "{:<34} {:>6} {:>6} {:>5} {:>4} {:>4} {:>5}   {:>6} {:>6} {:>5}  shape",
        "seed", "nDCG", "nDCG'", "P", "bad", "jdg", "genre", "plot", "plot'", "genre"
    );
    println!("{}", "-".repeat(104));
    /// Per split: the rail's and the control's scores, and each one's genre share where the seed has a genre.
    #[derive(Default)]
    struct Half {
        rail: Vec<Scores>,
        plot: Vec<Scores>,
        rail_genre: Vec<f64>,
        plot_genre: Vec<f64>,
    }
    let mut by_split: HashMap<&str, Half> = HashMap::new();
    let mut gaps: Vec<String> = Vec::new();
    let mut shape_lines: Vec<String> = Vec::new();
    let (mut shape_pass, mut shape_total, mut shape_changed) = (0, 0, 0);
    for c in &cases {
        let seed = (c.media, c.id);
        let row = row_for(&indexes, &params, seed);
        let plot = plot_row(&indexes, seed);
        let (s, p) = (score(&row, &c.grades, K), score(&plot, &c.grades, K));
        let shares = (genre_share(&indexes, seed, &row), genre_share(&indexes, seed, &plot));
        let verdict = match &c.shape {
            None => String::new(),
            Some(shape) => {
                let failures = case_shape_failures(&indexes, &params, c, shape);
                shape_total += 1;
                // A case recorded as failing is a defect the set holds us to; one whose outcome moved either
                // way is news, and the file's `observed` wants updating with it.
                let changed = failures.is_empty() != shape.observed_pass;
                shape_changed += usize::from(changed);
                let tag = if changed {
                    "  CHANGED"
                } else if !failures.is_empty() {
                    "  (known)"
                } else {
                    ""
                };
                for f in &failures {
                    shape_lines.push(format!("  {}: {f}{tag}", c.case.title));
                }
                if failures.is_empty() {
                    shape_pass += 1;
                    format!("pass{tag}")
                } else {
                    format!("FAIL{tag}")
                }
            }
        };
        let name: String = c.case.title.chars().take(26).collect();
        line(&format!("{name} [{}]", c.case.split), &s, &p, shares, &verdict);
        if show.as_deref() == Some(c.case.seed.as_str()) {
            for (at, &(media, id)) in row.iter().take(K).enumerate() {
                let grade = match c.grades.get(&(media, id)) {
                    Some(Grade::Good) => "good",
                    Some(Grade::Ok) => "ok",
                    Some(Grade::Bad) => "bad",
                    None => "-",
                };
                println!("    {:>2}. {grade:<4} {}", at + 1, title(&indexes, media, id));
            }
        }
        for half in [c.case.split.as_str(), "all"] {
            let entry = by_split.entry(half).or_default();
            entry.rail.push(s);
            entry.plot.push(p);
            entry.rail_genre.extend(shares.0);
            entry.plot_genre.extend(shares.1);
        }
        if unjudged > 0 {
            let mut seen = HashSet::new();
            for (arm, list) in [("rail", &row), ("plot", &plot)] {
                for (at, &(media, id)) in list.iter().take(unjudged).enumerate() {
                    if !c.grades.contains_key(&(media, id)) && seen.insert((media, id)) {
                        let key = match media {
                            MediaType::Movie => format!("movie:{id}"),
                            MediaType::Tv => format!("series:{id}"),
                        };
                        gaps.push(format!(
                            "{} <- {arm} #{}: {{\"id\": \"{key}\", \"title\": {:?}, \"grade\": \"\", \"basis\": \"\"}}",
                            c.case.seed,
                            at + 1,
                            title(&indexes, media, id)
                        ));
                    }
                }
            }
        }
    }
    // A seed the judged set does not hold is shown too, ungraded.
    if let Some((media, id)) = show.as_deref().and_then(parse_key) {
        if !cases.iter().any(|c| (c.media, c.id) == (media, id)) {
            println!("{} (not judged):", title(&indexes, media, id));
            let row = row_for(&indexes, &params, (media, id));
            for (at, &(kind, other)) in row.iter().take(K).enumerate() {
                println!("    {:>2}. {}", at + 1, title(&indexes, kind, other));
            }
        }
    }
    println!("{}", "-".repeat(104));
    let average = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    for half in ["dev", "test", "all"] {
        if let Some(h) = by_split.get(half) {
            let shares = (average(&h.rail_genre), average(&h.plot_genre));
            line(&format!("MEAN {half} (n={})", h.rail.len()), &mean(&h.rail), &mean(&h.plot), shares, "");
        }
    }
    println!("\nnDCG' ignores unjudged titles; bad and jdg are totals over the cases, of {K} per case.");
    println!(
        "genre is the share of the first {SHAPE_K} carrying the seed's own primary genre: 1.00 is a genre shelf. \
         Read it beside nDCG; lowering one by breaking the other is not an improvement."
    );
    println!("shape: {shape_pass} of {shape_total} pass, {shape_changed} changed from the file's `observed`");
    for l in &shape_lines {
        println!("{l}");
    }
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
        assert!(cases.iter().any(|c| c.shape.is_some()), "the shipped set carries shape assertions");
    }

    fn tv(id: u32) -> Key {
        (MediaType::Tv, id)
    }

    /// Titles 1..=99 are Crime Police Procedurals, 100.. Drama, 200.. Comedy.
    fn values((_, id): Key, field: Field) -> Vec<String> {
        let (genre, subgenre) = match id {
            0..=99 => ("Crime", "Police Procedural"),
            100..=199 => ("Drama", "Political"),
            _ => ("Comedy", "Sitcom"),
        };
        match field {
            Field::PrimaryGenre => vec![genre.to_owned()],
            Field::Subgenre => vec![subgenre.to_owned()],
            Field::Mood => Vec::new(),
        }
    }

    fn shape(json: &str) -> Shape {
        serde_json::from_str::<ShapeSpec>(json).unwrap().parse("series:1").unwrap()
    }

    fn seen<'a>(row: &'a [Key], best_row: &'a [Key]) -> Seen<'a> {
        Seen { seed: tv(1000), row, best_row, rail: Scores::default(), plot: Scores::default() }
    }

    /// The Wire's #23 row — twenty crime procedurals — fails every genre assertion; a row that is half crime
    /// and reaches three genres passes them; and a share exactly at the limit is allowed.
    #[test]
    fn a_genre_shelf_fails_the_share_and_distinct_assertions_and_a_varied_row_passes() {
        let wire = shape(
            r#"{"max_share": {"primaryGenre": {"Crime": 0.8}, "subgenre": {"Police Procedural": 0.5}},
                "min_distinct": {"primaryGenre": 3}, "observed": "FAIL"}"#,
        );
        let shelf: Vec<Key> = (1..=20).map(tv).collect();
        let failures = shape_failures(&wire, &seen(&shelf, &[]), &values);
        assert_eq!(failures.len(), 3, "{failures:?}");
        let varied: Vec<Key> = (1..=10).chain(100..105).chain(200..205).map(tv).collect();
        assert!(shape_failures(&wire, &seen(&varied, &[]), &values).is_empty());
        let at_limit: Vec<Key> = (1..=16).chain(100..102).chain(200..202).map(tv).collect();
        let crime = shape(r#"{"max_share": {"primaryGenre": {"Crime": 0.8}}, "observed": "pass"}"#);
        assert!(shape_failures(&crime, &seen(&at_limit, &[]), &values).is_empty(), "16 of 20 is 0.8");
        // Only the first screenful counts: a shelf past it is not the visible row.
        let late: Vec<Key> = varied.iter().copied().chain((21..=60).map(tv)).collect();
        assert!(shape_failures(&wire, &seen(&late, &[]), &values).is_empty());
    }

    /// The seed must sit within the first n of its best neighbour's row: at n it passes, one past and absent
    /// fail, and a seed with no row at all has no best neighbour to ask.
    #[test]
    fn mutual_holds_the_seed_to_its_best_neighbours_first_n() {
        let mutual = shape(r#"{"mutual": 3, "observed": "pass"}"#);
        let row = [tv(1), tv(2)];
        let at = |position: usize| -> Vec<Key> {
            let mut best: Vec<Key> = (500..510).map(tv).collect();
            best.insert(position, tv(1000));
            best
        };
        assert!(shape_failures(&mutual, &seen(&row, &at(2)), &values).is_empty(), "third is within 3");
        assert_eq!(
            shape_failures(&mutual, &seen(&row, &at(3)), &values),
            ["mutual 3: the seed is #4 in its first title's row"]
        );
        let absent: Vec<Key> = (500..510).map(tv).collect();
        assert_eq!(
            shape_failures(&mutual, &seen(&row, &absent), &values),
            ["mutual 3: the seed is absent in its first title's row"]
        );
        assert_eq!(shape_failures(&mutual, &seen(&[], &[]), &values), ["mutual: the row is empty"]);
    }

    /// The served row must do strictly better than the plot-only control: a tie has not beaten it.
    #[test]
    fn beats_plot_needs_a_strictly_better_ndcg_than_the_control() {
        let beats = shape(r#"{"beats_plot": true, "observed": "pass"}"#);
        let scored = |rail: f64, plot: f64| Seen {
            rail: Scores { ndcg: rail, ..Scores::default() },
            plot: Scores { ndcg: plot, ..Scores::default() },
            ..seen(&[], &[])
        };
        assert!(shape_failures(&beats, &scored(0.7, 0.3), &values).is_empty());
        assert_eq!(shape_failures(&beats, &scored(0.3, 0.3), &values).len(), 1);
        assert_eq!(shape_failures(&beats, &scored(0.2, 0.3), &values).len(), 1);
        // Off, the same scores ask nothing.
        let off = shape(r#"{"mutual": 20, "observed": "pass"}"#);
        let (row, best_row) = ([tv(1)], [tv(1000)]);
        let losing = Seen { row: &row, best_row: &best_row, ..scored(0.2, 0.3) };
        assert!(shape_failures(&off, &losing, &values).is_empty());
    }

    /// A shape that could only ever pass by not being read is refused: a field atlas does not know, a key the
    /// file does not define, a fraction outside 0..=1, an unknown outcome, or no assertion at all.
    #[test]
    fn a_shape_that_cannot_be_read_as_written_is_refused() {
        let refused = |json: &str| -> String {
            match serde_json::from_str::<ShapeSpec>(json) {
                Err(e) => e.to_string(),
                Ok(spec) => spec.parse("series:1").unwrap_err(),
            }
        };
        assert!(refused(r#"{"max_share": {"genre": {"Crime": 0.8}}, "observed": "pass"}"#).contains("genre"));
        assert!(refused(r#"{"max_shares": {}, "observed": "pass"}"#).contains("max_shares"));
        assert!(refused(r#"{"max_share": {"mood": {"Dark": 1.5}}, "observed": "pass"}"#).contains("fraction"));
        assert!(refused(r#"{"mutual": 5, "observed": "fail"}"#).contains("pass or FAIL"));
        assert!(refused(r#"{"observed": "pass"}"#).contains("no assertion"));
    }

    /// On the real corpus, opt-in like every such test (`DEN_STORE`): each case's shape holds or fails exactly
    /// as its `observed` says, on the dataset `about.shape.observedOn` names — so a change that breaks a
    /// passing shape, or mends a failing one, has to say so in the file. And the genre shelf the assertions
    /// exist for is still caught: The Wire's row from the premise-only scorer the pooled one replaced
    /// (`den_index::more_like_this`, 20 of 20 Crime in #23) fails them.
    ///
    /// Ranked without character links, as `rail-eval` ranks without `CACHE_DIR`.
    #[test]
    fn every_shape_is_as_observed_and_the_old_genre_shelf_fails() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to exercise this");
            return;
        };
        let about: serde_json::Value = serde_json::from_str(EMBEDDED).unwrap();
        let observed_on = about["about"]["shape"]["observedOn"].as_str().expect("about.shape.observedOn");
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        if ds.meta.dataset_version != observed_on {
            eprintln!(
                "SKIP: shapes were observed on {observed_on}, this store is {}",
                ds.meta.dataset_version
            );
            return;
        }
        let indexes = crate::queries::load_for_tools(&ds).expect("the indexes load");
        let production = den_index::SimilarParams::default();
        let mut moved = Vec::new();
        for c in embedded() {
            let Some(shape) = &c.shape else { continue };
            let failures = case_shape_failures(&indexes, &production, c, shape);
            if failures.is_empty() != shape.observed_pass {
                let was = if shape.observed_pass { "pass" } else { "FAIL" };
                moved.push(format!("{}: observed {was}, now failing {failures:?}", c.case.seed));
            }
        }
        assert!(moved.is_empty(), "shapes moved from `observed`: {moved:#?}");

        let wire = embedded().iter().find(|c| c.case.seed == "series:1438").expect("The Wire is judged");
        let old = |id: u32| -> Vec<Key> {
            den_index::more_like_this(Some(&indexes.plot), indexes.premise.as_ref(), id, MediaType::Tv)
                .into_iter()
                .map(tv)
                .collect()
        };
        let row = old(1438);
        let best_row = row.first().map_or_else(Vec::new, |&(_, id)| old(id));
        let seen = Seen {
            seed: tv(1438),
            row: &row,
            best_row: &best_row,
            rail: score(&row, &wire.grades, K),
            plot: score(&plot_row(&indexes, tv(1438)), &wire.grades, K),
        };
        let failures = shape_failures(wire.shape.as_ref().unwrap(), &seen, &|key, field| {
            values_of(&indexes, key, field)
        });
        assert!(
            failures.iter().any(|f| f.starts_with("max_share primaryGenre Crime")),
            "the premise-only row is no longer a crime shelf: {failures:?}"
        );
    }
}
