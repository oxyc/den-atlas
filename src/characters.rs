//! Titles that share a character, from TMDB's credits (oxyc/den-atlas#43).
//!
//! Two titles whose casts play the same named character are almost always the same franchise, and this
//! finds the links Wikidata's "part of the series" lacks — above all TV spin-offs and films of a series, which
//! share their people and their characters but rarely a series item. A name alone is noise ("Doctor",
//! "Max", "Mother"), so a link needs one of five kinds of evidence (`Tier`), measured against a hand-judged
//! sample: ~85% of the links it makes are the same franchise and ~96% are genuinely related.
//!
//! The credits are TMDB's (`/movie/{id}/credits`, `/tv/{id}/aggregate_credits`), fetched and kept on the box
//! by `tmdb.rs`; this builds the list from what it keeps, joined onto the store by its own `keys` column.
//! Character-link evidence is one of the uses TMDB's terms leave open, and the rules on that are at the top
//! of `tmdb.rs`. The tiers were measured on IMDb's principals, which name a title's first ten or so people;
//! TMDB lists a film's whole cast, so only the first `BILLED` of it are read (see `BILLED` for the parity).
//!
//! What stays resident is one short list per store row — a neighbour's row, the tier, and how rare the
//! shared name is — never cluster ids: crossovers chain over a thousand titles into one connected
//! component, so "in the same cluster" would say nothing.

use crate::ratings::Key;
use crate::tmdb::{Credits, Role};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// Billing positions read per title (TMDB's `order`, from 0). IMDb's principals — what the tiers were measured
/// on — carry about this many people per title, and a whole cast list multiplies the pairs sharing a generic
/// name without adding evidence.
pub const BILLED: u32 = 10;

/// A name shared by more titles than this is not evidence of anything (`Doctor`, `Himself`).
const MAX_DF: u32 = 80;
/// A one-word name on this many titles is generic.
const SINGLE_DF: u32 = 20;
/// A word in this many distinct names is generic (`john`, `police`).
const GENERIC_TOKEN: u32 = 150;
/// From this many titles on, a character is a common one — Holmes, Dracula, Scrooge — whose re-adaptations
/// share the name without following each other, so the link is weighted down (`CharacterLink::weight`).
const COMMON_DF: u32 = 30;
/// Billing positions that count as "top-billed" for the same-actor generic-name tier.
const TOP_BILLED: u32 = 3;
/// An actor in more corpus titles than this is skipped by the name-subset tier: a prolific character actor
/// plays "Jack" and "Jack Ryan" in unrelated films.
const SUBSET_MAX_TITLES: usize = 60;

/// Words that carry no identity in a name.
const STOP: &[&str] = &[
    "the", "le", "la", "les", "l", "el", "los", "las", "de", "del", "du", "des", "da", "di", "der", "die",
    "das", "of", "a", "an", "and", "in", "at", "on", "with", "s", "un", "une", "il", "lo", "von", "van",
    "mr", "mrs", "dr", "jr", "sr", "ii", "iii", "o", "d", "t", "j", "m", "st", "y", "e", "et", "und", "och",
    "og", "en", "to", "from",
];
/// The name-subset tier's own list: ranks and titles too, so "Captain Jean-Luc Picard" is read as the name
/// it contains.
const SUBSET_STOP: &[&str] = &[
    "the",
    "le",
    "la",
    "les",
    "l",
    "el",
    "de",
    "del",
    "du",
    "da",
    "di",
    "der",
    "die",
    "of",
    "a",
    "s",
    "von",
    "van",
    "jr",
    "sr",
    "o",
    "d",
    "j",
    "m",
    "st",
    "captain",
    "capt",
    "lieutenant",
    "lt",
    "commander",
    "detective",
    "det",
    "agent",
    "sergeant",
    "sgt",
    "officer",
    "inspector",
    "colonel",
    "col",
    "major",
    "general",
    "professor",
    "prof",
    "doctor",
    "chief",
    "deputy",
    "sheriff",
    "commissioner",
    "admiral",
    "ensign",
    "private",
    "corporal",
    "special",
];
/// Roles that are not characters, even played by one actor in both titles.
const NOT_A_CHARACTER: &[&str] = &[
    "self",
    "himself",
    "herself",
    "narrator",
    "host",
    "additional voices",
    "various",
    "various characters",
    "various roles",
    "themselves",
    "co host",
    "presenter",
    "additional cast",
    "guest",
    "voice",
    "cameo",
];

/// Why two titles are linked, strongest evidence first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// One actor plays the same specific character in both.
    SameActor,
    /// Two or more shared specific names, at least one of them a full name ("Jesse Pinkman").
    TwoNames,
    /// Three or more shared one-word names, or two billed in the top three of both.
    Singles,
    /// One actor, billed in the top three of both, plays the same generic name ("Max"), and the two casts
    /// share at least one other person.
    SameActorGeneric,
    /// One actor plays a name in one and a longer form of it in the other ("Jesse" / "Jesse Pinkman"), or
    /// the same name under a different rank or title ("Captain Jean-Luc Picard" / "Jean-Luc Picard"): with
    /// a two-word shorter name, or two actors doing so.
    SameActorSubset,
}

impl Tier {
    pub const ALL: [Tier; 5] =
        [Tier::SameActor, Tier::TwoNames, Tier::Singles, Tier::SameActorGeneric, Tier::SameActorSubset];

    /// Whether the same person plays the character in both titles — the audit's line between following a
    /// franchise and a recast or re-adaptation.
    pub fn same_actor(self) -> bool {
        matches!(self, Tier::SameActor | Tier::SameActorGeneric | Tier::SameActorSubset)
    }

    pub fn name(self) -> &'static str {
        match self {
            Tier::SameActor => "same_actor",
            Tier::TwoNames => "two_names",
            Tier::Singles => "singles",
            Tier::SameActorGeneric => "same_actor_generic",
            Tier::SameActorSubset => "same_actor_subset",
        }
    }
}

/// One neighbour of a title.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharacterLink {
    /// The neighbour's store row.
    pub row: u32,
    pub tier: Tier,
    /// How rare the rarest shared name is: 1.0 below `COMMON_DF` titles, `COMMON_DF / titles` from there
    /// (Holmes on 40 titles → 0.75). For `SameActorGeneric` the names are generic, so this says little and
    /// the tier is the evidence.
    pub weight: f32,
}

impl CharacterLink {
    pub fn same_actor(&self) -> bool {
        self.tier.same_actor()
    }

    /// How strongly the link says the two titles are one franchise, in 0..=1: the rarity `weight` in full
    /// when one actor plays the character in both or the two titles also share a series (`shares_series`),
    /// `RECAST` of it otherwise. The audit's line: a same-actor link is a franchise followed, a recast or
    /// re-adaptation (Les Misérables, A Christmas Carol) weaker evidence of one.
    pub fn strength(&self, shares_series: bool) -> f64 {
        let confirmed = self.same_actor() || shares_series;
        f64::from(self.weight) * if confirmed { 1.0 } else { RECAST }
    }
}

/// What a character link counts for, of a confirmed one's, when neither the actor nor a shared series
/// confirms it.
pub const RECAST: f64 = 0.5;

/// Every title's character neighbours, indexed BY STORE ROW.
pub struct CharacterIndex {
    /// Row `r`'s links are `links[starts[r]..starts[r + 1]]`.
    starts: Vec<u32>,
    links: Vec<CharacterLink>,
    /// Undirected links per tier, in `Tier::ALL` order.
    per_tier: [usize; 5],
    /// The names titles can be filtered by, and who plays each.
    named: NamedCharacters,
}

impl std::fmt::Debug for CharacterIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CharacterIndex")
            .field("rows", &self.rows())
            .field("linked", &self.linked())
            .field("links", &self.links())
            .finish()
    }
}

impl CharacterIndex {
    /// A store row's neighbours, strongest tier first and rarer names before common ones within a tier.
    /// Empty for a row with no links and for a row past the end.
    pub fn of(&self, row: usize) -> &[CharacterLink] {
        match (self.starts.get(row), self.starts.get(row + 1)) {
            (Some(&from), Some(&to)) => &self.links[from as usize..to as usize],
            _ => &[],
        }
    }

    /// The store rows this was built over.
    pub fn rows(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// How many rows have at least one neighbour.
    pub fn linked(&self) -> usize {
        self.starts.windows(2).filter(|w| w[1] > w[0]).count()
    }

    /// Links between two titles, each counted once.
    pub fn links(&self) -> usize {
        self.per_tier.iter().sum()
    }

    /// Links per tier, each counted once.
    pub fn per_tier(&self) -> impl Iterator<Item = (Tier, usize)> + '_ {
        Tier::ALL.into_iter().zip(self.per_tier)
    }

    /// The names titles can be filtered by (`filter.rs`).
    pub fn named(&self) -> &NamedCharacters {
        &self.named
    }

    /// Resident size, for the log: the links, and the names titles are filtered by.
    pub(crate) fn bytes(&self) -> usize {
        self.starts.len() * std::mem::size_of::<u32>()
            + self.links.len() * std::mem::size_of::<CharacterLink>()
            + self.named.bytes()
    }
}

/// The live holder: `tmdb.rs` swaps a new list in whenever the credits it keeps change.
pub struct Characters {
    index: RwLock<Option<Arc<CharacterIndex>>>,
    /// Whether the first build has finished, however it ended (`settled`). A holder made with `unsettled` is
    /// waiting for one; any other has nothing to wait for.
    settled: tokio::sync::watch::Sender<bool>,
}

impl Default for Characters {
    fn default() -> Self {
        Characters { index: RwLock::new(None), settled: tokio::sync::watch::Sender::new(true) }
    }
}

impl Characters {
    #[cfg(test)]
    pub fn with_index(index: CharacterIndex) -> Self {
        let holder = Characters::default();
        holder.set(index);
        holder
    }

    /// A holder whose first build is still to come: `settled` waits until `settle` says it has finished.
    pub fn unsettled() -> Self {
        Characters { settled: tokio::sync::watch::Sender::new(false), ..Characters::default() }
    }

    /// The current list; `None` until the first build lands.
    pub fn index(&self) -> Option<Arc<CharacterIndex>> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, index: CharacterIndex) {
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(index));
    }

    /// The first build has finished — with a list, or without one.
    pub fn settle(&self) {
        self.settled.send_replace(true);
    }

    /// Once the first build has finished. The index loads wait here, because More Like This reads the links
    /// and memoises what it ranks: a row ranked in the seconds before they landed would be served, without
    /// them, until the indexes were next released.
    pub async fn settled(&self) {
        // The sender lives in `self`, so the channel cannot close while this waits.
        let _ = self.settled.subscribe().wait_for(|settled| *settled).await;
    }
}

/// The line the log gets for a built list.
pub fn describe(index: &CharacterIndex) -> String {
    let tiers: Vec<String> = index.per_tier().map(|(tier, n)| format!("{} {n}", tier.name())).collect();
    format!(
        "{} links over {} of {} store rows ({}), {} filterable names, {:.1} MB resident",
        index.links(),
        index.linked(),
        index.rows(),
        tiers.join(", "),
        index.named().len(),
        index.bytes() as f64 / 1_000_000.0
    )
}

/// One credited, named role.
struct Principal {
    ordering: u32,
    nconst: u32,
    is_self: bool,
    /// Interned normalised names.
    names: Vec<u32>,
}

/// Each corpus title's store row and named roles, in billing order.
type Titles = Vec<(u32, Vec<Principal>)>;

/// A role played as oneself — IMDb's `self` category, which TMDB has no field for.
const SELF: &[&str] = &["self", "himself", "herself", "themselves"];

/// The neighbour list from the credits `tmdb.rs` keeps, joined onto a store's rows by its `keys` column.
pub fn build(view: &den_store::Store<'_>, credits: &HashMap<Key, Credits>) -> Result<CharacterIndex, String> {
    build_billed(view, credits, BILLED)
}

fn build_billed(
    view: &den_store::Store<'_>,
    credits: &HashMap<Key, Credits>,
    billed: u32,
) -> Result<CharacterIndex, String> {
    let keys = view.per_row::<u64>("keys").map_err(|e| e.to_string())?;
    let rows = keys.iter().enumerate().filter_map(|(row, &packed)| {
        let kept = credits.get(&(u8::from(packed >> 32 == 1), packed as u32))?;
        Some((u32::try_from(row).ok()?, kept.roles.as_slice()))
    });
    Ok(from_roles(keys.len(), rows, billed))
}

/// The neighbour list from each row's roles, reading the first `billed` of each.
fn from_roles<'a>(
    row_count: usize,
    rows: impl Iterator<Item = (u32, &'a [Role])>,
    billed: u32,
) -> CharacterIndex {
    let (titles, names) = parse(rows, billed);
    let named = NamedCharacters::build(&titles, &names, row_count);
    let edges = link(titles, &names);
    CharacterIndex { named, ..index(row_count, &edges) }
}

/// Every named role per corpus title, its names normalised and interned.
fn parse<'a>(rows: impl Iterator<Item = (u32, &'a [Role])>, billed: u32) -> (Titles, Vec<String>) {
    let mut interned: HashMap<String, u32> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let mut titles: Titles = Vec::new();
    for (row, roles) in rows {
        // One principal per credited person, as IMDb lists one: a series' aggregate credits give a person
        // one role per character they played, and those are one person's names, not several people's.
        let mut credited: Vec<((u32, u32), Vec<&str>)> = Vec::new();
        for role in roles.iter().filter(|r| r.order < billed) {
            let at = (role.order, role.person);
            match credited.iter_mut().find(|(held, _)| *held == at) {
                Some((_, characters)) => characters.push(&role.character),
                None => credited.push((at, vec![&role.character])),
            }
        }
        let mut principals: Vec<Principal> = Vec::new();
        for ((order, person), characters) in credited {
            let found = names_of(&characters);
            let is_self = !found.is_empty() && found.iter().all(|n| SELF.contains(&n.as_str()));
            let mut ids: Vec<u32> = found
                .into_iter()
                .map(|n| {
                    *interned.entry(n).or_insert_with_key(|n| {
                        names.push(n.clone());
                        (names.len() - 1) as u32
                    })
                })
                .collect();
            if ids.is_empty() {
                continue;
            }
            ids.sort_unstable();
            principals.push(Principal { ordering: order, nconst: person, is_self, names: ids });
        }
        if !principals.is_empty() {
            principals.sort_by_key(|p| (p.ordering, p.nconst));
            titles.push((row, principals));
        }
    }
    titles.sort_unstable_by_key(|(row, _)| *row);
    (titles, names)
}

/// A title's non-self names: best billing position (1-based over its named roles, self roles included) and
/// who plays each.
struct Cast {
    row: u32,
    named: HashMap<u32, (u32, Vec<u32>)>,
    /// Everyone credited with a named role, self roles included, sorted.
    people: Vec<u32>,
}

type Pair = (u32, u32);

fn pair(a: u32, b: u32) -> Pair {
    (a.min(b), a.max(b))
}

/// The links, each once, keyed by the two rows in order: the main rule first, then the same-actor
/// generic-name tier, then the name-subset tier, each adding only pairs the ones before did not link.
fn link(titles: Titles, names: &[String]) -> HashMap<Pair, (Tier, f32)> {
    let casts: Vec<Cast> = titles
        .iter()
        .map(|(row, principals)| {
            let mut named: HashMap<u32, (u32, Vec<u32>)> = HashMap::new();
            for (rank, p) in (1u32..).zip(principals) {
                if p.is_self {
                    continue;
                }
                for &n in &p.names {
                    let entry = named.entry(n).or_insert((u32::MAX, Vec::new()));
                    entry.0 = entry.0.min(rank);
                    if !entry.1.contains(&p.nconst) {
                        entry.1.push(p.nconst);
                    }
                }
            }
            let mut people: Vec<u32> = principals.iter().map(|p| p.nconst).collect();
            people.sort_unstable();
            people.dedup();
            Cast { row: *row, named, people }
        })
        .collect();

    let (df, token_count, kind) = classify(&titles, names);
    let tokens = |word: &str| token_count.get(word).copied().unwrap_or(0);
    let weight = |n: u32| {
        let df = df[n as usize];
        if df < COMMON_DF {
            1.0
        } else {
            COMMON_DF as f32 / df as f32
        }
    };

    // The main rule: pairs sharing a specific name.
    let mut postings: HashMap<u32, Vec<usize>> = HashMap::new();
    for (t, cast) in casts.iter().enumerate() {
        for &n in cast.named.keys() {
            if (2..=MAX_DF).contains(&df[n as usize]) && kind[n as usize] != Kind::Generic {
                postings.entry(n).or_default().push(t);
            }
        }
    }
    struct Shared {
        name: u32,
        rank: u32,
        same_actor: bool,
    }
    let mut candidates: HashMap<Pair, Vec<Shared>> = HashMap::new();
    for (&n, holders) in &postings {
        for (i, &a) in holders.iter().enumerate() {
            for &b in &holders[i + 1..] {
                let (ra, pa) = &casts[a].named[&n];
                let (rb, pb) = &casts[b].named[&n];
                candidates.entry(pair(casts[a].row, casts[b].row)).or_default().push(Shared {
                    name: n,
                    rank: (*ra).max(*rb),
                    same_actor: pa.iter().any(|p| pb.contains(p)),
                });
            }
        }
    }
    let mut edges: HashMap<Pair, (Tier, f32)> = HashMap::new();
    for (key, shared) in candidates {
        let tier = if shared.iter().any(|s| s.same_actor) {
            Tier::SameActor
        } else if shared.len() >= 2 && shared.iter().any(|s| kind[s.name as usize] == Kind::Multi) {
            Tier::TwoNames
        } else if shared.len() >= 3 || (shared.len() == 2 && shared.iter().all(|s| s.rank <= 3)) {
            Tier::Singles
        } else {
            continue;
        };
        let rarest = shared.iter().map(|s| weight(s.name)).fold(0.0, f32::max);
        edges.insert(key, (tier, rarest));
    }

    // Same actor, billed in the top three of both, same name even a generic one, and one more shared person.
    let skip: HashSet<&str> = NOT_A_CHARACTER.iter().copied().collect();
    let mut played: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (t, (_, principals)) in titles.iter().enumerate() {
        for (rank, p) in (1u32..).zip(principals) {
            if rank > TOP_BILLED || p.is_self {
                continue;
            }
            for &n in &p.names {
                if !skip.contains(names[n as usize].as_str()) {
                    played.entry((p.nconst, n)).or_default().push(t);
                }
            }
        }
    }
    let mut generic: HashMap<Pair, f32> = HashMap::new();
    for (&(_, n), holders) in &mut played {
        holders.sort_unstable();
        holders.dedup();
        for (i, &a) in holders.iter().enumerate() {
            for &b in &holders[i + 1..] {
                let key = pair(casts[a].row, casts[b].row);
                if edges.contains_key(&key) || shared_people(&casts[a].people, &casts[b].people) < 2 {
                    continue;
                }
                let w = generic.entry(key).or_insert(0.0);
                *w = w.max(weight(n));
            }
        }
    }
    edges.extend(generic.into_iter().map(|(key, w)| (key, (Tier::SameActorGeneric, w))));

    // Same actor, one name's words a subset of the other's.
    let subset_stop: HashSet<&str> = SUBSET_STOP.iter().copied().collect();
    let cores: Vec<Vec<&str>> = names
        .iter()
        .map(|name| {
            let mut core: Vec<&str> = name.split(' ').filter(|w| !subset_stop.contains(w)).collect();
            core.sort_unstable();
            core.dedup();
            core
        })
        .collect();
    let specific = |core: &[&str]| core.len() >= 2 && core.iter().any(|w| tokens(w) < GENERIC_TOKEN);
    // actor -> title -> (core, weight of the name it came from)
    let mut roles: HashMap<u32, HashMap<usize, Vec<(u32, f32)>>> = HashMap::new();
    for (t, (_, principals)) in titles.iter().enumerate() {
        for p in principals.iter().filter(|p| !p.is_self) {
            for &n in &p.names {
                if !cores[n as usize].is_empty() {
                    roles.entry(p.nconst).or_default().entry(t).or_default().push((n, weight(n)));
                }
            }
        }
    }
    // pair -> (actors matching, whether any shorter name has two words, best weight)
    let mut subset: HashMap<Pair, (Vec<u32>, bool, f32)> = HashMap::new();
    for (&actor, by_title) in &roles {
        if !(2..=SUBSET_MAX_TITLES).contains(&by_title.len()) {
            continue;
        }
        let mut ts: Vec<usize> = by_title.keys().copied().collect();
        ts.sort_unstable();
        for (i, &a) in ts.iter().enumerate() {
            for &b in &ts[i + 1..] {
                let key = pair(casts[a].row, casts[b].row);
                if edges.contains_key(&key) {
                    continue;
                }
                for &(x, wx) in &by_title[&a] {
                    for &(y, wy) in &by_title[&b] {
                        // The same name is the other tiers' evidence. Two names that differ only by a
                        // rank or title ("Captain Jean-Luc Picard" / "Jean-Luc Picard") have equal cores
                        // and count here, as a subset of each other.
                        if x == y {
                            continue;
                        }
                        let (cx, cy) = (&cores[x as usize], &cores[y as usize]);
                        let ((small, _), (big, w)) =
                            if cx.len() <= cy.len() { ((cx, wx), (cy, wy)) } else { ((cy, wy), (cx, wx)) };
                        if !small.iter().all(|word| big.contains(word)) || !specific(big) {
                            continue;
                        }
                        let entry = subset.entry(key).or_insert((Vec::new(), false, 0.0));
                        if !entry.0.contains(&actor) {
                            entry.0.push(actor);
                        }
                        entry.1 |= small.len() >= 2;
                        entry.2 = entry.2.max(w);
                    }
                }
            }
        }
    }
    edges.extend(
        subset
            .into_iter()
            .filter(|(_, (actors, two_words, _))| *two_words || actors.len() >= 2)
            .map(|(key, (_, _, w))| (key, (Tier::SameActorSubset, w))),
    );
    edges
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Generic,
    Single,
    Multi,
}

/// Each name's document frequency over titles (self roles aside), each word's count over the distinct names
/// played at all, and each name's kind.
fn classify<'a>(titles: &Titles, names: &'a [String]) -> (Vec<u32>, HashMap<&'a str, u32>, Vec<Kind>) {
    let mut df = vec![0u32; names.len()];
    for (_, principals) in titles {
        let played: HashSet<u32> =
            principals.iter().filter(|p| !p.is_self).flat_map(|p| p.names.iter().copied()).collect();
        for n in played {
            df[n as usize] += 1;
        }
    }
    let mut token_count: HashMap<&str, u32> = HashMap::new();
    for (n, name) in names.iter().enumerate() {
        if df[n] == 0 {
            continue;
        }
        let words: HashSet<&str> = name.split(' ').collect();
        for word in words {
            *token_count.entry(word).or_default() += 1;
        }
    }
    let tokens = |word: &str| token_count.get(word).copied().unwrap_or(0);
    let stop: HashSet<&str> = STOP.iter().copied().collect();
    let kind: Vec<Kind> = names
        .iter()
        .enumerate()
        .map(|(n, name)| {
            let words: Vec<&str> = name.split(' ').filter(|w| !stop.contains(w)).collect();
            match words[..] {
                [] => Kind::Generic,
                [word] if df[n] >= SINGLE_DF || tokens(word) >= GENERIC_TOKEN || word.chars().count() < 4 => {
                    Kind::Generic
                }
                [_] => Kind::Single,
                _ if words.iter().all(|w| tokens(w) >= GENERIC_TOKEN) => Kind::Generic,
                _ => Kind::Multi,
            }
        })
        .collect();
    (df, token_count, kind)
}

/// The characters a title can be filtered by (`filter.rs`, the `character` kind): a named role played in at
/// least two titles, neither generic nor one of `NOT_A_CHARACTER` — the same names the links are made of — and
/// the store rows playing each.
#[derive(Default)]
pub struct NamedCharacters {
    /// Normalised names (`normalise`), sorted, so a prefix is a range.
    names: Vec<Box<str>>,
    /// Name `i`'s rows are `rows[starts[i]..starts[i + 1]]`, ascending.
    starts: Vec<u32>,
    rows: Vec<u32>,
    /// One bit per store row: whether it plays any of these names — the titles a character is known for.
    known: Vec<u64>,
}

impl NamedCharacters {
    fn build(titles: &Titles, names: &[String], row_count: usize) -> NamedCharacters {
        let (df, _, kind) = classify(titles, names);
        let skip: HashSet<&str> = NOT_A_CHARACTER.iter().copied().collect();
        let kept = |n: u32| {
            df[n as usize] >= 2
                && kind[n as usize] != Kind::Generic
                && !skip.contains(names[n as usize].as_str())
        };
        let mut played: HashMap<u32, Vec<u32>> = HashMap::new();
        for (row, principals) in titles {
            let mut own: Vec<u32> =
                principals.iter().filter(|p| !p.is_self).flat_map(|p| p.names.iter().copied()).collect();
            own.sort_unstable();
            own.dedup();
            for n in own.into_iter().filter(|&n| kept(n)) {
                played.entry(n).or_default().push(*row);
            }
        }
        let mut ordered: Vec<(u32, Vec<u32>)> = played.into_iter().collect();
        ordered.sort_unstable_by(|a, b| names[a.0 as usize].cmp(&names[b.0 as usize]));
        let mut out = NamedCharacters {
            known: vec![0; row_count.div_ceil(64)],
            starts: vec![0],
            ..NamedCharacters::default()
        };
        for (n, mut rows) in ordered {
            rows.sort_unstable();
            rows.dedup();
            for &row in &rows {
                if let Some(word) = out.known.get_mut(row as usize / 64) {
                    *word |= 1 << (row % 64);
                }
            }
            out.names.push(names[n as usize].clone().into_boxed_str());
            out.rows.extend(rows);
            out.starts.push(out.rows.len() as u32);
        }
        out
    }

    /// The store rows playing a normalised name; empty for a name that is not one of these.
    pub fn rows(&self, name: &str) -> &[u32] {
        match self.names.binary_search_by(|n| (**n).cmp(name)) {
            Ok(i) => &self.rows[self.starts[i] as usize..self.starts[i + 1] as usize],
            Err(_) => &[],
        }
    }

    /// Every name starting with `prefix`, with its rows, in name order.
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a [u32])> + 'a {
        let from = self.names.partition_point(|n| (**n) < *prefix);
        self.names[from..].iter().take_while(move |n| n.starts_with(prefix)).enumerate().map(move |(i, n)| {
            let at = from + i;
            (&**n, &self.rows[self.starts[at] as usize..self.starts[at + 1] as usize])
        })
    }

    /// One bit per store row: whether it plays any of these names.
    pub fn known(&self) -> &[u64] {
        &self.known
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    fn bytes(&self) -> usize {
        self.names.iter().map(|n| n.len() + std::mem::size_of::<Box<str>>()).sum::<usize>()
            + (self.starts.len() + self.rows.len()) * std::mem::size_of::<u32>()
            + self.known.len() * std::mem::size_of::<u64>()
    }
}

/// A character name as the index holds it: the normalisation every role name went through — case,
/// diacritics, brackets, a trailing number, age and honorific words — so a query meets the names it read.
pub fn normalise(name: &str) -> String {
    norm_one(name)
}

/// How many people two sorted casts share.
fn shared_people(a: &[u32], b: &[u32]) -> usize {
    a.iter().filter(|p| b.binary_search(p).is_ok()).count()
}

/// The links as one list per store row, each link stored from both ends.
fn index(row_count: usize, edges: &HashMap<Pair, (Tier, f32)>) -> CharacterIndex {
    let mut directed: Vec<(u32, CharacterLink)> = Vec::with_capacity(edges.len() * 2);
    let mut per_tier = [0usize; 5];
    for (&(a, b), &(tier, weight)) in edges {
        per_tier[tier as usize] += 1;
        directed.push((a, CharacterLink { row: b, tier, weight }));
        directed.push((b, CharacterLink { row: a, tier, weight }));
    }
    directed.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb).then(a.tier.cmp(&b.tier)).then(b.weight.total_cmp(&a.weight)).then(a.row.cmp(&b.row))
    });
    let mut starts = vec![0u32; row_count + 1];
    for &(row, _) in &directed {
        starts[row as usize + 1] += 1;
    }
    let mut sum = 0;
    for start in &mut starts {
        sum += *start;
        *start = sum;
    }
    CharacterIndex {
        starts,
        links: directed.into_iter().map(|(_, link)| link).collect(),
        per_tier,
        named: NamedCharacters::default(),
    }
}

/// A role's character list as normalised names: "Bruce Wayne / Batman" gives both, "Joker (voice)" gives
/// "joker", "Young Anakin" gives "anakin".
fn names_of(characters: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for character in characters {
        for part in character.split('/').flat_map(split_dash) {
            let name = norm_one(part);
            if !name.is_empty() && !name.chars().all(char::is_numeric) && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Split on a hyphen with whitespace on both sides ("Tom - Older"), not inside a word ("Spider-Man").
fn split_dash(s: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut parts = Vec::new();
    let mut from = 0;
    for i in 1..chars.len().saturating_sub(1) {
        if chars[i].1 == '-' && chars[i - 1].1.is_whitespace() && chars[i + 1].1.is_whitespace() {
            parts.push(&s[from..chars[i].0]);
            from = chars[i + 1].0;
        }
    }
    parts.push(&s[from..]);
    parts
}

const PREFIXES: &[&str] = &[
    "young",
    "younger",
    "old",
    "older",
    "teen",
    "teenage",
    "adult",
    "little",
    "baby",
    "kid",
    "child",
    "elderly",
    "middle aged",
    "the voice of",
    "voice of",
];
const HONORIFICS: &[&str] = &["dr", "mr", "mrs", "ms", "miss", "sir", "mister"];
const SUFFIXES: &[&str] = &["voice", "s voice", "narrator", "uncredited", "archive footage"];

/// One name, normalised: bracketed notes dropped, diacritics folded, lowercased, punctuation to spaces, a
/// trailing number ("Guard #2") and age, honorific and voice markers removed.
///
/// Every role name of the corpus passes through here on each build, so it allocates only where a step has to
/// make new text: the folded, lowercased name, and the one output. Everything after the punctuation pass
/// only ever shortens the name, so it works on slices of it.
fn norm_one(s: &str) -> String {
    // `str::to_lowercase`, not per character: it lowercases a final sigma as `ς`.
    let mut s: String = drop_brackets(s)
        .nfkd()
        .filter(|&c| canonical_combining_class(c) == 0)
        .collect::<String>()
        .to_lowercase();
    if s.contains("'s voice") {
        s = s.replace("'s voice", "");
    }
    // Punctuation to spaces, and every run of whitespace to one space with none at either end — what
    // `split_whitespace().join(" ")` over the mapped name gives, in one pass. A `’` becomes a space here like
    // a `'`, so it needs no folding to one first.
    let mut words = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        let c = if c.is_alphanumeric() || c == '_' || c == '#' || c.is_whitespace() { c } else { ' ' };
        if c.is_whitespace() {
            gap = !words.is_empty();
        } else {
            if gap {
                words.push(' ');
                gap = false;
            }
            words.push(c);
        }
    }
    let mut s = drop_number(&words).trim();
    for _ in 0..2 {
        s = drop_prefix(s, PREFIXES);
        s = drop_prefix(s, HONORIFICS);
        s = drop_suffix(s);
    }
    s.trim().to_owned()
}

/// `(…)` and `[…]` spans replaced by a space; an unclosed bracket is kept.
fn drop_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        let close = match c {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(end) = close.and_then(|close| rest[1..].find(close)) {
            out.push(' ');
            rest = &rest[1 + end + 1..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// A trailing "#2", "# 2" or " 2" removed; digits glued to a word ("R2D2") kept.
fn drop_number(s: &str) -> &str {
    let head = s.trim_end_matches(char::is_numeric);
    if head.len() == s.len() {
        return s;
    }
    let trimmed = head.trim_end();
    if let Some(before) = trimmed.strip_suffix('#') {
        return before.trim_end();
    }
    if trimmed.len() < head.len() {
        return trimmed;
    }
    s
}

fn drop_prefix<'a>(s: &'a str, words: &[&str]) -> &'a str {
    for word in words {
        if let Some(rest) = s.strip_prefix(word) {
            if rest.starts_with(char::is_whitespace) {
                return rest.trim_start();
            }
        }
    }
    s
}

/// A trailing voice/narrator marker removed; the earliest-starting match wins, so "x s voice" loses
/// " s voice" rather than " voice".
fn drop_suffix(s: &str) -> &str {
    SUFFIXES
        .iter()
        .filter_map(|suffix| {
            let before = s.strip_suffix(suffix)?;
            before.ends_with(char::is_whitespace).then(|| before.trim_end())
        })
        .min_by_key(|before| before.len())
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(characters: &[&str]) -> Vec<String> {
        let mut names = names_of(characters);
        names.sort();
        names
    }

    #[test]
    fn normalises_names_the_way_the_audit_did() {
        assert_eq!(norm(&["Bruce Wayne / Batman"]), ["batman", "bruce wayne"]);
        assert_eq!(norm(&["Bruce Wayne/Batman"]), ["batman", "bruce wayne"]);
        assert_eq!(norm(&["Joker (voice)"]), ["joker"]);
        assert_eq!(norm(&["Joker [uncredited]"]), ["joker"]);
        assert_eq!(norm(&["Young Anakin"]), ["anakin"]);
        assert_eq!(norm(&["Young Dr. Jones"]), ["jones"], "an age marker, then an honorific");
        assert_eq!(norm(&["Guard #2"]), ["guard"]);
        assert_eq!(norm(&["Soldier 3"]), ["soldier"]);
        assert_eq!(norm(&["R2-D2"]), ["r2 d2"], "digits inside a name stay");
        assert_eq!(norm(&["Chloé"]), ["chloe"]);
        assert_eq!(norm(&["Gandalf's voice"]), ["gandalf"]);
        assert_eq!(norm(&["Tom - Older"]), ["older", "tom"]);
        assert_eq!(norm(&["Spider-Man"]), ["spider man"], "a hyphen inside a word is not a separator");
        assert_eq!(norm(&["Shrek", "Shrek (voice)"]), ["shrek"]);
        assert_eq!(norm(&["2"]), Vec::<String>::new(), "a bare number is no name");
        assert_eq!(norm(&["(voice)"]), Vec::<String>::new());
        // A shorter name is a different name, not a spelling of the longer one: they meet only through
        // the name-subset tier.
        assert_ne!(norm(&["Jesse"]), norm(&["Jesse Pinkman"]));
    }

    /// Synthetic credits. Each tuple: store row, billing position, person, character.
    fn credits(roles: &[(u32, u32, u32, &str)]) -> HashMap<u32, Vec<Role>> {
        let mut by_row: HashMap<u32, Vec<Role>> = HashMap::new();
        for &(row, order, person, character) in roles {
            by_row.entry(row).or_default().push(Role { order, person, character: character.into() });
        }
        by_row
    }

    fn built(roles: &[(u32, u32, u32, &str)]) -> CharacterIndex {
        let by_row = credits(roles);
        let row_count = roles.iter().map(|r| r.0 as usize + 1).max().unwrap_or(0).max(10);
        from_roles(row_count, by_row.iter().map(|(&row, roles)| (row, roles.as_slice())), BILLED)
    }

    fn tier(index: &CharacterIndex, a: usize, b: u32) -> Option<Tier> {
        index.of(a).iter().find(|link| link.row == b).map(|link| link.tier)
    }

    #[test]
    fn one_actor_playing_one_specific_character_links() {
        let index = built(&[(1, 1, 100, "Walter White"), (2, 1, 100, "Walter White")]);
        assert_eq!(tier(&index, 1, 2), Some(Tier::SameActor));
        assert_eq!(tier(&index, 2, 1), Some(Tier::SameActor), "a link is stored from both ends");
        assert!(index.of(1)[0].same_actor());
        assert_eq!(index.links(), 1);
    }

    #[test]
    fn two_shared_names_with_a_full_name_link_across_a_recast() {
        let index = built(&[
            (1, 1, 100, "Jesse Pinkman"),
            (1, 2, 101, "Saul"),
            (2, 1, 200, "Jesse Pinkman"),
            (2, 2, 201, "Saul"),
        ]);
        assert_eq!(tier(&index, 1, 2), Some(Tier::TwoNames));
        assert!(!index.of(1)[0].same_actor());
        // One full name alone, recast, is not enough.
        let one = built(&[(1, 1, 100, "Jesse Pinkman"), (2, 1, 200, "Jesse Pinkman")]);
        assert_eq!(tier(&one, 1, 2), None);
    }

    #[test]
    fn two_top_billed_single_names_link() {
        let index = built(&[
            (1, 1, 100, "Frasier"),
            (1, 2, 101, "Niles"),
            (2, 1, 200, "Frasier"),
            (2, 3, 201, "Niles"),
        ]);
        assert_eq!(tier(&index, 1, 2), Some(Tier::Singles));
        // Billed below the top three in one of them, two single names are not enough ...
        let low = built(&[
            (1, 1, 100, "Frasier"),
            (1, 2, 101, "Niles"),
            (2, 1, 200, "Frasier"),
            (2, 2, 202, "Someone Else"),
            (2, 3, 203, "Another Person"),
            (2, 4, 201, "Niles"),
        ]);
        assert_eq!(tier(&low, 1, 2), None);
        // ... and three are, wherever they are billed.
        let three = built(&[
            (1, 1, 100, "Frasier"),
            (1, 2, 101, "Niles"),
            (1, 3, 102, "Daphne"),
            (2, 5, 200, "Frasier"),
            (2, 6, 201, "Niles"),
            (2, 7, 202, "Daphne"),
        ]);
        assert_eq!(tier(&three, 1, 2), Some(Tier::Singles));
    }

    /// "Max" is under four letters and "Doctor" is on twenty-odd titles: both generic, so two recast
    /// titles sharing them are not linked, where a specific pair of names would be.
    #[test]
    fn generic_names_do_not_link() {
        let mut roles =
            vec![(1, 1, 100, "Max"), (1, 2, 101, "Doctor"), (2, 1, 200, "Max"), (2, 2, 201, "Doctor")];
        for t in 10..35 {
            roles.push((t, 1, 1000 + t, "Doctor"));
        }
        let index = built(&roles);
        assert_eq!(tier(&index, 1, 2), None);
        assert!(index.of(1).is_empty());
    }

    #[test]
    fn one_top_billed_actor_in_a_generic_role_links_only_with_another_shared_person() {
        let alone = built(&[(1, 1, 100, "Max"), (2, 1, 100, "Max")]);
        assert_eq!(tier(&alone, 1, 2), None, "one shared person is not enough for a generic name");
        let index =
            built(&[(1, 1, 100, "Max"), (1, 5, 300, "Bartender"), (2, 1, 100, "Max"), (2, 6, 300, "Pilot")]);
        assert_eq!(tier(&index, 1, 2), Some(Tier::SameActorGeneric));
        // Billed fourth in one of them, it is not the lead's role.
        let low = built(&[
            (1, 1, 100, "Max"),
            (1, 2, 300, "Bartender"),
            (2, 1, 301, "Someone"),
            (2, 2, 300, "Pilot"),
            (2, 3, 302, "Another"),
            (2, 4, 100, "Max"),
        ]);
        assert_eq!(tier(&low, 1, 2), None);
    }

    /// "Jesse" and "Jesse Pinkman" by one actor: a one-word shorter name needs a second actor doing the
    /// same; a two-word one does not.
    #[test]
    fn one_actor_playing_a_longer_form_of_the_name_links() {
        let one = built(&[(1, 1, 100, "Jesse"), (2, 1, 100, "Jesse Pinkman")]);
        assert_eq!(tier(&one, 1, 2), None);
        let two_actors = built(&[
            (1, 1, 100, "Jesse"),
            (1, 2, 101, "Skyler"),
            (2, 1, 100, "Jesse Pinkman"),
            (2, 2, 101, "Skyler White"),
        ]);
        assert_eq!(tier(&two_actors, 1, 2), Some(Tier::SameActorSubset));
        let two_words =
            built(&[(1, 1, 100, "Jean-Luc Picard"), (2, 1, 100, "Jean-Luc Picard of the Enterprise")]);
        assert_eq!(tier(&two_words, 1, 2), Some(Tier::SameActorSubset));
    }

    /// One actor as "Captain Jean-Luc Picard" and as "Jean-Luc Picard": the names differ only by rank, which
    /// is what this tier is for. A rank in front of a one-word surname is still too little to go on.
    #[test]
    fn a_name_that_differs_only_by_rank_links_the_same_actor() {
        let index = built(&[(1, 1, 100, "Captain Jean-Luc Picard"), (2, 1, 100, "Jean-Luc Picard")]);
        assert_eq!(tier(&index, 1, 2), Some(Tier::SameActorSubset));
        let near_miss = built(&[(1, 1, 100, "Detective Jones"), (2, 1, 100, "Sergeant Jones")]);
        assert_eq!(tier(&near_miss, 1, 2), None, "one surname under two ranks is not a character");
        let recast = built(&[(1, 1, 100, "Captain Jean-Luc Picard"), (2, 1, 200, "Jean-Luc Picard")]);
        assert_eq!(tier(&recast, 1, 2), None, "the tier needs the same actor");
    }

    /// Holmes and Watson on forty titles each still link two of them, weighted down; a rare name is not.
    #[test]
    fn a_common_character_is_weighted_down() {
        let mut roles = Vec::new();
        for t in 1..=40 {
            roles.push((t, 1, 1000 + t, "Sherlock Holmes"));
            roles.push((t, 2, 2000 + t, "John Watson"));
        }
        let index = built(&roles);
        let link = index.of(1).iter().find(|l| l.row == 2).expect("two Holmes films link");
        assert_eq!(link.tier, Tier::TwoNames);
        assert!((link.weight - 0.75).abs() < 1e-6, "{}", link.weight);
        let rare = built(&[(1, 1, 100, "Walter White"), (2, 1, 100, "Walter White")]);
        assert_eq!(rare.of(1)[0].weight, 1.0);
    }

    /// A same-actor link counts in full; a recast one half, unless a shared series confirms it; and a common
    /// name's weight scales either.
    #[test]
    fn a_links_strength_is_full_for_the_same_actor_or_a_shared_series_and_half_for_a_recast() {
        let link = |tier, weight| CharacterLink { row: 0, tier, weight };
        assert_eq!(link(Tier::SameActor, 1.0).strength(false), 1.0);
        assert_eq!(link(Tier::SameActorSubset, 1.0).strength(false), 1.0);
        assert_eq!(link(Tier::TwoNames, 1.0).strength(false), RECAST);
        assert_eq!(link(Tier::TwoNames, 1.0).strength(true), 1.0, "a shared series confirms a recast");
        assert_eq!(link(Tier::TwoNames, 0.75).strength(false), 0.75 * RECAST, "Holmes, recast");
        assert_eq!(link(Tier::SameActor, 0.75).strength(false), 0.75);
    }

    #[test]
    fn a_title_with_no_principals_has_no_neighbours() {
        let index = built(&[(1, 1, 100, "Walter White"), (2, 1, 100, "Walter White")]);
        assert!(index.of(3).is_empty());
        assert!(index.of(0).is_empty());
        assert!(index.of(10_000).is_empty(), "a row past the end");
        assert_eq!(index.linked(), 2);
    }

    /// TMDB has no `self` category: a role named Himself is one, and its name is never a character.
    #[test]
    fn a_role_played_as_oneself_is_not_a_character() {
        let index = built(&[
            (1, 1, 100, "Himself"),
            (1, 2, 101, "Self"),
            (2, 1, 100, "Himself"),
            (2, 2, 101, "Self"),
        ]);
        assert!(index.of(1).is_empty(), "{:?}", index.of(1));
    }

    /// A series' aggregate credits give one person a role per character; they are that person's names, read
    /// together as IMDb read one principal's list.
    #[test]
    fn one_persons_several_roles_are_one_principal() {
        let index = built(&[
            (1, 1, 100, "Bruce Wayne"),
            (1, 1, 100, "Batman"),
            (1, 2, 101, "Alfred Pennyworth"),
            (2, 1, 100, "Batman"),
            (2, 3, 101, "Alfred Pennyworth"),
        ]);
        assert_eq!(tier(&index, 1, 2), Some(Tier::SameActor));
        // Past `BILLED` a role is not read at all.
        let deep = built(&[(1, BILLED, 100, "Walter White"), (2, BILLED, 100, "Walter White")]);
        assert!(deep.of(1).is_empty());
    }

    /// Over a real store: its `keys` column names the rows (movie 1 is row 0, movie 2 row 1).
    #[test]
    fn builds_against_a_real_store() {
        let dir = std::env::temp_dir().join(format!("den-atlas-characters-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        let credits = |character: &str| Credits {
            fetched: 0,
            roles: vec![Role { order: 0, person: 100, character: character.into() }],
        };
        let kept = HashMap::from([
            ((0, 1), credits("Walter White")),
            ((0, 2), credits("Walter White")),
            ((0, 999_999), credits("Walter White")),
        ]);
        let index = build(&mapped.view(), &kept).unwrap();
        assert_eq!(index.rows(), 12);
        assert_eq!(index.links(), 1, "the title the store lacks links nothing: {index:?}");
        assert_eq!(tier(&index, 0, 1), Some(Tier::SameActor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole corpus, for measuring: `STORE=<den-….store> CREDITS=<tmdb-credits.tsv> cargo test --release
    /// characters::tests::measure -- --ignored --nocapture`, with `CHARACTERS_BILLED` to read another depth.
    /// With `CHARACTERS_EDGES_OUT` set, writes one `tt… tt… tier weight` line per link, by the store's IMDb
    /// ids, for comparing with another build.
    #[test]
    #[ignore = "needs a real store and TMDB's credits, which are never committed"]
    fn measure() {
        let store = std::env::var("STORE").expect("STORE names a store");
        let mapped = crate::store::MappedStore::open(std::path::Path::new(&store)).unwrap();
        let view = mapped.view();
        let credits = crate::tmdb::read_credits(std::path::Path::new(&std::env::var("CREDITS").unwrap()), 0);
        let billed = std::env::var("CHARACTERS_BILLED").ok().and_then(|b| b.parse().ok()).unwrap_or(BILLED);
        let started = std::time::Instant::now();
        let index = build_billed(&view, &credits, billed).unwrap();
        println!("billed {billed}, built in {:.2}s: {}", started.elapsed().as_secs_f64(), describe(&index));
        if let Ok(out) = std::env::var("CHARACTERS_EDGES_OUT") {
            let imdb = view.per_row::<u32>("imdb").unwrap();
            let strings = view.strings().unwrap();
            let tt = |row: usize| strings.get(imdb[row]).unwrap_or("-").to_owned();
            let mut lines = String::new();
            for row in 0..index.rows() {
                for link in index.of(row).iter().filter(|l| l.row as usize > row) {
                    let (a, b) = (tt(row), tt(link.row as usize));
                    lines.push_str(&format!("{a} {b} {} {}\n", link.tier.name(), link.weight));
                }
            }
            std::fs::write(out, lines).unwrap();
        }
    }
}
