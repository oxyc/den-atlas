//! Slate checks for the billboard (`/recommend`), over made-up households kept in `fixtures/billboard/`.
//!
//!   CACHE_DIR=<dir> den-atlas billboard-check <dataset dir>
//!   BILLBOARD_FIXTURES=<dir> CACHE_DIR=<dir> den-atlas billboard-check <dataset dir>   # other households
//!
//! Each fixture is a `den-atlas replay` file (`recommend::fixture`): a household's request, the service lists it was
//! ranked with and the moment it was ranked at. The households are invented — one taste each, on three services in
//! one country — because the repository is public and a real library is not ours to publish. The lists carry no
//! rating: TMDB's scores are kept on the box only (`tmdb.rs`), so this reads them from `CACHE_DIR` and downloads
//! TMDB's daily exports exactly as serving does, and the numbers move a little from day to day with them.
//!
//! Nothing here is a judgement of whether a slide is good. These are properties of the slate as a whole that a
//! viewer notices without knowing the household, each read over the first `LEAD` slides — the ones a billboard
//! actually shows before anyone scrolls:
//!
//! - **catalogue** — slides that are neither new nor newly arrived (`recommend::stale`).
//! - **low quality** — slides whose `quality` is under `recommend::POOR_QUALITY`.
//! - **films / series** — the mix, on a surface that shows both.
//! - **zero buzz** — slides nothing says anyone is watching.
//! - **stacked** — the most slides that sit within `recommend::SAME_INTEREST` of one slide's plot, itself included:
//!   several slides from one interest.
//! - **overlap** — slides a household shares with each other household. Different tastes on the same services
//!   should not get the same billboard.
//!
//! The exit code is 1 when any household fails a threshold, so a scorer change that brings a problem back fails
//! the run.

use crate::queries::Indexes;
use crate::recommend;
use den_index::MediaType;

/// The slides each check reads: a billboard's first screenful and what the TV rotates through first.
const LEAD: usize = 10;

/// The thresholds a household must meet, each out of `LEAD`.
///
/// - At most 3 catalogue slides: the billboard is for what is new; three old titles that fit very well are room
///   for the household's own tastes without the slate turning into its back catalogue (6–7 did before).
/// - At most 1 low-quality slide: a poorly received release may still earn one place on fit and timeliness; four
///   did before.
/// - At least 3 of each type on a surface showing both: nobody's billboard should be nine films and a series.
/// - At most 3 slides with no buzz at all: five to seven had none before, because the popularity signal read only
///   client hints.
/// - At most 2 stacked slides: a sequel beside its original may pass where keeping them apart would cost too much
///   (`recommend::RULE_COST`), but not three of one interest.
/// - At most 3 slides shared with any other household: these tastes barely touch, so more than that is the
///   services' lists speaking rather than the household.
const MAX_CATALOGUE: usize = 3;
const MAX_LOW_QUALITY: usize = 1;
const MIN_EACH_TYPE: usize = 3;
const MAX_ZERO_BUZZ: usize = 3;
const MAX_STACKED: usize = 2;
const MAX_OVERLAP: usize = 3;

/// One household's slate, measured.
#[derive(Debug, Default, PartialEq)]
struct Slate {
    catalogue: usize,
    low_quality: usize,
    films: usize,
    series: usize,
    zero_buzz: usize,
    stacked: usize,
    /// The lowest fit among the lead: what the rules above cost in taste. Printed, not checked.
    lowest_fit: f64,
    /// The whole answer's films and series, beyond the lead.
    all_films: usize,
    all_series: usize,
}

/// The checks over one answer's slides. `near` says whether two slides' plots are within `SAME_INTEREST`.
fn measure(
    slides: &[serde_json::Value],
    near: impl Fn(&serde_json::Value, &serde_json::Value) -> bool,
) -> Slate {
    let lead = &slides[..slides.len().min(LEAD)];
    let term = |slide: &serde_json::Value, name: &str| slide["why"][name].as_f64().unwrap_or(0.0);
    let count = |keep: &dyn Fn(&serde_json::Value) -> bool| lead.iter().filter(|s| keep(s)).count();
    let film = |slide: &serde_json::Value| slide["type"] == "movie";
    Slate {
        catalogue: count(&|s| {
            term(s, "fresh") < recommend::STALE_FRESH && term(s, "arrived") < recommend::STALE_ARRIVAL
        }),
        low_quality: count(&|s| term(s, "quality") < recommend::POOR_QUALITY),
        films: count(&film),
        series: count(&|s| !film(s)),
        zero_buzz: count(&|s| term(s, "buzz") <= 0.0),
        stacked: lead.iter().map(|a| lead.iter().filter(|b| near(a, b)).count()).max().unwrap_or(0),
        lowest_fit: lead.iter().map(|s| term(s, "fit")).fold(f64::INFINITY, f64::min),
        all_films: slides.iter().filter(|s| film(s)).count(),
        all_series: slides.iter().filter(|s| !film(s)).count(),
    }
}

/// What a slate fails, as words; empty when it passes. `both` is whether its surface shows both types.
fn failures(slate: &Slate, both: bool) -> Vec<String> {
    let mut failed = Vec::new();
    let mut check = |ok: bool, what: String| {
        if !ok {
            failed.push(what);
        }
    };
    check(slate.catalogue <= MAX_CATALOGUE, format!("{} catalogue > {MAX_CATALOGUE}", slate.catalogue));
    check(
        slate.low_quality <= MAX_LOW_QUALITY,
        format!("{} low quality > {MAX_LOW_QUALITY}", slate.low_quality),
    );
    if both {
        check(
            slate.films.min(slate.series) >= MIN_EACH_TYPE,
            format!("mix {}/{}", slate.films, slate.series),
        );
    }
    check(slate.zero_buzz <= MAX_ZERO_BUZZ, format!("{} zero buzz > {MAX_ZERO_BUZZ}", slate.zero_buzz));
    check(slate.stacked <= MAX_STACKED, format!("{} stacked > {MAX_STACKED}", slate.stacked));
    failed
}

fn key(slide: &serde_json::Value) -> Option<(MediaType, u32)> {
    let media = if slide["type"] == "movie" { MediaType::Movie } else { MediaType::Tv };
    Some((media, slide["id"].as_u64()?.try_into().ok()?))
}

/// Whether two slides' plots are within `SAME_INTEREST` of each other, by the plot index.
fn plots_near(indexes: &Indexes, a: &serde_json::Value, b: &serde_json::Value) -> bool {
    let row = |slide| key(slide).and_then(|(media, id)| indexes.plot.row_of(id, media));
    match (row(a), row(b)) {
        (Some(a), Some(b)) => a == b || indexes.plot.similarity(a, b) >= recommend::SAME_INTEREST,
        _ => key(a) == key(b),
    }
}

fn name(indexes: &Indexes, slide: &serde_json::Value) -> String {
    let card = key(slide).and_then(|key| indexes.cards.as_ref()?.get(&key));
    card.map_or_else(|| slide["id"].to_string(), |c| c.title.clone())
}

/// Exit code, as the other subcommands return one.
pub async fn run(dir: &std::path::Path) -> i32 {
    let fixtures = std::env::var("BILLBOARD_FIXTURES").unwrap_or_else(|_| "fixtures/billboard".to_owned());
    let mut paths: Vec<std::path::PathBuf> = match std::fs::read_dir(&fixtures) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect(),
        Err(e) => {
            eprintln!("billboard-check: {fixtures}: {e}");
            return 1;
        }
    };
    paths.sort();
    let dataset = match crate::dataset::Dataset::load(dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("billboard-check: dataset at {} will not load: {e}", dir.display());
            return 1;
        }
    };
    // TMDB's kept vote counts and credits and its daily exports, read as serving reads them: the checks are about
    // the scorer as it runs in production, which has all three. The counts and credits are whatever `CACHE_DIR`
    // keeps — the box's, or a seed (`scripts/tmdb-seed.py`); this asks TMDB for nothing.
    let cache_dir = std::env::var("CACHE_DIR").ok().filter(|d| !d.is_empty()).map(std::path::PathBuf::from);
    let Some(cache_dir) = cache_dir else {
        eprintln!("billboard-check: CACHE_DIR names no directory of kept TMDB numbers (tmdb-votes.tsv)");
        return 1;
    };
    let tmdb = match crate::tmdb::Tmdb::new(dataset.store.clone(), Some(cache_dir), None, 0) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("billboard-check: {e}");
            return 1;
        }
    };
    eprintln!("{}", tmdb.load().await);
    let export = match crate::titles::TitleSearch::new(crate::titles::EXPORT_BASE) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("billboard-check: {e}");
            return 1;
        }
    };
    match export.refresh().await {
        Ok(line) => eprintln!("{line}"),
        Err(e) => {
            eprintln!("billboard-check: {e}");
            return 1;
        }
    }
    let queries = crate::queries::IndexQueries::new(&dataset)
        .with_ratings(Some(tmdb.ratings()))
        .with_characters(Some(tmdb.characters()));
    let indexes = match queries.get(|| ()).await {
        Ok((indexes, _)) => indexes,
        Err(e) => {
            eprintln!("billboard-check: {e}");
            return 1;
        }
    };
    let export = export.index();

    println!(
        "dataset {}  ·  {} households from {fixtures}  ·  first {LEAD} slides",
        indexes.dataset_version,
        paths.len()
    );
    println!(
        "{:<12} {:>5} {:>5} {:>11} {:>5} {:>7} {:>7} {:>11}  failed",
        "household", "cat", "lowq", "films/ser", "buzz0", "stacked", "min fit", "all f/s"
    );
    let mut failed = false;
    let mut leads: Vec<(String, Vec<(MediaType, u32)>)> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    for path in &paths {
        let label = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let fixture = std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).map_err(|e| e.to_string()));
        let (request, lists, now) = match fixture.and_then(|f| recommend::replayed(&f)) {
            Ok(replayed) => replayed,
            Err(e) => {
                eprintln!("billboard-check: {}: {e}", path.display());
                return 1;
            }
        };
        let answer = recommend::answer(&indexes, export.as_deref(), &request, &lists, now);
        let slides = answer["slides"].as_array().map(Vec::as_slice).unwrap_or_default();
        let slate = measure(slides, |a, b| plots_near(&indexes, a, b));
        let both = request.surface.as_deref().is_none_or(|s| s == "home");
        let why = failures(&slate, both);
        failed |= !why.is_empty();
        println!(
            "{label:<12} {:>5} {:>5} {:>11} {:>5} {:>7} {:>7.2} {:>11}  {}",
            slate.catalogue,
            slate.low_quality,
            format!("{}/{}", slate.films, slate.series),
            slate.zero_buzz,
            slate.stacked,
            slate.lowest_fit,
            format!("{}/{}", slate.all_films, slate.all_series),
            if why.is_empty() { "-".to_owned() } else { why.join(", ") }
        );
        shown.push(format!("{label}:"));
        for (at, slide) in slides.iter().take(LEAD).enumerate() {
            shown.push(format!("  {:>2}. {}", at + 1, recommend::describe(&indexes, slide)));
        }
        leads.push((label, slides.iter().take(LEAD).filter_map(key).collect()));
    }
    println!("\noverlap (first {LEAD} slides shared, at most {MAX_OVERLAP}):");
    for (i, (a, lead_a)) in leads.iter().enumerate() {
        for (b, lead_b) in &leads[i + 1..] {
            let shared: Vec<_> = lead_a.iter().filter(|k| lead_b.contains(k)).collect();
            let over = shared.len() > MAX_OVERLAP;
            failed |= over;
            let named: Vec<String> = shared
                .iter()
                .map(|&&(media, id)| {
                    name(&indexes, &serde_json::json!({"type": if media == MediaType::Movie { "movie" } else { "series" }, "id": id}))
                })
                .collect();
            println!(
                "  {a} / {b}: {}{} {}",
                shared.len(),
                if over { " FAILED" } else { "" },
                named.join(", ")
            );
        }
    }
    println!("\n{}", shown.join("\n"));
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slide(kind: &str, id: u32, fresh: f64, arrived: f64, quality: f64, buzz: f64) -> serde_json::Value {
        serde_json::json!({"type": kind, "id": id,
            "why": {"fresh": fresh, "arrived": arrived, "quality": quality, "buzz": buzz}})
    }

    #[test]
    fn a_slate_is_measured_over_its_lead_alone() {
        let mut slides = vec![
            slide("movie", 1, 0.01, 0.1, 0.1, 0.0),
            slide("movie", 2, 0.6, 0.0, 0.5, 0.4),
            slide("series", 3, 0.0, 0.9, 0.5, 0.2),
            slide("series", 4, 0.02, 0.25, 0.7, 0.0),
        ];
        // Beyond the lead: counted in the whole answer's mix and nowhere else.
        slides.extend((10..20).map(|id| slide("movie", id, 0.0, 0.0, 0.0, 0.0)));
        let near = |a: &serde_json::Value, b: &serde_json::Value| a["type"] == b["type"];
        let slate = measure(&slides, near);
        assert_eq!(
            slate,
            Slate {
                catalogue: 1 + 1 + 6,
                low_quality: 1 + 6,
                films: 8,
                series: 2,
                zero_buzz: 2 + 6,
                stacked: 8,
                lowest_fit: 0.0,
                all_films: 12,
                all_series: 2,
            }
        );
        let failed = failures(&slate, true);
        assert_eq!(failed.len(), 5, "{failed:?}");
        assert_eq!(failures(&slate, false).len(), 4, "a one-type surface has no mix to fail");
        assert!(failures(&Slate { films: 5, series: 5, ..Slate::default() }, true).is_empty());
    }
}
