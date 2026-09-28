//! Other versions of a title's story (oxyc/den-atlas#112): remakes and other adaptations of the same source
//! work. Two kinds of link make them:
//!
//! - the optional store sections `den_store::Store::other_versions` reads, which den-dataset groups from
//!   Wikidata P144 (based on) and P4969 (derivative work);
//! - a shared title character (`character_links`), from TMDB's credits at runtime: *Young Wallander* and the
//!   British and Swedish *Wallander* all have Kurt Wallander in their name and in their cast. Wikidata cannot
//!   make this link: *Young Wallander*'s item states no character (P674) and Kurt Wallander names it in no
//!   "present in work" (P1441).
//!
//! A version that belongs to a curated franchise stands for that whole franchise: the row shows every member,
//! grouped and labelled by the franchise, since the British *Wallander*'s version of the Swedish series is
//! also a version of the Swedish films. The seed's own curated primary franchise is left out, because
//! `/index/franchise` shows it; the store keeps those links, so a change to the franchise grouping needs no
//! change to the versions.
//!
//! More Like This leaves every one of these out (`kept_out`): the seed's franchise, its versions, their
//! franchises, and the versions of its franchise's members, which all have rows of their own.

use crate::characters::CharacterIndex;
use crate::franchises::Franchise;
use crate::queries::Indexes;
use den_index::MediaType;
use std::collections::{BTreeSet, HashMap, HashSet};

type Key = (MediaType, u32);

/// The most titles one character may name and still link versions — den-dataset's cutoff for one source
/// (`pipeline/versions.py`). Kurt Wallander names 19; Sherlock Holmes (70) and Bruce Wayne (67) are
/// characters every adaptation and spin-off shares, not one story's versions.
pub const CHARACTER_CUTOFF: usize = 40;

/// Every store row's versions through a title character: two titles playing one named character link when
/// the character names no more than `CHARACTER_CUTOFF` titles and a word of its name is in both titles' names
/// ("Kurt Wallander" in *Young Wallander* and *Wallander*). A version is another production, so two titles
/// that continue one line are not: those sharing a Wikidata series, and those where one actor plays the same
/// character in both (`Tier::same_actor`) — sequels (*Pippi*, *Barry McKenzie*) and crossovers (*Lupin the
/// 3rd vs. Detective Conan*) that no curated franchise holds. Ascending rows, both directions.
pub fn character_links(
    view: &den_store::Store<'_>,
    characters: &CharacterIndex,
) -> Result<HashMap<u32, Vec<u32>>, String> {
    let err = |e: den_store::StoreError| e.to_string();
    let strings = view.strings().map_err(err)?;
    let titles = view.list::<u32>("alias_titles_v", "alias_titles_o").map_err(err)?;
    let series = view.franchises().map_err(err)?;
    let title_words = |row: u32| -> HashSet<String> {
        titles
            .get(den_store::Row(row as usize))
            .iter()
            .filter_map(|&id| strings.get(id))
            .flat_map(|text| {
                crate::characters::normalise(text).split(' ').map(str::to_owned).collect::<Vec<_>>()
            })
            .collect()
    };
    let apart = |a: u32, b: u32| {
        let (of_a, of_b) = (series.get(den_store::Row(a as usize)), series.get(den_store::Row(b as usize)));
        of_a.iter().any(|s| of_b.contains(s))
            || characters.of(a as usize).iter().any(|l| l.row == b && l.same_actor())
    };
    Ok(pair_up(characters.named().with_prefix(""), title_words, apart))
}

/// `character_links`' rule over `(normalised name, store rows playing it)`, each row's title words, and
/// whether two rows continue one line.
fn pair_up<'a>(
    names: impl Iterator<Item = (&'a str, &'a [u32])>,
    title_words: impl Fn(u32) -> HashSet<String>,
    apart: impl Fn(u32, u32) -> bool,
) -> HashMap<u32, Vec<u32>> {
    let mut words: HashMap<u32, HashSet<String>> = HashMap::new();
    let mut links: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    for (name, rows) in names {
        if !(2..=CHARACTER_CUTOFF).contains(&rows.len()) {
            continue;
        }
        for word in crate::characters::name_words(name) {
            let titled: Vec<u32> = rows
                .iter()
                .copied()
                .filter(|&row| words.entry(row).or_insert_with(|| title_words(row)).contains(word))
                .collect();
            for (at, &a) in titled.iter().enumerate() {
                for &b in &titled[at + 1..] {
                    if !apart(a, b) {
                        links.entry(a).or_default().insert(b);
                        links.entry(b).or_default().insert(a);
                    }
                }
            }
        }
    }
    links.into_iter().map(|(row, others)| (row, others.into_iter().collect())).collect()
}

/// The titles `title` is directly another version of, each once with its relation (`source` or `remake`, a
/// remake winning): the store's links, then those a title character makes, which are `source`. Nothing is
/// left out here.
fn linked(indexes: &Indexes, title: Key) -> Vec<(Key, &'static str)> {
    let store = indexes.store.view();
    let (Ok(Some(row)), Ok(keys)) =
        (store.row_of(u8::from(title.0 == MediaType::Tv), title.1), store.per_row::<u64>("keys"))
    else {
        return Vec::new();
    };
    let key_at = |at: usize| -> Option<Key> {
        let packed = *keys.get(at)?;
        Some((if packed >> 32 == 1 { MediaType::Tv } else { MediaType::Movie }, packed as u32))
    };
    let mut out: Vec<(Key, &'static str)> = Vec::new();
    let mut add = |key: Key, kind: &'static str| match out.iter_mut().find(|(held, _)| *held == key) {
        Some(held) if kind == "remake" => held.1 = kind,
        Some(_) => {}
        None => out.push((key, kind)),
    };
    if let Ok(versions) = store.other_versions() {
        for version in versions.get(row) {
            let kind = match version.kind {
                den_store::VersionKind::Remake => "remake",
                den_store::VersionKind::SharedSource => "source",
            };
            if let Some(key) = key_at(version.row.0) {
                add(key, kind);
            }
        }
    }
    if let Some(index) = indexes.characters.as_ref().and_then(|c| c.index()) {
        for &other in index.versions_of(row.0) {
            if let Some(key) = key_at(other as usize) {
                add(key, "source");
            }
        }
    }
    out
}

/// The members of a title's curated primary franchise, the title among them; none for a title without one.
fn members(indexes: &Indexes, key: Key) -> impl Iterator<Item = Key> + '_ {
    indexes.franchises.primary(key).into_iter().flat_map(|f| f.members.iter().map(|m| m.key))
}

/// Everything More Like This leaves to the franchise and Other versions rows: the seed's curated franchise,
/// every version of the seed or of a member of that franchise, and every member of those versions'
/// franchises. The seed itself is not in it.
pub fn kept_out(indexes: &Indexes, seed: Key) -> HashSet<Key> {
    let own: Vec<Key> = std::iter::once(seed).chain(members(indexes, seed)).collect();
    let mut out: HashSet<Key> = own.iter().copied().collect();
    for &title in &own {
        for (version, _) in linked(indexes, title) {
            if out.insert(version) {
                out.extend(members(indexes, version));
            }
        }
    }
    out.remove(&seed);
    out
}

/// One title in the Other versions row.
pub struct Version {
    pub key: Key,
    /// `remake` or `source`: how the seed reaches it; a franchise member the seed does not link to itself
    /// takes how it reaches the franchise.
    pub kind: &'static str,
    /// The curated franchise it is shown as part of, and its era there.
    pub group: Option<Group>,
}

pub struct Group {
    pub franchise: u32,
    pub era: u32,
}

/// The seed's other versions as served. A version in a curated franchise brings the whole franchise, era by
/// era in the franchise's own order and release order within each; the seed's own franchise is left out.
/// Titles and franchises come in release order — a franchise at its first member's date, a title with no
/// release date last — the most popular first on one date, then by key. Empty for a title with none, not in
/// the store, or a store without them.
pub fn of_title(indexes: &Indexes, export: Option<&den_titlesearch::TitleIndex>, seed: Key) -> Vec<Version> {
    let store = indexes.store.view();
    let Ok(released) = store.per_row::<i32>("released") else { return Vec::new() };
    let day = |key: Key| -> i32 {
        store
            .row_of(u8::from(key.0 == MediaType::Tv), key.1)
            .ok()
            .flatten()
            .and_then(|row| released.get(row.0).copied())
            .filter(|&d| d != i32::MIN)
            .unwrap_or(i32::MAX)
    };
    let stronger = |held: &mut &'static str, kind: &'static str| {
        if kind == "remake" {
            *held = kind;
        }
    };
    let mut alone: Vec<(Key, &'static str)> = Vec::new();
    let mut franchises: Vec<(u32, &'static str)> = Vec::new();
    let direct = linked(indexes, seed);
    for &(key, kind) in &direct {
        if key == seed || indexes.franchises.shares_primary(seed, key) {
            continue;
        }
        match indexes.franchises.membership(key) {
            Some(membership) => match franchises.iter_mut().find(|(f, _)| *f == membership.franchise) {
                Some((_, held)) => stronger(held, kind),
                None => franchises.push((membership.franchise, kind)),
            },
            None => alone.push((key, kind)),
        }
    }
    let mut runs: Vec<(i32, f64, Key, Vec<Version>)> = alone
        .into_iter()
        .map(|(key, kind)| {
            let popularity = crate::plotrows::popularity(indexes, export, key);
            (day(key), popularity, key, vec![Version { key, kind, group: None }])
        })
        .collect();
    for (franchise, kind) in franchises {
        let Some(group) = indexes.franchises.group(franchise) else { continue };
        let mut listed: Vec<_> = group.members.iter().filter(|m| m.key != seed).collect();
        listed.sort_by_key(|m| (group.eras.get(m.era as usize).map_or(u32::MAX, |era| era.order), m.order));
        let Some(first) = listed.iter().min_by_key(|m| (day(m.key), m.key)).map(|m| m.key) else { continue };
        // A member the seed links to itself keeps its own relation; the rest take the franchise's.
        let own_kind = |key: Key| direct.iter().find(|(held, _)| *held == key).map_or(kind, |&(_, k)| k);
        let versions = listed
            .iter()
            .map(|m| Version {
                key: m.key,
                kind: own_kind(m.key),
                group: Some(Group { franchise, era: m.era }),
            })
            .collect();
        runs.push((day(first), crate::plotrows::popularity(indexes, export, first), first, versions));
    }
    runs.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
    runs.into_iter().flat_map(|run| run.3).collect()
}

/// How a franchise is named in the row: its name, and the country most of its members were made in when more
/// than half share one — "Wallander (Sweden)" beside the British *Wallander*.
fn label(indexes: &Indexes, franchise: &Franchise) -> String {
    let mut counts: HashMap<[u8; 2], usize> = HashMap::new();
    for member in &franchise.members {
        let Some(record) = indexes.facts.as_ref().and_then(|f| f.get(member.key.1, member.key.0)) else {
            continue;
        };
        for &code in &record.countries {
            *counts.entry(code).or_default() += 1;
        }
    }
    let country = counts
        .into_iter()
        .filter(|&(_, n)| n * 2 > franchise.members.len())
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .and_then(|(code, _)| country_name(indexes, std::str::from_utf8(&code).ok()?));
    match country {
        Some(country) => format!("{} ({country})", franchise.name),
        None => franchise.name.clone(),
    }
}

/// A country's name from the store's entity table: the item carrying the ISO code, the lowest Q-id where
/// several do (Sweden rather than a historical state).
fn country_name(indexes: &Indexes, code: &str) -> Option<String> {
    let store = indexes.store.view();
    let (births, strings) = (store.birthplaces().ok()?, store.strings().ok()?);
    let (qids, names) = (store.column::<u32>("ent_qid").ok()?, store.column::<u32>("ent_name").ok()?);
    (0..qids.len())
        .filter(|&e| births.iso(e as u32).and_then(|s| strings.get(s)) == Some(code))
        .min_by_key(|&e| qids[e])
        .and_then(|e| strings.get(*names.get(e)?))
        .map(str::to_owned)
}

/// `/index/title`'s `otherVersions`: `[{type, id, kind}]` in the served order, or `None` for a title with none,
/// so the field is left out exactly when a client shows no row.
pub fn summary_json(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    seed: Key,
) -> Option<serde_json::Value> {
    let found = of_title(indexes, export, seed);
    (!found.is_empty()).then(|| {
        serde_json::json!(found
            .iter()
            .map(|v| serde_json::json!({ "type": type_name(v.key.0), "id": v.key.1, "kind": v.kind }))
            .collect::<Vec<_>>())
    })
}

/// `/index/versions/<type>/<id>.json`: the seed's other versions as cards, each with its `kind`, in the order
/// `of_title` serves them. A franchise's members also carry `group`: the franchise's id, its `label` for the
/// row, and the member's era. A version with no card is `{type, id, kind}`, as a franchise member is.
pub fn route_json(
    indexes: &Indexes,
    export: Option<&den_titlesearch::TitleIndex>,
    seed: Key,
) -> serde_json::Value {
    let mut labels: HashMap<u32, String> = HashMap::new();
    let versions: Vec<serde_json::Value> = of_title(indexes, export, seed)
        .into_iter()
        .map(|version| {
            let mut title = indexes
                .cards
                .as_ref()
                .and_then(|cards| cards.get(&version.key))
                .map(|card| crate::plotrows::title_json(indexes, version.key, card))
                .unwrap_or_else(
                    || serde_json::json!({ "type": type_name(version.key.0), "id": version.key.1 }),
                );
            title["kind"] = serde_json::json!(version.kind);
            if let Some(group) = &version.group {
                if let Some(franchise) = indexes.franchises.group(group.franchise) {
                    let label = labels.entry(group.franchise).or_insert_with(|| label(indexes, franchise));
                    let era = franchise.eras.get(group.era as usize);
                    title["group"] = serde_json::json!({
                        "id": franchise.id,
                        "name": franchise.name,
                        "label": label,
                        "era": era.map(|era| serde_json::json!({ "id": era.id, "name": era.name })),
                    });
                }
            }
            title
        })
        .collect();
    serde_json::json!({
        "seed": { "type": type_name(seed.0), "id": seed.1 },
        "total": versions.len(),
        "versions": versions,
    })
}

fn type_name(media: MediaType) -> &'static str {
    if media == MediaType::Tv {
        "series"
    } else {
        "movie"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows 0–3 are the Swedish film, the British series, *Young Wallander* and a sequel to the Swedish film
    /// with the same lead; row 4 plays the character without the name in its title.
    #[test]
    fn a_title_character_links_recast_versions_named_for_it() {
        let titles: HashMap<u32, &str> = HashMap::from([
            (0, "wallander mastermind"),
            (1, "wallander"),
            (2, "young wallander"),
            (3, "wallander the revenge"),
            (4, "one step behind"),
        ]);
        let words = |row: u32| titles[&row].split(' ').map(str::to_owned).collect();
        let wallander = [0, 1, 2, 3, 4];
        let links = pair_up([("kurt wallander", &wallander[..])].into_iter(), words, |a, b| (a, b) == (0, 3));
        assert_eq!(links[&2], [0, 1, 3], "Young Wallander is every recast's version");
        assert_eq!(links[&0], [1, 2], "one actor in both is a sequel, not a version");
        assert!(!links.contains_key(&4), "the name must be in both titles");

        let many: Vec<u32> = (0..=CHARACTER_CUTOFF as u32).collect();
        let big = pair_up(
            [("sherlock holmes", &many[..])].into_iter(),
            |_| HashSet::from(["sherlock".to_owned()]),
            |_, _| false,
        );
        assert!(big.is_empty(), "a character on more than {CHARACTER_CUTOFF} titles links nothing");
        let rank = pair_up(
            [("captain", &wallander[..])].into_iter(),
            |_| HashSet::from(["captain".to_owned()]),
            |_, _| false,
        );
        assert!(rank.is_empty(), "a rank is no name");
    }
}
