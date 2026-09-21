//! A/B the shipped More Like This scorer against the pooled one, on the real shipped corpus.
//!
//!   cargo run --release -p den-index --example rail_ab -- <dataset dir>
//!
//! The dataset dir is a producer out-dir holding `labels-t02.json` + `vectors-bge-m3.bin` (the plot index)
//! and `labels-premise.json` + `vectors-premise.bin` (the premise index).
//!
//! Reports, per anchor and in aggregate, the two numbers that describe the SHAPE of a row — the share of the
//! twenty carrying the anchor's own primary genre, and the share carrying one single subgenre. Those are the
//! measurable form of "it reads as a genre shelf", which no per-title assertion can express.

use den_index::{more_like_this, more_like_this_pooled, Authorship, Facets, Index, MediaType};
use std::collections::{HashMap, HashSet};

/// `mediaType:tmdbId` -> the Wikidata q-ids credited as director, writer or creator.
///
/// Read from the shipped facts sidecar. den-index does not know what a fact is, so the lookup is built here
/// and passed in as a callback — the same shape den-atlas would use, where `facts.rs` already holds this.
type Credits = HashMap<(u32, String), HashSet<String>>;

/// Facts-backed authorship for one seed: who made it, where it lived, and which titles share either.
struct FactsAuthorship<'a> {
    makers: &'a Credits,
    homes: &'a Credits,
    key: String,
    mine_makers: HashSet<String>,
    mine_homes: HashSet<String>,
}

impl Authorship for FactsAuthorship<'_> {
    fn nominate(&self) -> Vec<u32> {
        let mut out = Vec::new();
        // Makers only. Nominating everything that shares a HOME floods the pool — HBO alone is 131 titles —
        // and measured worse: The Wire kept The Deuce but lost Show Me a Hero, and mean genre share rose
        // from 46% to 48%. A home is where a title lived, not evidence that it is the same kind of thing.
        for (set, mine) in [(self.makers, &self.mine_makers)] {
            if mine.is_empty() {
                continue;
            }
            for ((id, mt), theirs) in set {
                if *mt == self.key && !theirs.is_disjoint(mine) {
                    out.push(*id);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
    fn makers(&self, id: u32) -> f64 {
        share(self.makers, &self.key, id, &self.mine_makers)
    }
    fn home(&self, id: u32) -> f64 {
        share(self.homes, &self.key, id, &self.mine_homes)
    }
}

fn share(set: &Credits, key: &str, id: u32, mine: &HashSet<String>) -> f64 {
    if mine.is_empty() {
        return 0.0;
    }
    set.get(&(id, key.to_string()))
        .map_or(0.0, |theirs| mine.intersection(theirs).count() as f64 / mine.len() as f64)
}

fn load_credits(dir: &str, fields: &[&str]) -> Credits {
    let raw = std::fs::read(format!("{dir}/facts-merged.json")).expect("facts");
    let v: serde_json::Value = serde_json::from_slice(&raw).expect("facts json");
    let mut out: HashMap<(u32, String), HashSet<String>> = HashMap::new();
    for r in v["records"].as_array().into_iter().flatten() {
        let (Some(id), Some(mt)) = (r["tmdbId"].as_u64(), r["mediaType"].as_str()) else { continue };
        let mut set = HashSet::new();
        for field in fields {
            for q in r[field].as_array().into_iter().flatten() {
                if let Some(q) = q.as_str() {
                    set.insert(q.to_string());
                }
            }
        }
        if !set.is_empty() {
            out.insert((id as u32, mt.to_string()), set);
        }
    }
    out
}

/// Facets from the completed model pass, keyed `mediaType:tmdbId`.
struct JevFacets {
    by_key: HashMap<String, Vec<(String, String, f64)>>,
    world: HashMap<String, f64>,
    nouls: HashMap<String, Vec<(String, f64)>>,
    critique: HashMap<String, Vec<(String, f64)>>,
    critique_raw: HashMap<String, Vec<(String, f64)>>,
    idf: HashMap<String, f64>,
    prevalence: HashMap<(String, String), f64>,
    key: String,
    /// The harness's own string dictionary, filled lazily as the scorer asks for names: `ids` maps a
    /// name to its id, `names` maps back so `prevalence` can recover the pair it was handed.
    ids: std::cell::RefCell<HashMap<String, den_index::ValueId>>,
    names: std::cell::RefCell<Vec<String>>,
    /// Facet axes get their own table — the trait passes an axis as a `u8`, separately from values.
    axes: std::cell::RefCell<Vec<String>>,
}

/// Interns a name to the id the trait now passes.
///
/// The harness reads JSON, where every axis and value is a string; the scorer compares ids. One table
/// here gives the same equality the strings had — two names share an id exactly when they are equal —
/// which is the property the store gets from writing one dictionary.
impl JevFacets {
    fn id(&self, name: &str) -> den_index::ValueId {
        if let Some(found) = self.ids.borrow().get(name) {
            return *found;
        }
        let mut names = self.names.borrow_mut();
        let next = names.len() as den_index::ValueId;
        names.push(name.to_owned());
        self.ids.borrow_mut().insert(name.to_owned(), next);
        next
    }

    fn name(&self, id: den_index::ValueId) -> Option<String> {
        self.names.borrow().get(id as usize).cloned()
    }

    fn weigh(&self, pairs: &[(String, f64)]) -> Vec<den_index::Weighted> {
        pairs.iter().map(|(name, p)| (self.id(name), *p)).collect()
    }

    fn axis(&self, name: &str) -> den_index::Axis {
        let mut axes = self.axes.borrow_mut();
        match axes.iter().position(|a| a == name) {
            Some(found) => found as den_index::Axis,
            None => {
                axes.push(name.to_owned());
                (axes.len() - 1) as den_index::Axis
            }
        }
    }

    fn axis_name(&self, axis: den_index::Axis) -> Option<String> {
        self.axes.borrow().get(axis as usize).cloned()
    }
}

impl Facets for JevFacets {
    fn facets(&self, id: u32) -> Vec<(den_index::Axis, den_index::ValueId, f64)> {
        self.by_key
            .get(&format!("{}:{}", self.key, id))
            .map(|rows| rows.iter().map(|(a, v, c)| (self.axis(a), self.id(v), *c)).collect())
            .unwrap_or_default()
    }
    /// Axes the seed reads >= 0.8 on, weighted by ln(N / titles >= 0.7 on that axis): its defining
    /// arguments, with a common one worth less than a rare one.
    fn critique_defining(&self, id: u32) -> Vec<den_index::Weighted> {
        self.critique_raw
            .get(&format!("{}:{}", self.key, id))
            .map(|rows| {
                rows.iter()
                    .filter(|(_, p)| *p >= 0.8)
                    .filter_map(|(a, _)| {
                        let w = self.idf.get(a).copied().unwrap_or(0.0);
                        (w > 0.0).then(|| (self.id(a), w))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn critique_raw(&self, id: u32) -> Vec<den_index::Weighted> {
        self.critique_raw
            .get(&format!("{}:{}", self.key, id))
            .map(|rows| self.weigh(rows))
            .unwrap_or_default()
    }

    fn critique_top(&self, seed: u32, other: u32, n: usize) -> bool {
        let defining = self.critique_defining(seed);
        if defining.is_empty() {
            return false;
        }
        let score = |id: &str| {
            self.critique_raw
                .get(id)
                .and_then(|theirs| {
                    let theirs = self.weigh(theirs);
                    let total: f64 = defining.iter().map(|(_, w)| w).sum();
                    (total > 0.0).then(|| {
                        defining
                            .iter()
                            .filter_map(|(a, w)| theirs.iter().find(|(n, _)| n == a).map(|(_, p)| w * p))
                            .sum::<f64>()
                            / total
                    })
                })
                .unwrap_or(0.0)
        };
        let mine = score(&format!("{}:{}", self.key, other));
        // How many titles of this media type beat it. Cheap enough at 7.5k rows, and this only runs for
        // candidates that would otherwise be cut by the tone floor.
        let better = self.critique_raw.keys().filter(|k| k.starts_with(&self.key) && score(k) > mine).count();
        better < n
    }

    fn critique(&self, id: u32) -> Vec<den_index::Weighted> {
        self.critique.get(&format!("{}:{}", self.key, id)).map(|rows| self.weigh(rows)).unwrap_or_default()
    }
    fn nouls(&self, id: u32) -> Vec<den_index::Weighted> {
        self.nouls.get(&format!("{}:{}", self.key, id)).map(|rows| self.weigh(rows)).unwrap_or_default()
    }
    fn world(&self, id: u32) -> f64 {
        self.world.get(&format!("{}:{}", self.key, id)).copied().unwrap_or(0.0)
    }
    fn prevalence(&self, axis: den_index::Axis, value: den_index::ValueId) -> f64 {
        let (Some(axis), Some(value)) = (self.axis_name(axis), self.name(value)) else {
            return 1.0;
        };
        self.prevalence.get(&(axis, value)).copied().unwrap_or(1.0)
    }
}

fn load_facets(dir: &str, key: &str) -> JevFacets {
    let raw = std::fs::read(format!("{dir}/jev-facets.json")).expect("jev-facets");
    let v: serde_json::Value = serde_json::from_slice(&raw).expect("facets json");
    let mut by_key: HashMap<String, Vec<(String, String, f64)>> = HashMap::new();
    let mut world: HashMap<String, f64> = HashMap::new();
    let mut nouls: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    let mut critique_raw: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut counts: HashMap<(String, String), f64> = HashMap::new();
    let mut total: f64 = 0.0;
    for (k, axes) in v.as_object().into_iter().flatten() {
        let same_type = k.starts_with(key);
        if same_type {
            total += 1.0;
        }
        let mut list = Vec::new();
        for (axis, pair) in axes.as_object().into_iter().flatten() {
            if axis == "__critique" {
                critique_raw.insert(
                    k.clone(),
                    pair.as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(n, p)| Some((n.clone(), p.as_f64()?)))
                        .collect(),
                );
                continue;
            }
            if axis == "__nouls" {
                let list = pair
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(n, p)| Some((n.clone(), p.as_f64()?)))
                    .collect();
                nouls.insert(k.clone(), list);
                continue;
            }
            if axis == "__world" {
                world.insert(k.clone(), pair.as_f64().unwrap_or(0.0));
                continue;
            }
            let (Some(val), Some(conf)) = (pair[0].as_str(), pair[1].as_f64()) else { continue };
            list.push((axis.to_string(), val.to_string(), conf));
            if same_type {
                *counts.entry((axis.to_string(), val.to_string())).or_insert(0.0) += 1.0;
            }
        }
        by_key.insert(k.clone(), list);
    }
    // Center the critique profile on the per-axis mean within this media type, once, here — raw cosine
    // over seventeen mostly-low values is dominated by a shared baseline (Oz 0.885 / Angel 0.792 against
    // The Wire; centered, +0.700 / +0.274). Every title gets every axis, so a missing answer is centered
    // to -mean rather than silently skipped by the cosine's name intersection.
    let same: Vec<&String> = critique_raw.keys().filter(|k| k.starts_with(key)).collect();
    let mut axes: Vec<String> = critique_raw.values().flat_map(|m| m.keys().cloned()).collect();
    axes.sort();
    axes.dedup();
    let mut means: HashMap<String, f64> = HashMap::new();
    for axis in &axes {
        let sum: f64 = same.iter().map(|k| critique_raw[*k].get(axis).copied().unwrap_or(0.0)).sum();
        means.insert(axis.clone(), sum / same.len().max(1) as f64);
    }
    // idf over titles of this media type reading >= 0.7 on an axis.
    let mut idf: HashMap<String, f64> = HashMap::new();
    for axis in &axes {
        let n = same.iter().filter(|k| critique_raw[**k].get(axis).copied().unwrap_or(0.0) >= 0.7).count();
        idf.insert(axis.clone(), ((same.len().max(1) as f64) / (n.max(1) as f64)).ln().max(0.0));
    }
    let critique_raw_vecs: HashMap<String, Vec<(String, f64)>> = critique_raw
        .iter()
        .map(|(k, m)| (k.clone(), m.iter().map(|(a, p)| (a.clone(), *p)).collect()))
        .collect();

    let critique: HashMap<String, Vec<(String, f64)>> = critique_raw
        .iter()
        .map(|(k, m)| {
            let centered =
                axes.iter().map(|a| (a.clone(), m.get(a).copied().unwrap_or(0.0) - means[a])).collect();
            (k.clone(), centered)
        })
        .collect();

    // Prevalence within the seed's own media type: `continuity = episodic` is rare among films and common
    // among series, and a share computed over both would misprice it for each.
    let prevalence = counts.into_iter().map(|(k, n)| (k, n / total.max(1.0))).collect();
    JevFacets {
        by_key,
        world,
        nouls,
        critique,
        critique_raw: critique_raw_vecs,
        idf,
        prevalence,
        key: key.to_string(),
        ids: Default::default(),
        names: Default::default(),
        axes: Default::default(),
    }
}

fn load(dir: &str, labels: &str, vectors: &str) -> Index {
    let l = std::fs::read(format!("{dir}/{labels}")).expect("labels");
    let v = std::fs::read(format!("{dir}/{vectors}")).expect("vectors");
    Index::from_blobs(&l, v).expect("index")
}

/// The share of a row carrying the anchor's own primary genre, and the largest share carrying one subgenre.
fn shape(index: &Index, ids: &[u32], media: MediaType, seed_genre: &str) -> (f64, f64, String) {
    if ids.is_empty() {
        return (0.0, 0.0, String::new());
    }
    let mut same_genre = 0usize;
    let mut sub: HashMap<String, usize> = HashMap::new();
    for &id in ids {
        if let Some(l) = index.labels(id, media) {
            if l.primary_genre == seed_genre {
                same_genre += 1;
            }
            if let Some((n, _)) =
                l.subgenres.iter().filter(|(_, c)| *c >= 0.55).max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            {
                *sub.entry((*n).to_string()).or_insert(0) += 1;
            }
        }
    }
    let (top_name, top_n) = sub.into_iter().max_by_key(|&(_, n)| n).unwrap_or((String::new(), 0));
    (same_genre as f64 / ids.len() as f64, top_n as f64 / ids.len() as f64, top_name)
}

fn title(index: &Index, id: u32, media: MediaType) -> String {
    index.labels(id, media).map_or_else(|| format!("{id}"), |l| format!("{id} [{}]", l.primary_genre))
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: rail_ab <dataset dir>");
    let plot = load(&dir, "labels-t02.json", "vectors-bge-m3.bin");
    let premise = load(&dir, "labels-premise.json", "vectors-premise.bin");
    let makers = load_credits(&dir, &["directors", "screenwriters", "creators", "makers"]);
    let homes = load_credits(&dir, &["broadcaster", "productionCompanies"]);
    let facets_tv = load_facets(&dir, "tv");
    let facets_movie = load_facets(&dir, "movie");

    // Anchors chosen to cover the reported defects and the cases the rail already gets right, so a change
    // that only helps The Wire is visible as such.
    let anchors: Vec<(&str, u32, MediaType)> = vec![
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

    // Titles a viewer would expect, and ones the row should not contain. Checked in both arms.
    let wanted: HashMap<u32, Vec<(&str, u32)>> = HashMap::from([
        (
            1438,
            vec![
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
        (314365, vec![("The Post", 446354), ("She Said", 837881)]),
        (137, vec![("Palm Springs", 587792)]),
        (1396, vec![("Better Call Saul", 60059)]),
        (5723, vec![("Begin Again", 198277), ("Sing Street", 369557), ("Flora and Son", 1059811)]),
        (3322, vec![("The Wire", 1438)]),
        (758336, vec![("Voicemails for Isabelle", 614945)]),
        (614945, vec![("Love Again", 758336)]),
    ]);
    let unwanted: HashMap<u32, Vec<(&str, u32)>> = HashMap::from([
        (1438, vec![("Bates Motel", 46786)]),
        (76331, vec![("Dynasty 1981", 3769), ("Dallas", 6647)]),
    ]);

    println!(
        "{:<16} {:>9} {:>9}   {:>9} {:>9}   {:>4} {:>4}",
        "anchor", "A genre", "A subgen", "B genre", "B subgen", "A n", "B n"
    );
    println!("{}", "-".repeat(82));
    let (mut ag, mut asg, mut bg, mut bsg, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut notes: Vec<String> = Vec::new();

    for (name, id, media) in &anchors {
        let Some(seed) = premise.labels(*id, *media).or_else(|| plot.labels(*id, *media)) else {
            println!("{name:<16}  NOT IN EITHER INDEX");
            continue;
        };
        let genre = seed.primary_genre.to_string();
        let a = more_like_this(Some(&plot), Some(&premise), *id, *media);
        let key = if *media == MediaType::Tv { "tv" } else { "movie" }.to_string();
        let auth = FactsAuthorship {
            makers: &makers,
            homes: &homes,
            mine_makers: makers.get(&(*id, key.clone())).cloned().unwrap_or_default(),
            mine_homes: homes.get(&(*id, key.clone())).cloned().unwrap_or_default(),
            key,
        };
        let fx: &dyn Facets = if *media == MediaType::Tv { &facets_tv } else { &facets_movie };
        let b_full = more_like_this_pooled(Some(&plot), Some(&premise), *id, *media, Some(&auth), Some(fx));
        // Shape is judged on the visible screenful, not the whole scrollable row, so the numbers stay
        // comparable with the shipped scorer's twenty.
        let b: Vec<u32> = b_full.iter().copied().take(20).collect();
        let (a_g, a_s, _) = shape(&plot, &a, *media, &genre);
        let (b_g, b_s, _) = shape(&plot, &b, *media, &genre);
        println!(
            "{name:<16} {:>8.0}% {:>8.0}%   {:>8.0}% {:>8.0}%   {:>4} {:>4}",
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

        for (label, want) in wanted.get(id).unwrap_or(&Vec::new()) {
            let pa = a.iter().position(|x| x == want).map_or("-".into(), |p| (p + 1).to_string());
            let pb = b_full.iter().position(|x| x == want).map_or("-".into(), |p| (p + 1).to_string());
            if pa != pb {
                notes.push(format!("  want {label:<20} {name:<14} A={pa:<4} B={pb}"));
            }
        }
        for (label, bad) in unwanted.get(id).unwrap_or(&Vec::new()) {
            let pa = a.iter().position(|x| x == bad).map_or("-".into(), |p| (p + 1).to_string());
            let pb = b_full.iter().position(|x| x == bad).map_or("-".into(), |p| (p + 1).to_string());
            if pa != pb {
                notes.push(format!("  DROP {label:<20} {name:<14} A={pa:<4} B={pb}"));
            }
        }
    }

    println!("{}", "-".repeat(82));
    println!(
        "{:<16} {:>8.0}% {:>8.0}%   {:>8.0}% {:>8.0}%",
        "MEAN",
        ag / n * 100.0,
        asg / n * 100.0,
        bg / n * 100.0,
        bsg / n * 100.0
    );
    println!("\nmoves:");
    for line in notes {
        println!("{line}");
    }

    // Print one anchor's row in full, built exactly as the loop above builds it — an independently
    // constructed copy printed a different row than the one being measured, which is worse than no print.
    if let Ok(want) = std::env::var("RAIL_SHOW") {
        if let Some((name, id, media)) = anchors.iter().find(|a| a.0 == want) {
            let key = if *media == MediaType::Tv { "tv" } else { "movie" }.to_string();
            let auth = FactsAuthorship {
                makers: &makers,
                homes: &homes,
                mine_makers: makers.get(&(*id, key.clone())).cloned().unwrap_or_default(),
                mine_homes: homes.get(&(*id, key.clone())).cloned().unwrap_or_default(),
                key: key.clone(),
            };
            let fx: &dyn Facets = if *media == MediaType::Tv { &facets_tv } else { &facets_movie };
            println!(
                "
{name}, pooled scorer:"
            );
            for (i, id) in
                more_like_this_pooled(Some(&plot), Some(&premise), *id, *media, Some(&auth), Some(fx))
                    .iter()
                    .enumerate()
                    .take(20)
            {
                println!("  {:>2}. {}", i + 1, title(&plot, *id, *media));
            }
        }
    }
}
