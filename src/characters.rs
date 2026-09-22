//! Titles that share a character, from IMDb's public `title.principals` dump (oxyc/den-atlas#43).
//!
//! Two titles whose casts play the same named character are almost always the same franchise, and this
//! finds the links Wikidata's "part of the series" lacks — above all TV spin-offs and films of a series, which
//! share their people and their characters but rarely a series item. A name alone is noise ("Doctor",
//! "Max", "Mother"), so a link needs one of five kinds of evidence (`Tier`), measured against a hand-judged
//! sample: ~85% of the links it makes are the same franchise and ~96% are genuinely related.
//!
//! IMDb's licence allows this data to be joined at runtime and never shipped, so atlas builds the list on the
//! box, the way it joins `title.ratings` (`ratings.rs`): the store's `imdb` column names the rows, and
//! everything the dump says about a title the corpus lacks is dropped as it is read. The dump is ~780 MB
//! gzipped, so unlike the ratings it is STREAMED through the gunzip and the filter rather than downloaded
//! whole, and what survives the filter (~24 MB) is kept in `CACHE_DIR` when there is one, so a restart
//! rebuilds from that instead of downloading the dump again. The list is refreshed weekly.
//!
//! What stays resident is one short list per store row — a neighbour's row, the tier, and how rare the
//! shared name is — never cluster ids: crossovers chain over a thousand titles into one connected
//! component, so "in the same cluster" would say nothing.

use crate::ratings::{imdb_rows, tconst};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// IMDb's daily dump: public, unauthenticated.
pub const PRINCIPALS_URL: &str = "https://datasets.imdbws.com/title.principals.tsv.gz";
/// The dump's header line, checked for the same reason `ratings.rs` checks its own: a moved column would
/// otherwise be read as something else and link titles on it.
const HEADER: &str = "tconst\tordering\tnconst\tcategory\tjob\tcharacters";
/// A character list changes slowly — new titles, the odd correction — and the dump is large.
const REFRESH_EVERY: Duration = Duration::from_secs(7 * 24 * 3600);
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The filtered rows, in `CACHE_DIR`: IMDb's own lines for the corpus's titles, in the dump's format.
const CACHE_FILE: &str = "imdb-principals.tsv";

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
    // Read by the billboard's fit and More Like This once #43 wires the list into scoring.
    #[allow(dead_code)]
    pub fn same_actor(&self) -> bool {
        self.tier.same_actor()
    }
}

/// Every title's character neighbours, indexed BY STORE ROW.
pub struct CharacterIndex {
    /// Row `r`'s links are `links[starts[r]..starts[r + 1]]`.
    starts: Vec<u32>,
    links: Vec<CharacterLink>,
    /// Undirected links per tier, in `Tier::ALL` order.
    per_tier: [usize; 5],
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

    /// Resident size of the two arrays, for the log.
    fn bytes(&self) -> usize {
        self.starts.len() * std::mem::size_of::<u32>()
            + self.links.len() * std::mem::size_of::<CharacterLink>()
    }
}

pub struct Characters {
    index: RwLock<Option<Arc<CharacterIndex>>>,
    client: reqwest::Client,
    url: String,
    /// The store the dump is filtered against, by path: opened per build, never held (see `ratings.rs`).
    store: PathBuf,
    /// Where the filtered rows are kept between restarts; `None` keeps nothing and downloads on every boot.
    cache: Option<PathBuf>,
}

impl Characters {
    pub fn new(store: PathBuf, url: &str, cache_dir: Option<&Path>) -> Result<Self, reqwest::Error> {
        // No overall timeout: the body is ~780 MB. A stalled read still fails.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()?;
        Ok(Characters {
            index: RwLock::new(None),
            client,
            url: url.to_owned(),
            store,
            cache: cache_dir.map(|dir| dir.join(CACHE_FILE)),
        })
    }

    #[cfg(test)]
    pub fn with_index(index: CharacterIndex) -> Self {
        Characters {
            index: RwLock::new(Some(Arc::new(index))),
            client: reqwest::Client::new(),
            url: String::new(),
            store: PathBuf::new(),
            cache: None,
        }
    }

    /// The current list; `None` until the first build lands.
    pub fn index(&self) -> Option<Arc<CharacterIndex>> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Download the dump, filter it to the corpus as it streams, keep the filtered rows, build the list and
    /// swap it in. Returns a line for the log.
    pub async fn refresh(&self) -> Result<String, String> {
        let started = Instant::now();
        let store = self.store.clone();
        let (rows, row_count) = tokio::task::spawn_blocking(move || {
            let mapped = crate::store::MappedStore::open(&store)?;
            imdb_rows(&mapped.view())
        })
        .await
        .map_err(|e| format!("characters task: {e}"))??;
        let rows = Arc::new(rows);
        let (filtered, downloaded) = self.download(Arc::clone(&rows)).await?;
        if let Some(path) = &self.cache {
            if let Err(e) = keep(path, &filtered) {
                eprintln!("imdb characters: could not keep the filtered rows at {} ({e})", path.display());
            }
        }
        let source = format!("{:.0} MB downloaded", downloaded as f64 / 1_000_000.0);
        self.build_from((rows, row_count), filtered, source, started).await
    }

    /// Build from the kept rows when they are younger than `REFRESH_EVERY`; how old they were, or `None`
    /// when there were none to use.
    async fn build_from_kept(&self) -> Option<Duration> {
        let path = self.cache.as_ref()?;
        let age = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        let age = SystemTime::now().duration_since(age).unwrap_or_default();
        if age >= REFRESH_EVERY {
            return None;
        }
        let started = Instant::now();
        let (store, path) = (self.store.clone(), path.clone());
        let read = tokio::task::spawn_blocking(move || {
            let mapped = crate::store::MappedStore::open(&store)?;
            let rows = imdb_rows(&mapped.view())?;
            let filtered = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok::<_, String>((rows, filtered))
        })
        .await;
        let built = match read {
            Ok(Ok(((rows, row_count), filtered))) => {
                let source = format!("kept rows {}h old", age.as_secs() / 3600);
                self.build_from((Arc::new(rows), row_count), filtered, source, started).await
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("characters task: {e}")),
        };
        match built {
            Ok(line) => {
                eprintln!("{line}");
                Some(age)
            }
            Err(e) => {
                eprintln!("imdb characters: the kept rows did not build ({e}); downloading");
                None
            }
        }
    }

    async fn build_from(
        &self,
        (rows, row_count): (Arc<HashMap<u32, u32>>, usize),
        filtered: Vec<u8>,
        source: String,
        started: Instant,
    ) -> Result<String, String> {
        let kept = filtered.len();
        let index = tokio::task::spawn_blocking(move || build(&rows, row_count, filtered.as_slice()))
            .await
            .map_err(|e| format!("characters task: {e}"))??;
        let tiers: Vec<String> = index.per_tier().map(|(tier, n)| format!("{} {n}", tier.name())).collect();
        let line = format!(
            "imdb characters: {} links over {} of {} store rows ({}), {:.1} MB of rows from {source}, \
             {:.1} MB resident, in {:.1}s",
            index.links(),
            index.linked(),
            index.rows(),
            tiers.join(", "),
            kept as f64 / 1_000_000.0,
            index.bytes() as f64 / 1_000_000.0,
            started.elapsed().as_secs_f64()
        );
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(index));
        Ok(line)
    }

    /// The dump, gunzipped and filtered on a blocking thread as the body arrives, so neither the 780 MB
    /// body nor the multi-GB text is ever held. The filtered rows, and the bytes downloaded.
    async fn download(&self, rows: Arc<HashMap<u32, u32>>) -> Result<(Vec<u8>, u64), String> {
        let mut resp = self.client.get(&self.url).send().await.map_err(|e| format!("{}: {e}", self.url))?;
        if !resp.status().is_success() {
            return Err(format!("{}: HTTP {}", self.url, resp.status()));
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(64);
        let filtering = tokio::task::spawn_blocking(move || {
            let gz = flate2::read::GzDecoder::new(ChannelReader { rx, pending: bytes::Bytes::new() });
            let mut out = Vec::new();
            filter(&rows, std::io::BufReader::new(gz), &mut out).map(|_| out)
        });
        let mut downloaded = 0u64;
        let mut fetched = Ok(());
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    downloaded += chunk.len() as u64;
                    // The filter stopped early; its own error says why.
                    if tx.send(chunk).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    fetched = Err(format!("{}: {e}", self.url));
                    break;
                }
            }
        }
        drop(tx);
        let filtered = filtering.await.map_err(|e| format!("characters task: {e}"))?;
        // A body cut short reads to the filter as a truncated gzip; the network error is the real cause.
        fetched?;
        Ok((filtered?, downloaded))
    }
}

/// The response body as a `Read`, for the gunzip on the blocking side.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    pending: bytes::Bytes,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            match self.rx.blocking_recv() {
                Some(chunk) => self.pending = chunk,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending = self.pending.slice(n..);
        Ok(n)
    }
}

/// Written beside and renamed over, so a crash mid-write never leaves a half file that looks fresh.
fn keep(path: &Path, filtered: &[u8]) -> std::io::Result<()> {
    let partial = path.with_extension("tsv.partial");
    std::fs::write(&partial, filtered)?;
    std::fs::rename(&partial, path)
}

/// Build from the kept rows if they are fresh, else download; then weekly. A failed download retries hourly
/// and the previous list keeps serving. Not a boot gate: nothing a request answers waits on this list.
pub async fn refresh_forever(characters: Arc<Characters>) {
    let mut wait = match characters.build_from_kept().await {
        Some(age) => REFRESH_EVERY.saturating_sub(age),
        None => Duration::ZERO,
    };
    loop {
        tokio::time::sleep(wait).await;
        wait = match characters.refresh().await {
            Ok(line) => {
                eprintln!("{line}");
                REFRESH_EVERY
            }
            Err(e) => {
                let serving =
                    if characters.index().is_some() { "keeping the previous list" } else { "no list yet" };
                eprintln!("imdb characters refresh failed ({e}); {serving}; retrying in an hour");
                RETRY_AFTER
            }
        };
    }
}

/// The dump's lines that can matter: a corpus title, an on-screen role, a character named. Written with the
/// header, so the kept file reads back through `build` like the dump itself. How many lines were kept.
pub(crate) fn filter(
    rows: &HashMap<u32, u32>,
    tsv: impl BufRead,
    out: &mut Vec<u8>,
) -> Result<usize, String> {
    let mut lines = tsv.lines();
    check_header(lines.next())?;
    out.extend_from_slice(HEADER.as_bytes());
    out.push(b'\n');
    let mut kept = 0;
    for line in lines {
        let line = line.map_err(|e| format!("reading the dump: {e}"))?;
        let mut fields = line.split('\t');
        let Some(id) = fields.next().and_then(tconst) else { continue };
        if !rows.contains_key(&id) {
            continue;
        }
        let (Some(_), Some(_), Some(category), Some(_), Some(characters)) =
            (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !matches!(category, "actor" | "actress" | "self") || characters == "\\N" {
            continue;
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
        kept += 1;
    }
    if kept == 0 {
        return Err(format!("the dump named a character for none of the store's {} titles", rows.len()));
    }
    Ok(kept)
}

fn check_header(head: Option<std::io::Result<String>>) -> Result<(), String> {
    match head {
        Some(Ok(head)) if head.trim_end() == HEADER => Ok(()),
        Some(Ok(head)) => Err(format!("unexpected header {head:?}, wanted {HEADER:?}")),
        Some(Err(e)) => Err(format!("reading the dump: {e}")),
        None => Err("the dump was empty".to_owned()),
    }
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

/// The neighbour list from `title.principals` lines — the dump itself or `filter`'s output — against a
/// store's IMDb ids (`imdb_rows`).
pub(crate) fn build(
    rows: &HashMap<u32, u32>,
    row_count: usize,
    tsv: impl BufRead,
) -> Result<CharacterIndex, String> {
    let (titles, names) = parse(rows, tsv)?;
    let edges = link(titles, &names);
    Ok(index(row_count, &edges))
}

/// Every named role per corpus title, its names normalised and interned.
fn parse(rows: &HashMap<u32, u32>, tsv: impl BufRead) -> Result<(Titles, Vec<String>), String> {
    let mut lines = tsv.lines();
    check_header(lines.next())?;
    let mut interned: HashMap<String, u32> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let mut by_row: HashMap<u32, Vec<Principal>> = HashMap::new();
    for line in lines {
        let line = line.map_err(|e| format!("reading the dump: {e}"))?;
        let f: Vec<&str> = line.split('\t').collect();
        let [tt, ordering, nconst, category, _job, characters] = f[..] else { continue };
        let Some(&row) = tconst(tt).and_then(|id| rows.get(&id)) else { continue };
        let is_self = match category {
            "actor" | "actress" => false,
            "self" => true,
            _ => continue,
        };
        let (Ok(ordering), Some(nconst)) = (ordering.parse(), person(nconst)) else { continue };
        let Ok(characters) = serde_json::from_str::<Vec<String>>(characters) else { continue };
        let mut ids: Vec<u32> = names_of(&characters)
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
        by_row.entry(row).or_default().push(Principal { ordering, nconst, is_self, names: ids });
    }
    let mut titles: Titles = by_row.into_iter().collect();
    titles.sort_unstable_by_key(|(row, _)| *row);
    for (_, principals) in &mut titles {
        principals.sort_by_key(|p| (p.ordering, p.nconst));
    }
    Ok((titles, names))
}

/// The numeric part of a person id.
fn person(id: &str) -> Option<u32> {
    id.strip_prefix("nm").filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))?.parse().ok()
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

    // Document frequency of each name over titles, and of each word over distinct names.
    let mut df = vec![0u32; names.len()];
    for cast in &casts {
        for &n in cast.named.keys() {
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
    let weight = |n: u32| {
        let df = df[n as usize];
        if df < COMMON_DF {
            1.0
        } else {
            COMMON_DF as f32 / df as f32
        }
    };
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
    CharacterIndex { starts, links: directed.into_iter().map(|(_, link)| link).collect(), per_tier }
}

/// A role's character list as normalised names: "Bruce Wayne / Batman" gives both, "Joker (voice)" gives
/// "joker", "Young Anakin" gives "anakin".
fn names_of(characters: &[String]) -> Vec<String> {
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
fn norm_one(s: &str) -> String {
    let s: String = drop_brackets(s)
        .nfkd()
        .filter(|&c| canonical_combining_class(c) == 0)
        .collect::<String>()
        .to_lowercase()
        .replace("'s voice", "")
        .replace('\u{2019}', "'");
    let s: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '#' || c.is_whitespace() { c } else { ' ' })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = drop_number(&s).trim().to_owned();
    for _ in 0..2 {
        s = drop_prefix(&s, PREFIXES);
        s = drop_prefix(&s, HONORIFICS);
        s = drop_suffix(&s);
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

fn drop_prefix(s: &str, words: &[&str]) -> String {
    for word in words {
        if let Some(rest) = s.strip_prefix(word) {
            if rest.starts_with(char::is_whitespace) {
                return rest.trim_start().to_owned();
            }
        }
    }
    s.to_owned()
}

/// A trailing voice/narrator marker removed; the earliest-starting match wins, so "x s voice" loses
/// " s voice" rather than " voice".
fn drop_suffix(s: &str) -> String {
    SUFFIXES
        .iter()
        .filter_map(|suffix| {
            let before = s.strip_suffix(suffix)?;
            before.ends_with(char::is_whitespace).then(|| before.trim_end())
        })
        .min_by_key(|before| before.len())
        .unwrap_or(s)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn norm(characters: &[&str]) -> Vec<String> {
        let mut names = names_of(&characters.iter().map(|c| c.to_string()).collect::<Vec<_>>());
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

    /// Rows for a synthetic dump. Each tuple: title number, billing position, person number, character.
    fn dump(roles: &[(u32, u32, u32, &str)]) -> String {
        let mut s = format!("{HEADER}\n");
        for &(title, ordering, person, character) in roles {
            let chars = serde_json::to_string(&[character]).unwrap();
            s.push_str(&format!("tt{title:07}\t{ordering}\tnm{person:07}\tactor\t\\N\t{chars}\n"));
        }
        s
    }

    /// Title `tt000000n` is store row n, for every title named in the dump.
    fn built(roles: &[(u32, u32, u32, &str)]) -> CharacterIndex {
        let rows: HashMap<u32, u32> = roles.iter().map(|r| (r.0, r.0)).collect();
        let row_count = roles.iter().map(|r| r.0 as usize + 1).max().unwrap_or(0).max(10);
        build(&rows, row_count, Cursor::new(dump(roles))).expect("a synthetic dump builds")
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

    #[test]
    fn a_title_with_no_principals_has_no_neighbours() {
        let index = built(&[(1, 1, 100, "Walter White"), (2, 1, 100, "Walter White")]);
        assert!(index.of(3).is_empty());
        assert!(index.of(0).is_empty());
        assert!(index.of(10_000).is_empty(), "a row past the end");
        assert_eq!(index.linked(), 2);
    }

    #[test]
    fn filters_to_the_corpus_and_to_named_on_screen_roles() {
        let tsv = format!(
            "{HEADER}\n\
             tt0000001\t1\tnm0000100\tactor\t\\N\t[\"Walter White\"]\n\
             tt0000001\t2\tnm0000101\tdirector\t\\N\t\\N\n\
             tt0000001\t3\tnm0000102\tactress\t\\N\t\\N\n\
             tt0000009\t1\tnm0000100\tactor\t\\N\t[\"Walter White\"]\n\
             tt0000002\t1\tnm0000103\tself\t\\N\t[\"Self\"]\n"
        );
        let rows = HashMap::from([(1, 0), (2, 1)]);
        let mut out = Vec::new();
        assert_eq!(filter(&rows, Cursor::new(tsv), &mut out), Ok(2));
        let kept = String::from_utf8(out).unwrap();
        assert!(kept.starts_with(HEADER), "the kept rows read back like the dump");
        assert!(kept.contains("Walter White") && kept.contains("Self"));
        assert!(!kept.contains("tt0000009") && !kept.contains("director"));
        assert!(build(&rows, 2, Cursor::new(kept)).is_ok());
    }

    #[test]
    fn a_changed_header_or_an_empty_match_fails() {
        let moved = "tconst\tnconst\tordering\tcategory\tjob\tcharacters\n";
        let rows = HashMap::from([(1, 0)]);
        assert!(filter(&rows, Cursor::new(moved), &mut Vec::new())
            .unwrap_err()
            .contains("unexpected header"));
        assert!(build(&rows, 1, Cursor::new(moved)).is_err());
        let none = format!("{HEADER}\ntt0000009\t1\tnm0000100\tactor\t\\N\t[\"X\"]\n");
        assert!(filter(&rows, Cursor::new(none), &mut Vec::new()).is_err());
    }

    /// Over a real store: its `imdb` column names the rows. The route fixture holds one IMDb id, so its
    /// one title has nobody to share a character with.
    #[test]
    fn builds_against_a_real_store() {
        let dir = std::env::temp_dir().join(format!("den-atlas-characters-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");
        let (rows, row_count) = imdb_rows(&mapped.view()).unwrap();
        let tsv = dump(&[(1, 1, 100, "Walter White"), (1, 2, 101, "Jesse Pinkman")]);
        let index = build(&rows, row_count, Cursor::new(tsv)).unwrap();
        assert_eq!(index.rows(), 12);
        assert_eq!(index.links(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The serving path end to end: the gzipped dump streamed from a server in pieces, gunzipped and
    /// filtered as it arrives, the filtered rows kept, and a restart building from them without asking.
    #[tokio::test]
    async fn streams_the_dump_keeps_the_filtered_rows_and_rebuilds_from_them() {
        use std::io::Write;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("den-atlas-characters-stream-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        // The fixture's one IMDb id, among thousands the store lacks, so the body spans many reads.
        let mut roles = vec![(1, 1, 100, "Walter White")];
        roles.extend((1000..30_000).map(|t| (t, 1, t, "Somebody Else")));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(dump(&roles).as_bytes()).unwrap();
        let body = gz.finish().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&asks);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = sock.read(&mut [0u8; 4096]).await;
                let head =
                    format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                for piece in body.chunks(1024) {
                    let _ = sock.write_all(piece).await;
                    let _ = sock.flush().await;
                }
                let _ = sock.shutdown().await;
            }
        });
        let url = format!("http://{addr}/title.principals.tsv.gz");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();

        let characters = Characters::new(ds.store.clone(), &url, Some(&cache)).unwrap();
        let line = characters.refresh().await.expect("the streamed dump builds");
        assert!(line.contains("0 links over 0 of 12 store rows"), "{line}");
        let kept = std::fs::read_to_string(cache.join(CACHE_FILE)).unwrap();
        assert_eq!(kept.lines().count(), 2, "the header and the corpus's one role: {kept}");

        let restarted = Characters::new(ds.store.clone(), &url, Some(&cache)).unwrap();
        assert!(restarted.build_from_kept().await.is_some());
        assert_eq!(restarted.index().map(|index| index.rows()), Some(12));
        assert_eq!(asks.load(std::sync::atomic::Ordering::SeqCst), 1, "the restart did not download");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole corpus, for measuring: `CHARACTERS_TSV=<filtered principals> cargo test --release
    /// characters::tests::measure -- --ignored --nocapture`. Every title in the file is its own row. With
    /// `CHARACTERS_EDGES_OUT` set, writes one `tt… tt… tier weight` line per link for comparing with another
    /// build.
    #[test]
    #[ignore = "needs IMDb's principals, which are never committed"]
    fn measure() {
        let path = std::env::var("CHARACTERS_TSV").expect("CHARACTERS_TSV names a principals file");
        let text = std::fs::read(&path).unwrap();
        let mut rows: HashMap<u32, u32> = HashMap::new();
        for line in text.split(|&b| b == b'\n').skip(1) {
            let id = line.split(|&b| b == b'\t').next().and_then(|id| std::str::from_utf8(id).ok());
            if let Some(id) = id.and_then(tconst) {
                let next = rows.len() as u32;
                rows.entry(id).or_insert(next);
            }
        }
        // With `CHARACTERS_GZ` naming the whole dump, time the gunzip and filter that serving streams it
        // through, against the titles in `CHARACTERS_TSV`.
        if let Ok(gz) = std::env::var("CHARACTERS_GZ") {
            let started = Instant::now();
            let file = std::fs::File::open(gz).unwrap();
            let mut out = Vec::new();
            let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(file));
            let kept = filter(&rows, reader, &mut out).unwrap();
            println!(
                "filtered the dump in {:.1}s: {kept} lines, {} bytes",
                started.elapsed().as_secs_f64(),
                out.len()
            );
        }
        let started = Instant::now();
        let index = build(&rows, rows.len(), text.as_slice()).unwrap();
        println!(
            "built in {:.2}s: {} links over {} of {} rows, {} bytes resident",
            started.elapsed().as_secs_f64(),
            index.links(),
            index.linked(),
            index.rows(),
            index.bytes()
        );
        for (tier, n) in index.per_tier() {
            println!("  {} {n}", tier.name());
        }
        if let Ok(out) = std::env::var("CHARACTERS_EDGES_OUT") {
            let tt: HashMap<u32, u32> = rows.iter().map(|(&id, &row)| (row, id)).collect();
            let mut lines = String::new();
            for row in 0..index.rows() {
                for link in index.of(row).iter().filter(|l| l.row as usize > row) {
                    lines.push_str(&format!(
                        "tt{:07} tt{:07} {} {}\n",
                        tt[&(row as u32)],
                        tt[&link.row],
                        link.tier.name(),
                        link.weight
                    ));
                }
            }
            std::fs::write(out, lines).unwrap();
        }
    }
}
