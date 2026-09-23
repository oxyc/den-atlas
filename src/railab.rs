//! A/B the shipped More Like This scorer against the pooled one, on the real shipped corpus.
//!
//!   DATA_DIR=<dataset dir> den-atlas rail-ab <dataset dir>
//!   RAIL_SHOW="The Wire" den-atlas rail-ab <dataset dir>     # …and print that anchor's row in full
//!
//! Reports, per anchor and in aggregate, the two numbers that describe the SHAPE of a row — the share of
//! the twenty carrying the anchor's own primary genre, and the share carrying one single subgenre. Those
//! are the measurable form of "it reads as a genre shelf", which no per-title assertion can express.
//!
//! # Why this is a subcommand and not an example
//!
//! It was `den-index/examples/rail_ab.rs`, and it read four files out of a producer out-dir: the facts
//! JSON, a `jev-facets.json` rail sidecar, labels and vectors. Two of those are no longer published, so it
//! panicked on any current generation — while still COMPILING, because CI builds examples. A tuning tool
//! nobody can start is the same as one that does not exist, and that had now happened to this file twice.
//!
//! Everything it parsed is in the store, and serving's readers of it are `den_index::SeedFacets` and
//! `den_index::SeedAuthorship`, over the store serving maps. About two hundred lines of bespoke JSON
//! parsing went with the move, and with them the risk that mattered most: the harness built its own facets
//! and its own authorship, by hand, from a different file than serving reads. It now measures what serving
//! actually does, through the same code.

use crate::queries::Indexes;
use den_index::{
    more_like_this, more_like_this_pooled, Authorship, Facets, Index, MediaType, SeedAuthorship, SeedFacets,
};
use std::collections::HashMap;

/// Anchors chosen to cover the reported defects and the cases the rail already gets right, so a change
/// that only helps The Wire is visible as such.
const ANCHORS: &[(&str, u32, MediaType)] = &[
    ("The Wire", 1438, MediaType::Tv),
    ("Succession", 76331, MediaType::Tv),
    ("Chernobyl", 87108, MediaType::Tv),
    ("Breaking Bad", 1396, MediaType::Tv),
    ("Fleabag", 67070, MediaType::Tv),
    ("Russian Doll", 84977, MediaType::Tv),
    ("Veep", 2947, MediaType::Tv),
    ("True Detective", 46648, MediaType::Tv),
    ("Spotlight", 314365, MediaType::Movie),
    ("The Insider", 9008, MediaType::Movie),
    ("Groundhog Day", 137, MediaType::Movie),
    ("Palm Springs", 587792, MediaType::Movie),
    ("Spirited Away", 129, MediaType::Movie),
    ("Inside Out", 150540, MediaType::Movie),
    ("Paddington", 116149, MediaType::Movie),
    ("Once", 5723, MediaType::Movie),
    ("Angel", 2426, MediaType::Tv),
    ("Oz", 3322, MediaType::Tv),
    // A pair the rail already gets right, both ways round: a regression guard, not a defect. The premise
    // index ranks each the other's #1 while plot ranks them 166th and 43rd — premise earning its place.
    ("Love Again", 758336, MediaType::Movie),
    ("Voicemails for Isabelle", 614945, MediaType::Movie),
];

/// Titles a viewer would expect in an anchor's row.
const WANTED: &[(u32, &[(&str, u32)])] = &[
    (
        1438,
        &[
            ("We Own This City", 125949),
            ("Homicide", 4464),
            ("Show Me a Hero", 63248),
            ("The Corner", 14531),
            ("Oz", 3322),
            ("Deadwood", 1406),
            ("The Deuce", 65817),
            ("Treme", 17967),
            ("Generation Kill", 17035),
        ],
    ),
    (314365, &[("The Post", 446354), ("She Said", 837881)]),
    (137, &[("Palm Springs", 587792)]),
    (1396, &[("Better Call Saul", 60059)]),
    (5723, &[("Begin Again", 198277), ("Sing Street", 369557), ("Flora and Son", 1059811)]),
    (3322, &[("The Wire", 1438)]),
    (758336, &[("Voicemails for Isabelle", 614945)]),
    (614945, &[("Love Again", 758336)]),
];

/// …and ones it should not.
const UNWANTED: &[(u32, &[(&str, u32)])] =
    &[(1438, &[("Bates Motel", 46786)]), (76331, &[("Dynasty 1981", 3769), ("Dallas", 6647)])];

fn listed<'a>(table: &'a [(u32, &'a [(&'a str, u32)])], id: u32) -> &'a [(&'a str, u32)] {
    table.iter().find(|(anchor, _)| *anchor == id).map_or(&[][..], |(_, list)| list)
}

/// The share of a row carrying the anchor's own primary genre, and the largest share carrying one subgenre.
///
/// Expect the pooled arm's subgenre share to read 15% on nearly every anchor, and do not read that as a
/// broken measurement: `more_like_this_pooled` caps any one subgenre at `SUBGENRE_CAP` (3) within the
/// first `KEEP` (20), so 3/20 is the ceiling and it binds almost everywhere. The number is still worth
/// printing — it says the cap is what is shaping the row, and an anchor BELOW 15% is one where the pooled
/// row ran out of same-subgenre candidates on its own.
fn shape(index: &Index, ids: &[u32], media: MediaType, seed_genre: &str) -> (f64, f64) {
    if ids.is_empty() {
        return (0.0, 0.0);
    }
    let mut same_genre = 0usize;
    let mut sub: HashMap<String, usize> = HashMap::new();
    for &id in ids {
        if let Some(l) = index.labels(id, media) {
            if l.primary_genre == seed_genre {
                same_genre += 1;
            }
            if let Some((n, _)) = l
                .subgenres
                .iter()
                .filter(|(_, c)| *c >= 0.55)
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                *sub.entry((*n).to_string()).or_insert(0) += 1;
            }
        }
    }
    let top = sub.into_values().max().unwrap_or(0);
    (same_genre as f64 / ids.len() as f64, top as f64 / ids.len() as f64)
}

fn title(index: &Index, id: u32, media: MediaType) -> String {
    index.labels(id, media).map_or_else(|| format!("{id}"), |l| format!("{id} [{}]", l.primary_genre))
}

/// One anchor's pooled row, built the ONE way — the reporting loop and `RAIL_SHOW` both call this.
///
/// An independently constructed copy printed a different row than the one being measured, which is worse
/// than no print at all. The seed's own type only: the shipped scorer it is compared with never mixes.
fn pooled(indexes: &Indexes, id: u32, media: MediaType) -> Option<Vec<u32>> {
    let view = indexes.store.view();
    let facets = SeedFacets::new(&view, &indexes.store.aggregates, media).ok()?;
    let authorship = SeedAuthorship::of(&view, media, id).ok()?;
    let row = more_like_this_pooled(
        Some(&indexes.plot),
        indexes.premise.as_ref(),
        id,
        media,
        Some(&authorship as &dyn Authorship),
        Some(&facets as &dyn Facets),
    );
    Some(row.into_iter().filter(|&(kind, _)| kind == media).map(|(_, id)| id).collect())
}

/// Exit code, as the other subcommands return one.
pub fn run(dir: &std::path::Path) -> i32 {
    let dataset = match crate::dataset::Dataset::load(dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("rail-ab: dataset at {} will not load: {e}", dir.display());
            return 1;
        }
    };
    // The serving loader, not a second one. The harness used to build its own facets and authorship by
    // hand out of files serving does not read, so every number it produced was about the harness's
    // reading of the corpus rather than about the rail.
    let indexes = match crate::queries::load_for_tools(&dataset) {
        Ok(indexes) => indexes,
        Err(e) => {
            eprintln!("rail-ab: {e}");
            return 1;
        }
    };
    // No store-less arm to guard against any more: `load_for_tools` fails without one.
    let view = indexes.store.view();

    println!(
        "{:<24} {:>9} {:>9}   {:>9} {:>9}   {:>4} {:>4}",
        "anchor", "A genre", "A subgen", "B genre", "B subgen", "A n", "B n"
    );
    println!("{}", "-".repeat(90));
    let (mut ag, mut asg, mut bg, mut bsg, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut notes: Vec<String> = Vec::new();
    // The scalar a weight sweep is judged on. Genre share is a SHAPE, and the pooled scorer lowers it on
    // purpose, so "higher is better" is false for it — sweeping against it would tune the rail back into
    // the genre shelf it exists to stop being. Where the titles a viewer would actually expect END UP is
    // the thing a weight is for: Once is supposed to pull in the other three John Carney films, The Wire
    // the rest of David Simon's Baltimore. A miss counts as one past the end of the row, so dropping a
    // wanted title always costs more than ranking it last.
    let (mut want_rank_a, mut want_rank_b, mut want_n) = (0.0, 0.0, 0.0);
    let (mut bad_rank_a, mut bad_rank_b, mut bad_n) = (0.0, 0.0, 0.0);
    let miss = (den_index::MAX_ROW + 1) as f64;
    // The COUNTERWEIGHT, and the reason `want mean rank` cannot be swept on alone.
    //
    // Every title in `WANTED` is there because a viewer would expect it, and most of them are expected
    // BECAUSE they share a maker: four John Carney films, six David Simon shows. So raising `W_MAKER`
    // improves that number by construction, and a sweep against it alone recommends raising the weight
    // for ever — measured, monotonically, from 0.0 to 2.6 without turning.
    //
    // This is what raising it costs: the share of the visible twenty that shares a maker with the seed.
    // Past some point the row stops being "more like this" and becomes "more by this person", which is a
    // different row the detail screen already has.
    let (mut auth_share_a, mut auth_share_b) = (0.0, 0.0);

    for &(name, id, media) in ANCHORS {
        let Some(seed) = indexes
            .premise
            .as_ref()
            .and_then(|p| p.labels(id, media))
            .or_else(|| indexes.plot.labels(id, media))
        else {
            println!("{name:<24}  NOT IN EITHER INDEX");
            continue;
        };
        let genre = seed.primary_genre.to_string();
        let a = more_like_this(Some(&indexes.plot), indexes.premise.as_ref(), id, media);
        let Some(b_full) = pooled(&indexes, id, media) else {
            println!("{name:<24}  NO POOLED ROW");
            continue;
        };
        // Shape is judged on the visible screenful, not the whole scrollable row, so the numbers stay
        // comparable with the shipped scorer's twenty.
        let b: Vec<u32> = b_full.iter().copied().take(20).collect();
        let (a_g, a_s) = shape(&indexes.plot, &a, media, &genre);
        let (b_g, b_s) = shape(&indexes.plot, &b, media, &genre);
        println!(
            "{name:<24} {:>8.0}% {:>8.0}%   {:>8.0}% {:>8.0}%   {:>4} {:>4}",
            a_g * 100.0,
            a_s * 100.0,
            b_g * 100.0,
            b_s * 100.0,
            a.len(),
            b_full.len()
        );
        ag += a_g;
        asg += a_s;
        bg += b_g;
        bsg += b_s;
        n += 1.0;

        // Share of the visible twenty crediting one of the seed's own makers.
        let by_same_maker = |row: &[u32]| -> f64 {
            let Ok(authorship) = SeedAuthorship::of(&view, media, id) else { return 0.0 };
            if row.is_empty() {
                return 0.0;
            }
            let hits = row.iter().take(20).filter(|&&other| authorship.makers((media, other)) > 0.0).count();
            hits as f64 / row.len().min(20) as f64
        };
        auth_share_a += by_same_maker(&a);
        auth_share_b += by_same_maker(&b);

        let rank = |row: &[u32], want: u32| row.iter().position(|x| *x == want).map(|p| (p + 1) as f64);
        let shown = |r: Option<f64>| r.map_or("-".to_owned(), |p| (p as usize).to_string());
        for &(label, want) in listed(WANTED, id) {
            let (ra, rb) = (rank(&a, want), rank(&b_full, want));
            want_rank_a += ra.unwrap_or(miss);
            want_rank_b += rb.unwrap_or(miss);
            want_n += 1.0;
            if shown(ra) != shown(rb) {
                notes.push(format!("  want {label:<24} {name:<24} A={:<4} B={}", shown(ra), shown(rb)));
            }
        }
        for &(label, bad) in listed(UNWANTED, id) {
            let (ra, rb) = (rank(&a, bad), rank(&b_full, bad));
            // Inverted: for a title that should NOT be there, further down is better and absent is best.
            bad_rank_a += ra.unwrap_or(miss);
            bad_rank_b += rb.unwrap_or(miss);
            bad_n += 1.0;
            if shown(ra) != shown(rb) {
                notes.push(format!("  DROP {label:<24} {name:<24} A={:<4} B={}", shown(ra), shown(rb)));
            }
        }
    }

    println!("{}", "-".repeat(90));
    if n > 0.0 {
        println!(
            "{:<24} {:>8.0}% {:>8.0}%   {:>8.0}% {:>8.0}%",
            "MEAN",
            ag / n * 100.0,
            asg / n * 100.0,
            bg / n * 100.0,
            bsg / n * 100.0
        );
    }
    // One line a sweep can be read off. Lower is better on `want`, higher on `drop`.
    if want_n > 0.0 {
        println!(
            "\nwant mean rank  A={:>6.1}  B={:>6.1}   (of {want_n:.0}, a miss counts {miss:.0})",
            want_rank_a / want_n,
            want_rank_b / want_n
        );
    }
    if bad_n > 0.0 {
        println!(
            "drop mean rank  A={:>6.1}  B={:>6.1}   (of {bad_n:.0}, higher is better, {miss:.0} = absent)",
            bad_rank_a / bad_n,
            bad_rank_b / bad_n
        );
    }
    if n > 0.0 {
        println!(
            "same-maker share  A={:>5.0}%  B={:>5.0}%   (of the visible 20 — the cost of W_MAKER)",
            auth_share_a / n * 100.0,
            auth_share_b / n * 100.0
        );
    }
    println!("\nmoves:");
    for line in &notes {
        println!("{line}");
    }

    if let Ok(want) = std::env::var("RAIL_SHOW") {
        if let Some(&(name, id, media)) = ANCHORS.iter().find(|a| a.0 == want) {
            if let Some(row) = pooled(&indexes, id, media) {
                println!("\n{name}, pooled scorer:");
                for (i, id) in row.iter().enumerate().take(20) {
                    println!("  {:>2}. {}", i + 1, title(&indexes.plot, *id, media));
                }
            }
        } else {
            eprintln!("rail-ab: RAIL_SHOW={want:?} is not one of the anchors");
        }
    }
    0
}
