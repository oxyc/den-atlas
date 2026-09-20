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

use den_index::{more_like_this, more_like_this_pooled, Index, MediaType};
use std::collections::HashMap;

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
            if let Some((n, _)) = l
                .subgenres
                .iter()
                .filter(|(_, c)| *c >= 0.55)
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
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
    ];

    // Titles a viewer would expect, and ones the row should not contain. Checked in both arms.
    let wanted: HashMap<u32, Vec<(&str, u32)>> = HashMap::from([
        (1438, vec![("We Own This City", 125949), ("Homicide", 4464), ("Show Me a Hero", 63248), ("The Corner", 14531)]),
        (314365, vec![("The Post", 446354), ("She Said", 837881)]),
        (137, vec![("Palm Springs", 587792)]),
        (1396, vec![("Better Call Saul", 60059)]),
    ]);
    let unwanted: HashMap<u32, Vec<(&str, u32)>> =
        HashMap::from([(1438, vec![("Bates Motel", 46786)]), (76331, vec![("Dynasty 1981", 3769), ("Dallas", 6647)])]);

    println!("{:<16} {:>9} {:>9}   {:>9} {:>9}   {:>4} {:>4}", "anchor", "A genre", "A subgen", "B genre", "B subgen", "A n", "B n");
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
        let b = more_like_this_pooled(Some(&plot), Some(&premise), *id, *media);
        let (a_g, a_s, _) = shape(&plot, &a, *media, &genre);
        let (b_g, b_s, _) = shape(&plot, &b, *media, &genre);
        println!(
            "{name:<16} {:>8.0}% {:>8.0}%   {:>8.0}% {:>8.0}%   {:>4} {:>4}",
            a_g * 100.0, a_s * 100.0, b_g * 100.0, b_s * 100.0, a.len(), b.len()
        );
        ag += a_g; asg += a_s; bg += b_g; bsg += b_s; n += 1.0;

        for (label, want) in wanted.get(id).unwrap_or(&Vec::new()) {
            let pa = a.iter().position(|x| x == want).map_or("-".into(), |p| (p + 1).to_string());
            let pb = b.iter().position(|x| x == want).map_or("-".into(), |p| (p + 1).to_string());
            if pa != pb {
                notes.push(format!("  want {label:<20} {name:<14} A={pa:<4} B={pb}"));
            }
        }
        for (label, bad) in unwanted.get(id).unwrap_or(&Vec::new()) {
            let pa = a.iter().position(|x| x == bad).map_or("-".into(), |p| (p + 1).to_string());
            let pb = b.iter().position(|x| x == bad).map_or("-".into(), |p| (p + 1).to_string());
            if pa != pb {
                notes.push(format!("  DROP {label:<20} {name:<14} A={pa:<4} B={pb}"));
            }
        }
    }

    println!("{}", "-".repeat(82));
    println!(
        "{:<16} {:>8.0}% {:>8.0}%   {:>8.0}% {:>8.0}%",
        "MEAN", ag / n * 100.0, asg / n * 100.0, bg / n * 100.0, bsg / n * 100.0
    );
    println!("\nmoves:");
    for line in notes {
        println!("{line}");
    }

    if let Some(wire) = anchors.iter().find(|a| a.1 == 1438) {
        println!("\nThe Wire, pooled scorer:");
        for (i, id) in more_like_this_pooled(Some(&plot), Some(&premise), wire.1, wire.2).iter().enumerate() {
            println!("  {:>2}. {}", i + 1, title(&plot, *id, wire.2));
        }
    }
}
