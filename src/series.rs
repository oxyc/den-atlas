//! How strongly belonging to one franchise series says two titles belong together — the series' STRENGTH,
//! worked out once per load.
//!
//! Wikidata's "part of the series" (P179) names story franchises (James Bond, the MCU), studio catalogues
//! (Walt Disney Animation Studios feature films), anthologies (Masters of Horror) and critics' lists alike, and
//! size tells them apart in neither direction: Bond has 27 members, Welcome to the Blumhouse 8. What does is
//! how alike the members are, measured two ways:
//!
//! - **plot cohesion** — the mean pairwise CENTRED cosine of the members' plot vectors: each vector less the
//!   plot index's mean, re-normalised. Every plot vector shares a large common component, so raw cosine puts
//!   any two titles near the same value; centred, a random pair is ~0.
//! - **people share** — the share of member pairs that credit at least one person in common, cast or makers
//!   (directors, creators, screenwriters). An auteur cycle has unlike plots and shares everyone: Knives Out's
//!   plots are as far apart as a studio catalogue's, and every pair shares its writer-director.
//!
//! strength = max(ramp(plot, PLOT_LO, PLOT_HI), ramp(people, PEOPLE_LO, PEOPLE_HI)), in [0, 1].
//!
//! Membership is a LIST per title: a title can be in several real series (The Batman is in "Batman in film"
//! and in its own trilogy), so [`Title::series`] is a slice, and today's single `franchise` column is adapted
//! to one in [`SeriesStrength::from_facts`].

use crate::facts::Facts;
use den_index::{Index, MediaType};
use std::collections::HashMap;

/// Plot cohesion at or below `PLOT_LO` is no evidence, at or above `PLOT_HI` full evidence. Measured on the
/// published store (the opt-in test below): the big studio catalogues sit at 0.13–0.15 (Walt Disney
/// Animation 0.133, Sony Pictures Animation 0.139, DreamWorks 0.147) and two random animated films at 0.129,
/// so the ramp starts just above them; the loosest large story franchises sit at 0.24–0.25 (DC Extended
/// Universe 0.242, the whole MCU 0.245), where it ends. Bond (0.40) and Batman in film (0.32) are well past.
pub const PLOT_LO: f64 = 0.16;
pub const PLOT_HI: f64 = 0.24;

/// People share at or below `PEOPLE_LO` is no evidence, at or above `PEOPLE_HI` full. Catalogues and
/// anthologies share a person in 0–15% of pairs (a studio's regular voice actors, a composer); a trilogy by
/// one writer-director shares in every pair. `makers` counts screenwriters too, so a studio with house
/// writers shares more than its directors alone would: Studio Ghibli 0.44, Illumination 0.50.
pub const PEOPLE_LO: f64 = 0.2;
pub const PEOPLE_HI: f64 = 0.5;

/// The most members a series is measured on. Pairs grow with the square of the members, and 40 (780 pairs)
/// already pins the mean down; the sample is fixed per series, so the same store always gives the same
/// strengths.
const SAMPLE: usize = 40;

/// One title, as the strength reads it.
pub struct Title<'a> {
    pub key: (MediaType, u32),
    /// Its row in the plot index; `None` when it has no plot vector, and then it only counts towards people.
    pub row: Option<u32>,
    /// Every series it belongs to.
    pub series: &'a [u32],
    /// Everyone it credits, sorted and deduplicated. Empty means NOT KNOWN — Wikidata is open-world — so such
    /// a title is left out of the people share rather than counted as sharing nobody.
    pub people: Vec<u32>,
}

/// One series, measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Strength {
    /// Mean pairwise centred plot cosine; `None` under two members with a plot vector.
    pub plot: Option<f64>,
    /// Share of member pairs sharing a person; `None` under two members with known credits.
    pub people: Option<f64>,
    pub strength: f64,
}

#[derive(Default)]
pub struct SeriesStrength {
    by_series: HashMap<u32, Strength>,
}

impl SeriesStrength {
    /// Every series' strength out of the facts and the plot index. The facts' `franchise` is a single value
    /// today; it is read as a list of one so this does not change when the column becomes a list.
    pub fn from_facts(plot: &Index, facts: &Facts) -> SeriesStrength {
        let titles = facts.keys().filter_map(|(media, id)| {
            let record = facts.get(id, media)?;
            let series = record.franchise.as_slice();
            if series.is_empty() {
                return None;
            }
            let mut people: Vec<u32> = record.makers.iter().chain(&record.cast).copied().collect();
            people.sort_unstable();
            people.dedup();
            Some(Title { key: (media, id), row: plot.row_of(id, media), series, people })
        });
        SeriesStrength::build(plot, titles)
    }

    pub fn build<'a>(plot: &Index, titles: impl IntoIterator<Item = Title<'a>>) -> SeriesStrength {
        let titles: Vec<Title<'a>> = titles.into_iter().collect();
        let mut members: HashMap<u32, Vec<usize>> = HashMap::new();
        for (at, title) in titles.iter().enumerate() {
            for &series in title.series {
                let list = members.entry(series).or_default();
                if list.last() != Some(&at) {
                    list.push(at);
                }
            }
        }

        // Centred cosine from the index's own primitives: with m the mean, (a−m)·(b−m) = a·b − a·m − b·m + m·m,
        // and |a−m|² the same with b = a. Each member's projection and centred length are worked out once.
        let mean = plot.mean_vector();
        let mm: f64 = mean.iter().map(|m| m * m).sum();
        let centred: Vec<Option<(u32, f64, f64)>> = titles
            .iter()
            .map(|title| {
                let row = title.row?;
                let p = plot.projection(row, &mean);
                let length = (plot.similarity(row, row) - 2.0 * p + mm).max(0.0).sqrt();
                (length > 0.0).then_some((row, p, length))
            })
            .collect();

        let by_series = members
            .into_iter()
            .map(|(series, mut list)| {
                // A fixed pseudo-random order per series: not tmdb id order, which runs with release date and
                // would measure a long franchise on its oldest films.
                list.sort_unstable_by_key(|&at| mix(series, titles[at].key));
                let plotted: Vec<(u32, f64, f64)> =
                    list.iter().filter_map(|&at| centred[at]).take(SAMPLE).collect();
                let plot_cohesion = mean_over_pairs(&plotted, |a, b| {
                    (plot.similarity(a.0, b.0) - a.1 - b.1 + mm) / (a.2 * b.2)
                });
                let credited: Vec<&[u32]> = list
                    .iter()
                    .map(|&at| titles[at].people.as_slice())
                    .filter(|people| !people.is_empty())
                    .take(SAMPLE)
                    .collect();
                let people_share = mean_over_pairs(&credited, |a, b| {
                    f64::from(u8::from(a.iter().any(|p| b.binary_search(p).is_ok())))
                });
                let by_plot = ramp(plot_cohesion.unwrap_or(0.0), PLOT_LO, PLOT_HI);
                let by_people = ramp(people_share.unwrap_or(0.0), PEOPLE_LO, PEOPLE_HI);
                let strength = by_plot.max(by_people);
                (series, Strength { plot: plot_cohesion, people: people_share, strength })
            })
            .collect();
        SeriesStrength { by_series }
    }

    /// A series' strength in [0, 1]: 0 for a catalogue or list, 1 for a story franchise or an auteur cycle.
    /// 0 for a series with no measurable pair, which is no evidence either way.
    pub fn series_strength(&self, series: u32) -> f64 {
        self.by_series.get(&series).map_or(0.0, |s| s.strength)
    }

    /// A series' measurements, for tools and tests.
    #[cfg(test)]
    pub fn get(&self, series: u32) -> Option<&Strength> {
        self.by_series.get(&series)
    }

    pub fn len(&self) -> usize {
        self.by_series.len()
    }
}

fn ramp(x: f64, lo: f64, hi: f64) -> f64 {
    ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// The mean of `f` over every unordered pair; `None` under two items.
fn mean_over_pairs<T>(items: &[T], f: impl Fn(&T, &T) -> f64) -> Option<f64> {
    let n = items.len();
    if n < 2 {
        return None;
    }
    let mut sum = 0.0;
    for (i, a) in items.iter().enumerate() {
        for b in &items[i + 1..] {
            sum += f(a, b);
        }
    }
    Some(sum / (n * (n - 1) / 2) as f64)
}

/// A title's place in a series' sample order (splitmix64 of the series and the title).
fn mix(series: u32, (media, id): (MediaType, u32)) -> u64 {
    let kind = u64::from(matches!(media, MediaType::Tv));
    let mut z = (u64::from(series) << 33 ^ kind << 32 ^ u64::from(id)).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::fixture::{self, Title as Row};
    use crate::store::MappedStore;

    const CATALOGUE: u32 = 100;
    const SEQUELS: u32 = 200;
    const TRILOGY: u32 = 300;

    /// Axis `i` of an 8-dimensional space, as a quantised unit vector, signed.
    fn axis(i: usize, sign: i8) -> Vec<i8> {
        let mut v = vec![0i8; 8];
        v[i] = 127 * sign;
        v
    }

    /// A catalogue of four unlike films crediting nobody in common; a sequel pair telling one story; a
    /// trilogy by one director whose plots are as unlike as the catalogue's. Every axis also has a title
    /// pointing the other way outside any series, so the corpus mean is near zero and centring is visible
    /// only where it should be. Built once: the tests run in parallel and would race on one temp file.
    fn strengths() -> &'static SeriesStrength {
        static BUILT: std::sync::OnceLock<SeriesStrength> = std::sync::OnceLock::new();
        BUILT.get_or_init(build_fixture)
    }

    fn build_fixture() -> SeriesStrength {
        let film = |id: u32, plot: Vec<i8>, franchise: Option<u32>, makers: Vec<u32>, cast: Vec<u32>| Row {
            tmdb_id: id,
            plot,
            franchise,
            makers,
            cast,
            ..Row::default()
        };
        let mut rows = vec![
            film(1, axis(0, 1), Some(CATALOGUE), vec![11], vec![21]),
            film(2, axis(1, 1), Some(CATALOGUE), vec![12], vec![22]),
            film(3, axis(2, 1), Some(CATALOGUE), vec![13], vec![23]),
            film(4, axis(3, 1), Some(CATALOGUE), vec![14], vec![24]),
            film(5, axis(4, 1), Some(SEQUELS), Vec::new(), Vec::new()),
            film(6, axis(4, 1), Some(SEQUELS), Vec::new(), Vec::new()),
            film(7, axis(5, 1), Some(TRILOGY), vec![9], vec![31]),
            film(8, axis(6, 1), Some(TRILOGY), vec![9], vec![32]),
            film(9, axis(7, 1), Some(TRILOGY), vec![9], vec![33]),
        ];
        rows.extend((0..8).map(|i| film(50 + i as u32, axis(i, -1), None, Vec::new(), Vec::new())));

        let dir = std::env::temp_dir().join(format!("den-atlas-series-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("series.store");
        fixture::write(&path, "v1", 8, &rows, &[]);
        let store = MappedStore::open(&path).expect("store");
        let plot = Index::from_store_plot(&store.view()).expect("plot index");
        let facts = Facts::from_store(&store.view()).expect("facts");
        let strengths = SeriesStrength::from_facts(&plot, &facts);
        std::fs::remove_dir_all(&dir).ok();
        strengths
    }

    #[test]
    fn a_catalogue_of_unlike_titles_is_no_evidence() {
        let s = strengths();
        let catalogue = s.get(CATALOGUE).expect("measured");
        assert!(catalogue.plot.expect("four plots") < PLOT_LO, "{catalogue:?}");
        assert_eq!(catalogue.people, Some(0.0));
        assert_eq!(s.series_strength(CATALOGUE), 0.0);
    }

    #[test]
    fn a_sequel_pair_telling_one_story_is_full_evidence() {
        let s = strengths();
        let sequels = s.get(SEQUELS).expect("measured");
        assert!(sequels.plot.expect("two plots") > 0.99, "{sequels:?}");
        // Neither credits anyone: not known, so there is no pair to share over — not a share of 0.
        assert_eq!(sequels.people, None);
        assert_eq!(s.series_strength(SEQUELS), 1.0);
    }

    #[test]
    fn a_trilogy_by_one_director_is_full_evidence_through_its_people() {
        let s = strengths();
        let trilogy = s.get(TRILOGY).expect("measured");
        assert!(trilogy.plot.expect("three plots") < PLOT_LO, "the plots are unlike: {trilogy:?}");
        assert_eq!(trilogy.people, Some(1.0));
        assert_eq!(s.series_strength(TRILOGY), 1.0);
    }

    #[test]
    fn a_title_in_several_series_counts_in_each() {
        // No member has a plot vector, so the index is only there to be passed.
        let dir = std::env::temp_dir().join(format!("den-atlas-series-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("one.store");
        fixture::write(&path, "v1", 8, &[Row { tmdb_id: 99, plot: axis(0, 1), ..Row::default() }], &[]);
        let store = MappedStore::open(&path).expect("store");
        std::fs::remove_dir_all(&dir).ok();
        let plot = Index::from_store_plot(&store.view()).expect("plot index");
        let title = |id: u32, series: &'static [u32]| Title {
            key: (MediaType::Movie, id),
            row: None,
            series,
            people: vec![7],
        };
        let s = SeriesStrength::build(&plot, [title(1, &[1, 2]), title(2, &[1]), title(3, &[2])]);
        assert_eq!(s.get(1).and_then(|m| m.people), Some(1.0));
        assert_eq!(s.get(2).and_then(|m| m.people), Some(1.0));
        assert_eq!(s.series_strength(3), 0.0, "a series nobody is in is no evidence");
    }

    /// The named series of the franchise audit, measured on a REAL store. Opt-in via `DEN_STORE`; run with
    /// `--nocapture` to read the table. Q-ids are Wikidata's.
    #[test]
    fn named_series_on_the_real_store() {
        let Ok(path) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to measure the named series");
            return;
        };
        let store = MappedStore::open(std::path::Path::new(&path)).expect("store");
        let plot = Index::from_store_plot(&store.view()).expect("plot index");
        let facts = Facts::from_store(&store.view()).expect("facts");
        let started = std::time::Instant::now();
        let s = SeriesStrength::from_facts(&plot, &facts);
        let took = started.elapsed();

        let mut members: HashMap<u32, usize> = HashMap::new();
        for (media, id) in facts.keys() {
            if let Some(f) = facts.get(id, media).and_then(|r| r.franchise) {
                *members.entry(f).or_default() += 1;
            }
        }
        let named = [
            (2484680, "James Bond"),
            (26451486, "Eon James Bond series"),
            (2158362, "Superman in film"),
            (1975401, "Superman"),
            (642878, "Marvel Cinematic Universe"),
            (2111133, "Batman in film"),
            (18914861, "Batman"),
            (56070713, "Walt Disney Animation Studios feature film"),
            (26196748, "DreamWorks Animation feature films"),
            (26705935, "BBC's 100 Greatest Films of the 21st Century"),
            (115676286, "Knives Out"),
            (169604, "Three Flavours Cornetto trilogy"),
            (104848477, "Welcome to the Blumhouse"),
            (586486, "Masters of Horror"),
            (205683, "Scooby-Doo"),
            (97138261, "Pokémon"),
            (131144, "Tom and Jerry"),
            (104830727, "Studio Ghibli Feature Films"),
            (12294798, "Detective Conan"),
        ];
        let show = |x: Option<f64>| x.map_or("  —  ".to_owned(), |x| format!("{x:.3}"));
        eprintln!("| series | members | plot | people | strength |");
        for (qid, name) in named {
            let m = members.get(&qid).copied().unwrap_or(0);
            match s.get(qid) {
                Some(x) => {
                    eprintln!("| {name} | {m} | {} | {} | {:.2} |", show(x.plot), show(x.people), x.strength)
                }
                None => eprintln!("| {name} | {m} | not in the store's franchise column | | |"),
            }
        }

        // Two random animated films: where a studio catalogue sits by construction.
        let mean = plot.mean_vector();
        let mm: f64 = mean.iter().map(|m| m * m).sum();
        let mut animated: Vec<u32> = facts
            .keys()
            .filter(|&(media, id)| {
                media == MediaType::Movie && facts.get(id, media).is_some_and(|r| r.genres.contains(&16))
            })
            .filter_map(|(media, id)| plot.row_of(id, media))
            .collect();
        animated.sort_unstable();
        let centred = |a: u32, b: u32| {
            let (pa, pb) = (plot.projection(a, &mean), plot.projection(b, &mean));
            let la = (plot.similarity(a, a) - 2.0 * pa + mm).sqrt();
            let lb = (plot.similarity(b, b) - 2.0 * pb + mm).sqrt();
            (plot.similarity(a, b) - pa - pb + mm) / (la * lb)
        };
        let n = animated.len() as u64;
        let pairs = 4000u64;
        let random_animated = (0..pairs)
            .map(|i| {
                let a = animated[(mix(1, (MediaType::Movie, i as u32)) % n) as usize];
                let b = animated[(mix(2, (MediaType::Movie, i as u32)) % n) as usize];
                if a == b {
                    0.0
                } else {
                    centred(a, b)
                }
            })
            .sum::<f64>()
            / pairs as f64;
        eprintln!("random animated film pair: plot {random_animated:.3} over {n} animated films");

        let all: Vec<&Strength> = s.by_series.values().collect();
        let at = |f: &dyn Fn(f64) -> bool| all.iter().filter(|x| f(x.strength)).count();
        eprintln!(
            "{} series: {} at 0, {} between, {} at 1; built in {:.1} ms",
            all.len(),
            at(&|x| x == 0.0),
            at(&|x| x > 0.0 && x < 1.0),
            at(&|x| x == 1.0),
            took.as_secs_f64() * 1000.0
        );

        // Catalogues, a critics' list and anthologies carry nothing; story franchises and auteur cycles
        // carry everything, the cycles through their people alone.
        for qid in [56070713, 26196748, 26705935, 104848477, 586486] {
            assert_eq!(s.series_strength(qid), 0.0, "Q{qid} {:?}", s.get(qid));
        }
        for qid in [2484680, 2158362, 2111133, 115676286, 169604] {
            assert_eq!(s.series_strength(qid), 1.0, "Q{qid} {:?}", s.get(qid));
        }
        for qid in [115676286, 169604] {
            assert!(s.get(qid).and_then(|x| x.plot).is_some_and(|p| p < PLOT_HI), "Q{qid}");
        }
    }
}
