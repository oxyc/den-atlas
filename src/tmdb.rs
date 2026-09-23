//! TMDB's vote counts, scores and credits for the corpus, fetched on the box and kept in `CACHE_DIR`
//! (oxyc/den#118). `ratings.rs` and `characters.rs` build their indexes from what this keeps.
//!
//! # The rules — read these before using anything this module holds
//!
//! TMDB's API terms (§1.C) forbid making derivatives of TMDB content, and using the API "in connection with,
//! including for training, a machine learning (ML) or artificial intelligence (AI) based Application". Atlas
//! ranks with embeddings, so the line is drawn here, at the one place TMDB data enters it:
//!
//! 1. TMDB values — `vote_average`, `vote_count`, credits' character names — are used ONLY as plain sort
//!    keys, filters, floors, merit and popularity terms, and character-link evidence, inside atlas, at
//!    runtime.
//! 2. They are NEVER fed into an embedding, into anything learned or trained, or into a prompt to a model
//!    (Jev included).
//! 3. They are NEVER written into the published dataset or into any file that leaves the box: `CACHE_DIR`
//!    only, which is the box's own disk.
//! 4. Every kept value carries the time it was fetched and is dropped `MAX_AGE` (180 days) after it: TMDB caps
//!    caching at six months.
//! 5. Wherever atlas's own pages show these numbers, they carry TMDB's attribution.
//!
//! A feature that needs TMDB data outside these rules is a question for the owner, not a refactor.
//!
//! # Where it comes from
//!
//! Everything is asked of den-edge's TMDB proxy (`TMDB_PROXY`, e.g. `http://den-edge:8080/tmdb`): one key,
//! one on-disk answer cache and one daily ceiling for every Den service, so atlas holds no key of its own.
//! Atlas paces itself (`PACE`, well inside the proxy's per-address allowance) and keeps its own daily ceiling
//! (`TMDB_DAILY_MAX`), because the proxy's is shared with every guest browsing the web app.
//!
//! - **Vote counts** come in bulk from `/discover/{movie,tv}`, 20 titles a page, sliced by release or first
//!   air date so no slice passes discover's 500-page cap, at the corpus's own admission floor
//!   (`SWEEP_FLOOR`). A sweep is ~4,800 questions for ~47,550 of the corpus's titles, and runs every
//!   `SWEEP_EVERY` as fast as the daily ceiling allows. A title no sweep returned — its count fell under the
//!   floor, or it has no date — is asked for on its own once its count is `FILL_AFTER` old.
//! - **Credits** are `/movie/{id}/credits` and `/tv/{id}/aggregate_credits`: a share of the corpus every day,
//!   oldest first, so every list is asked again within `CREDITS_ROLL_DAYS`, and at once for any title the
//!   changes feed (`/{movie,tv}/changes`) says was edited. The changes feed says nothing about votes — TMDB does
//!   not record a vote as a change — which is why votes are swept instead.
//! - **A first boot** reads the seed den-dataset's own detail cache gave (`scripts/tmdb-seed.py`), copied into
//!   `CACHE_DIR` by hand; each entry keeps the time that body was fetched, so the six months are counted from
//!   TMDB's answer, not from the copy.

use crate::characters::{self, Characters};
use crate::ratings::{self, Key, Ratings};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAY: u64 = 86_400;
/// TMDB's six months: nothing kept is used or kept past this after it was fetched.
pub const MAX_AGE: u64 = 180 * DAY;
const VOTES_FILE: &str = "tmdb-votes.tsv";
const CREDITS_FILE: &str = "tmdb-credits.tsv";
const STATE_FILE: &str = "tmdb-sweep.tsv";
/// Each file's first line, CHECKED: a file in another layout is ignored whole rather than read into the
/// wrong fields.
const VOTES_HEADER: &str = "# den-atlas tmdb-votes v1\tmedia\tid\tvote_average\tvote_count\tfetched";
const CREDITS_HEADER: &str = "# den-atlas tmdb-credits v1\tmedia\tid\tfetched\torder\tperson\tcharacter";
const STATE_HEADER: &str = "# den-atlas tmdb-sweep v1";

/// The corpus's own admission floor (den-dataset `pipeline/floors.py`, the regional tier's 15 TMDB votes). A
/// sweep at 15 is predicted, from den-dataset's cached details, to return 47,546 of the 47,618 titles of
/// dataset 5b1c3213b6a1; a floor of 10 costs 1,500 more questions for the same titles.
const SWEEP_FLOOR: u32 = 15;
const SWEEP_EVERY: u64 = 30 * DAY;
/// A vote count this old was missed by every sweep since, so it is asked for by title.
const FILL_AFTER: u64 = 45 * DAY;
/// Every title's credits are asked again within this many days — a share of the corpus a day, oldest first,
/// so the corpus never comes due at once.
const CREDITS_ROLL_DAYS: u64 = 150;
/// Discover serves 500 pages of a query and no more.
const PAGE_CAP: u64 = 500;
/// Between two questions: 30 a minute, a quarter of what den-edge allows one address.
const PACE: Duration = Duration::from_secs(2);
/// Questions a UTC day, when `TMDB_DAILY_MAX` does not say. A sweep plus the day's credits fit in a week of
/// these, and they leave most of den-edge's shared ceiling to the web app's guests.
pub const DEFAULT_DAILY_MAX: u32 = 1500;
/// How often the refresh runs. A restart within this does not run it again.
const TICK_EVERY: u64 = DAY;

/// One title's vote count and score, and when TMDB said so.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Votes {
    pub average: f32,
    pub count: u32,
    pub fetched: u64,
}

/// One credited, named role. `order` is TMDB's billing position, from 0.
#[derive(Clone, Debug, PartialEq)]
pub struct Role {
    pub order: u32,
    pub person: u32,
    pub character: Box<str>,
}

/// One title's credited roles — only the first `characters::BILLED`, the only ones read — and when TMDB gave
/// them. An empty list is a title TMDB credits no named role for, kept so it is not asked again tomorrow.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Credits {
    pub fetched: u64,
    pub roles: Vec<Role>,
}

#[derive(Default)]
struct Kept {
    votes: HashMap<Key, Votes>,
    credits: HashMap<Key, Credits>,
}

impl Kept {
    /// Drop everything past `MAX_AGE`; how many were dropped.
    fn expire(&mut self, now: u64) -> usize {
        let before = self.votes.len() + self.credits.len();
        self.votes.retain(|_, v| fresh(v.fetched, now));
        self.credits.retain(|_, c| fresh(c.fetched, now));
        before - self.votes.len() - self.credits.len()
    }
}

/// Whether something fetched at `fetched` may still be used at `now`. `now == 0` reads everything, for tools.
fn fresh(fetched: u64, now: u64) -> bool {
    now == 0 || fetched + MAX_AGE > now
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn media_name(media: u8) -> &'static str {
    if media == 1 {
        "tv"
    } else {
        "movie"
    }
}

fn media_of(name: &str) -> Option<u8> {
    match name {
        "movie" => Some(0),
        "tv" => Some(1),
        _ => None,
    }
}

/// A kept file's lines after its header, or none when it is absent or in another layout.
fn lines_of(path: &Path, header: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut lines = text.lines();
    if lines.next() != Some(header) {
        eprintln!("tmdb: {} is not in the layout this build reads ({header:?}); ignoring it", path.display());
        return Vec::new();
    }
    lines.map(str::to_owned).collect()
}

/// The kept vote counts, without any past `MAX_AGE` at `now`.
pub fn read_votes(path: &Path, now: u64) -> HashMap<Key, Votes> {
    let mut votes = HashMap::new();
    for line in lines_of(path, VOTES_HEADER) {
        let f: Vec<&str> = line.split('\t').collect();
        let [media, id, average, count, fetched] = f[..] else { continue };
        let (Some(media), Ok(id), Ok(average), Ok(count), Ok(fetched)) =
            (media_of(media), id.parse(), average.parse(), count.parse(), fetched.parse())
        else {
            continue;
        };
        if fresh(fetched, now) {
            votes.insert((media, id), Votes { average, count, fetched });
        }
    }
    votes
}

/// The kept credits, without any past `MAX_AGE` at `now`.
pub fn read_credits(path: &Path, now: u64) -> HashMap<Key, Credits> {
    let mut credits: HashMap<Key, Credits> = HashMap::new();
    for line in lines_of(path, CREDITS_HEADER) {
        let f: Vec<&str> = line.splitn(6, '\t').collect();
        let [media, id, fetched, order, person, character] = f[..] else { continue };
        let (Some(media), Ok(id), Ok(fetched)) = (media_of(media), id.parse(), fetched.parse()) else {
            continue;
        };
        if !fresh(fetched, now) {
            continue;
        }
        let title = credits.entry((media, id)).or_insert(Credits { fetched, roles: Vec::new() });
        // A title with no named role is one line with the role's fields empty.
        if let (Ok(order), Ok(person)) = (order.parse(), person.parse()) {
            title.roles.push(Role { order, person, character: character.into() });
        }
    }
    credits
}

/// Written beside and renamed over, so a crash mid-write never leaves half a file that reads as whole.
fn write_file(path: &Path, header: &str, body: impl FnOnce(&mut Vec<u8>)) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(1 << 20);
    out.extend_from_slice(header.as_bytes());
    out.push(b'\n');
    body(&mut out);
    let partial = path.with_extension("tsv.partial");
    std::fs::write(&partial, &out)?;
    std::fs::rename(&partial, path)
}

fn write_votes(path: &Path, votes: &HashMap<Key, Votes>) -> std::io::Result<()> {
    let mut keys: Vec<&Key> = votes.keys().collect();
    keys.sort_unstable();
    write_file(path, VOTES_HEADER, |out| {
        for key in keys {
            let v = votes[key];
            let _ =
                writeln!(out, "{}\t{}\t{}\t{}\t{}", media_name(key.0), key.1, v.average, v.count, v.fetched);
        }
    })
}

fn write_credits(path: &Path, credits: &HashMap<Key, Credits>) -> std::io::Result<()> {
    let mut keys: Vec<&Key> = credits.keys().collect();
    keys.sort_unstable();
    write_file(path, CREDITS_HEADER, |out| {
        for key in keys {
            let c = &credits[key];
            let (media, id) = (media_name(key.0), key.1);
            if c.roles.is_empty() {
                let _ = writeln!(out, "{media}\t{id}\t{}\t\t\t", c.fetched);
            }
            for r in &c.roles {
                let character = r.character.replace(['\t', '\n', '\r'], " ");
                let _ = writeln!(out, "{media}\t{id}\t{}\t{}\t{}\t{character}", c.fetched, r.order, r.person);
            }
        }
    })
}

/// A date range of one media type's discover sweep, and the next page to ask of it. Dates are days since
/// 1970-01-01.
#[derive(Clone, Debug, PartialEq)]
struct Slice {
    media: u8,
    from: i64,
    to: i64,
    page: u64,
}

/// What the refresh must remember across restarts: the day's spending, when it last ran, and a sweep in
/// progress.
#[derive(Clone, Debug, Default, PartialEq)]
struct State {
    day: u64,
    spent: u32,
    last_tick: u64,
    sweep_started: u64,
    slices: Vec<Slice>,
}

impl State {
    fn read(path: &Path) -> State {
        let mut state = State::default();
        for line in lines_of(path, STATE_HEADER) {
            let f: Vec<&str> = line.split('\t').collect();
            let number = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok());
            match (f.first().copied(), number(1)) {
                (Some("day"), Some(v)) => state.day = v as u64,
                (Some("spent"), Some(v)) => state.spent = v as u32,
                (Some("last_tick"), Some(v)) => state.last_tick = v as u64,
                (Some("sweep_started"), Some(v)) => state.sweep_started = v as u64,
                (Some("slice"), _) => {
                    let media = f.get(1).and_then(|m| media_of(m));
                    if let (Some(media), Some(from), Some(to), Some(page)) =
                        (media, number(2), number(3), number(4))
                    {
                        state.slices.push(Slice { media, from, to, page: page as u64 });
                    }
                }
                _ => {}
            }
        }
        state
    }

    fn write(&self, path: &Path) -> std::io::Result<()> {
        write_file(path, STATE_HEADER, |out| {
            let _ = writeln!(out, "day\t{}\nspent\t{}", self.day, self.spent);
            let _ = writeln!(out, "last_tick\t{}\nsweep_started\t{}", self.last_tick, self.sweep_started);
            for s in &self.slices {
                let _ = writeln!(out, "slice\t{}\t{}\t{}\t{}", media_name(s.media), s.from, s.to, s.page);
            }
        })
    }
}

/// Days since 1970-01-01 for a civil date, and back (Howard Hinnant's algorithms).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A new sweep: every date from the first films to two years ahead, per media type; `split` halves a range
/// that discover would cap.
fn new_sweep(now: u64) -> Vec<Slice> {
    let (from, to) = (days_from_civil(1870, 1, 1), (now / DAY) as i64 + 2 * 365);
    vec![Slice { media: 0, from, to, page: 1 }, Slice { media: 1, from, to, page: 1 }]
}

fn discover_path(s: &Slice) -> String {
    let (kind, field) =
        if s.media == 1 { ("tv", "first_air_date") } else { ("movie", "primary_release_date") };
    format!(
        "/discover/{kind}?include_adult=false&page={}&sort_by={field}.asc&vote_count.gte={SWEEP_FLOOR}\
         &{field}.gte={}&{field}.lte={}",
        s.page,
        civil(s.from),
        civil(s.to)
    )
}

/// What one question came back as.
enum Asked {
    /// TMDB's answer, and when TMDB gave it (the proxy's `Last-Modified`: it may have kept the answer a while).
    Answer(serde_json::Value, u64),
    /// TMDB has no such thing.
    Gone,
    /// Stop asking today: out of budget, rate-limited, or the proxy is failing. `Budget::stopped` says why.
    Stop,
}

struct Proxy {
    client: reqwest::Client,
    /// Up to and excluding `/3`.
    base: String,
    daily_max: u32,
    /// `PACE`; a test asks without it.
    pace: Duration,
}

pub struct Tmdb {
    /// `CACHE_DIR`; `None` keeps everything in memory, so a restart starts over.
    dir: Option<PathBuf>,
    /// The store the numbers are joined onto, by path: opened per build, never held.
    store: PathBuf,
    kept: Mutex<Kept>,
    state: Mutex<State>,
    proxy: Option<Proxy>,
    ratings: Arc<Ratings>,
    characters: Arc<Characters>,
}

impl Tmdb {
    /// `proxy` is den-edge's `/tmdb` base; `None` serves what is kept and asks for nothing.
    pub fn new(
        store: PathBuf,
        dir: Option<PathBuf>,
        proxy: Option<String>,
        daily_max: u32,
    ) -> Result<Self, reqwest::Error> {
        let proxy = match proxy {
            Some(base) => Some(Proxy {
                client: reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?,
                base: base.trim_end_matches('/').to_owned(),
                daily_max,
                pace: PACE,
            }),
            None => None,
        };
        Ok(Tmdb {
            dir,
            store,
            kept: Mutex::new(Kept::default()),
            state: Mutex::new(State::default()),
            proxy,
            ratings: Arc::new(Ratings::default()),
            characters: Arc::new(Characters::default()),
        })
    }

    pub fn ratings(&self) -> Arc<Ratings> {
        Arc::clone(&self.ratings)
    }

    pub fn characters(&self) -> Arc<Characters> {
        Arc::clone(&self.characters)
    }

    pub fn asks(&self) -> bool {
        self.proxy.is_some()
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(name))
    }

    /// Read what is kept, drop what is past `MAX_AGE`, and build both indexes: the boot step, a local read
    /// only. A line for the log.
    pub async fn load(&self) -> String {
        let now = now_secs();
        let (votes, credits, state) =
            match (self.file(VOTES_FILE), self.file(CREDITS_FILE), self.file(STATE_FILE)) {
                (Some(v), Some(c), Some(s)) => tokio::task::spawn_blocking(move || {
                    (read_votes(&v, now), read_credits(&c, now), State::read(&s))
                })
                .await
                .unwrap_or_default(),
                _ => Default::default(),
            };
        let oldest = votes.values().map(|v| v.fetched).min();
        {
            let mut kept = crate::util::lock(&self.kept);
            kept.votes = votes;
            kept.credits = credits;
        }
        *crate::util::lock(&self.state) = state;
        let built = self.rebuild().await;
        let age = oldest
            .map_or_else(|| "none".to_owned(), |t| format!("oldest {} days", now.saturating_sub(t) / DAY));
        format!(
            "tmdb: loaded from {} ({age}); {built}",
            self.dir.as_ref().map_or("memory".into(), |d| d.display().to_string())
        )
    }

    /// Both indexes from what is kept now. A line for the log.
    async fn rebuild(&self) -> String {
        let (votes, credits) = {
            let kept = crate::util::lock(&self.kept);
            let votes: HashMap<Key, (f32, u32)> =
                kept.votes.iter().map(|(&k, v)| (k, (v.average, v.count))).collect();
            (votes, kept.credits.clone())
        };
        let store = self.store.clone();
        let built = tokio::task::spawn_blocking(move || {
            let mapped = crate::store::MappedStore::open(&store)?;
            let view = mapped.view();
            let ratings = ratings::build(&view, &votes);
            let characters = characters::build(&view, &credits);
            Ok::<_, String>((ratings, characters, credits.len()))
        })
        .await
        .map_err(|e| format!("tmdb build task: {e}"))
        .and_then(|built| built);
        match built {
            Ok((rated, linked, credited)) => {
                let votes = match rated {
                    Ok(index) => {
                        let line =
                            format!("vote counts for {} of {} store rows", index.matched(), index.rows());
                        self.ratings.set(Some(index));
                        line
                    }
                    Err(e) => {
                        self.ratings.set(None);
                        format!("no vote counts ({e})")
                    }
                };
                let characters = match linked {
                    Ok(index) => {
                        let line = characters::describe(&index);
                        self.characters.set(index);
                        line
                    }
                    Err(e) => format!("no character links ({e})"),
                };
                format!("{votes}; credits for {credited} titles, {characters}")
            }
            Err(e) => format!("indexes not rebuilt ({e}); the previous ones keep serving"),
        }
    }

    /// Once a day: drop what expired, ask what is due, keep it, rebuild. Never returns.
    pub async fn refresh_forever(self: Arc<Self>) {
        // Give the boot's own work a head start.
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            let now = now_secs();
            let last = crate::util::lock(&self.state).last_tick;
            if self.proxy.is_some() && now >= last + TICK_EVERY {
                let line = self.tick(now).await;
                eprintln!("{line}");
            }
            let next = crate::util::lock(&self.state).last_tick + TICK_EVERY;
            tokio::time::sleep(Duration::from_secs(next.saturating_sub(now_secs()).max(3600))).await;
        }
    }

    /// One refresh. A line for the log.
    async fn tick(&self, now: u64) -> String {
        let Some(proxy) = self.proxy.as_ref() else { return "tmdb: no proxy, nothing asked".to_owned() };
        let store = self.store.clone();
        let corpus = tokio::task::spawn_blocking(move || corpus_keys(&store)).await;
        let corpus = match corpus {
            Ok(Ok(corpus)) => corpus,
            Ok(Err(e)) => return format!("tmdb: refresh skipped, the store did not open ({e})"),
            Err(e) => return format!("tmdb: refresh skipped ({e})"),
        };
        let (mut budget, previous) = {
            let mut state = crate::util::lock(&self.state);
            if state.day != now / DAY {
                state.day = now / DAY;
                state.spent = 0;
            }
            let previous = std::mem::replace(&mut state.last_tick, now);
            (Budget { left: proxy.daily_max.saturating_sub(state.spent), asked: 0, stopped: None }, previous)
        };
        let expired = crate::util::lock(&self.kept).expire(now);

        let (changed, changes_seen) = self.changes(proxy, &corpus, (previous, now), &mut budget).await;
        let credited_changed = self.fetch_credits(proxy, &changed, &mut budget).await;
        let (pages, sweep) = self.sweep(proxy, &corpus, now, &mut budget).await;
        let filled = self.fill(proxy, &corpus, now, &mut budget).await;
        let (missing, rolling) = {
            let kept = crate::util::lock(&self.kept);
            let mut missing: Vec<Key> =
                corpus.iter().filter(|k| !kept.credits.contains_key(k)).copied().collect();
            missing.sort_unstable();
            let mut oldest: Vec<(u64, Key)> = kept
                .credits
                .iter()
                .filter(|(k, _)| corpus.contains(k))
                .map(|(&k, c)| (c.fetched, k))
                .collect();
            oldest.sort_unstable();
            let share = (corpus.len() as u64).div_ceil(CREDITS_ROLL_DAYS) as usize;
            (missing, oldest.into_iter().take(share).map(|(_, k)| k).collect::<Vec<_>>())
        };
        let credited_missing = self.fetch_credits(proxy, &missing, &mut budget).await;
        let credited_rolling = self.fetch_credits(proxy, &rolling, &mut budget).await;

        {
            let mut state = crate::util::lock(&self.state);
            state.spent += budget.asked;
        }
        let kept = self.persist().await;
        let built = self.rebuild().await;
        format!(
            "tmdb: refreshed — {} questions ({} left today){}; changes: {changes_seen} titles, {} in the corpus, \
             {credited_changed} credits asked again; sweep: {pages} pages, {sweep}; {filled} counts asked by \
             title; credits: {credited_missing} missing, {credited_rolling} rolled; {expired} entries past six \
             months dropped; {kept}; {built}",
            budget.asked,
            budget.left,
            budget.stopped.as_deref().map(|why| format!(", stopped early: {why}")).unwrap_or_default(),
            changed.len(),
        )
    }

    /// Write what is kept to `CACHE_DIR`. A phrase for the log.
    async fn persist(&self) -> String {
        let (Some(v), Some(c), Some(s)) =
            (self.file(VOTES_FILE), self.file(CREDITS_FILE), self.file(STATE_FILE))
        else {
            return "kept in memory only".to_owned();
        };
        let (votes, credits, state) = {
            let kept = crate::util::lock(&self.kept);
            (kept.votes.clone(), kept.credits.clone(), crate::util::lock(&self.state).clone())
        };
        let written = tokio::task::spawn_blocking(move || {
            write_votes(&v, &votes)?;
            write_credits(&c, &credits)?;
            state.write(&s)
        })
        .await;
        match written {
            Ok(Ok(())) => "kept in CACHE_DIR".to_owned(),
            Ok(Err(e)) => format!("NOT kept ({e}); a restart asks again"),
            Err(e) => format!("NOT kept ({e}); a restart asks again"),
        }
    }

    /// One question, paced and counted against the day's budget.
    async fn ask(&self, proxy: &Proxy, path: &str, budget: &mut Budget) -> Asked {
        if budget.stopped.is_some() {
            return Asked::Stop;
        }
        let url = format!("{}/3{path}", proxy.base);
        let mut stale_asks = 0;
        let mut failures = 0;
        loop {
            if budget.left == 0 {
                return budget.stop("the day's TMDB_DAILY_MAX is spent");
            }
            tokio::time::sleep(proxy.pace).await;
            budget.left -= 1;
            budget.asked += 1;
            let resp = match proxy.client.get(&url).header("accept", "application/json").send().await {
                Ok(resp) => resp,
                Err(e) => {
                    failures += 1;
                    if failures < 2 {
                        continue;
                    }
                    return budget.stop(&format!("the proxy is unreachable ({e})"));
                }
            };
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Asked::Gone;
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
            {
                return budget.stop(&format!("the proxy answered {status}"));
            }
            if !status.is_success() {
                failures += 1;
                if failures < 2 {
                    continue;
                }
                return budget.stop(&format!("the proxy answered {status}"));
            }
            // A list the proxy kept past its freshness is served at once and asked again behind it; the
            // second question a moment later gets what TMDB says now.
            let stale = resp.headers().get("x-den-tmdb").is_some_and(|v| v == "stale");
            if stale && stale_asks < 2 {
                stale_asks += 1;
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let fetched = resp
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| httpdate::parse_http_date(v).ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or_else(now_secs, |d| d.as_secs());
            return match resp.json::<serde_json::Value>().await {
                Ok(body) => Asked::Answer(body, fetched),
                Err(e) => budget.stop(&format!("an answer did not read ({e})")),
            };
        }
    }

    /// Corpus titles TMDB's changes feed names since the last refresh (at most its 14 days), and how many
    /// titles the feed named in all.
    async fn changes(
        &self,
        proxy: &Proxy,
        corpus: &HashSet<Key>,
        (previous, now): (u64, u64),
        budget: &mut Budget,
    ) -> (Vec<Key>, usize) {
        let today = (now / DAY) as i64;
        // From the day before the last refresh, so a refresh that ran late misses nothing; the feed answers 14
        // days at most, and a first refresh has no credits older than the seed's to correct.
        let since = if previous == 0 { today - 1 } else { ((previous / DAY) as i64 - 1).max(today - 14) };
        let mut changed = Vec::new();
        let mut seen = 0;
        for media in [0u8, 1] {
            let mut page = 1u64;
            loop {
                let path = format!(
                    "/{}/changes?start_date={}&end_date={}&page={page}",
                    media_name(media),
                    civil(since),
                    civil(today)
                );
                let Asked::Answer(body, _) = self.ask(proxy, &path, budget).await else { break };
                for id in body["results"].as_array().into_iter().flatten().filter_map(|r| r["id"].as_u64()) {
                    seen += 1;
                    let key = (media, id as u32);
                    if corpus.contains(&key) {
                        changed.push(key);
                    }
                }
                if page >= body["total_pages"].as_u64().unwrap_or(1) {
                    break;
                }
                page += 1;
            }
        }
        changed.sort_unstable();
        changed.dedup();
        (changed, seen)
    }

    /// Ask for each title's credits; how many answered.
    async fn fetch_credits(&self, proxy: &Proxy, keys: &[Key], budget: &mut Budget) -> usize {
        let mut answered = 0;
        for &(media, id) in keys {
            let path = if media == 1 {
                format!("/tv/{id}/aggregate_credits")
            } else {
                format!("/movie/{id}/credits")
            };
            match self.ask(proxy, &path, budget).await {
                Asked::Answer(body, fetched) => {
                    let roles = roles_of(&body, media == 1);
                    crate::util::lock(&self.kept).credits.insert((media, id), Credits { fetched, roles });
                    answered += 1;
                }
                Asked::Gone => {
                    crate::util::lock(&self.kept).credits.remove(&(media, id));
                }
                Asked::Stop => break,
            }
        }
        answered
    }

    /// Carry the sweep on as far as the budget allows, starting a new one when the last is `SWEEP_EVERY` old.
    /// The pages asked, and where the sweep stands.
    async fn sweep(
        &self,
        proxy: &Proxy,
        corpus: &HashSet<Key>,
        now: u64,
        budget: &mut Budget,
    ) -> (usize, String) {
        {
            let mut state = crate::util::lock(&self.state);
            if state.slices.is_empty() {
                if state.sweep_started != 0 && now < state.sweep_started + SWEEP_EVERY {
                    let next = (state.sweep_started + SWEEP_EVERY - now) / DAY;
                    return (0, format!("idle, the next in {next} days"));
                }
                state.sweep_started = now;
                state.slices = new_sweep(now);
            }
        }
        let mut pages = 0;
        loop {
            let Some(slice) = crate::util::lock(&self.state).slices.first().cloned() else {
                return (pages, "complete".to_owned());
            };
            let body = match self.ask(proxy, &discover_path(&slice), budget).await {
                Asked::Answer(body, fetched) => Some((body, fetched)),
                Asked::Gone => None,
                Asked::Stop => break,
            };
            pages += 1;
            let mut state = crate::util::lock(&self.state);
            let Some((body, fetched)) = body else {
                state.slices.remove(0);
                continue;
            };
            let total = body["total_pages"].as_u64().unwrap_or(0);
            // Discover would stop at page 500: halve the range and ask each half from its first page.
            if slice.page == 1 && total > PAGE_CAP && slice.from < slice.to {
                let mid = slice.from + (slice.to - slice.from) / 2;
                state.slices[0] = Slice { to: mid, ..slice.clone() };
                state.slices.insert(1, Slice { from: mid + 1, ..slice.clone() });
            } else if slice.page >= total.min(PAGE_CAP) {
                state.slices.remove(0);
            } else {
                state.slices[0].page += 1;
            }
            drop(state);
            let mut kept = crate::util::lock(&self.kept);
            for (key, votes) in votes_of(&body, slice.media, fetched) {
                if corpus.contains(&key) {
                    kept.votes.insert(key, votes);
                }
            }
        }
        let left = crate::util::lock(&self.state).slices.len();
        (pages, format!("{left} slices left"))
    }

    /// Ask by title for the counts no sweep has brought in `FILL_AFTER` — only while no sweep is under way,
    /// since a sweep brings nearly all of them. How many answered.
    async fn fill(&self, proxy: &Proxy, corpus: &HashSet<Key>, now: u64, budget: &mut Budget) -> usize {
        if !crate::util::lock(&self.state).slices.is_empty() {
            return 0;
        }
        let mut due: Vec<(u64, Key)> = {
            let kept = crate::util::lock(&self.kept);
            corpus
                .iter()
                .filter_map(|k| match kept.votes.get(k) {
                    None => Some((0, *k)),
                    Some(v) if v.fetched + FILL_AFTER <= now => Some((v.fetched, *k)),
                    Some(_) => None,
                })
                .collect()
        };
        due.sort_unstable();
        let mut answered = 0;
        for (_, (media, id)) in due {
            match self.ask(proxy, &format!("/{}/{id}", media_name(media)), budget).await {
                Asked::Answer(body, fetched) => {
                    let mut kept = crate::util::lock(&self.kept);
                    match vote_fields(&body) {
                        Some((average, count)) => {
                            kept.votes.insert((media, id), Votes { average, count, fetched });
                        }
                        None => {
                            kept.votes.remove(&(media, id));
                        }
                    }
                    answered += 1;
                }
                Asked::Gone => {
                    crate::util::lock(&self.kept).votes.remove(&(media, id));
                }
                Asked::Stop => break,
            }
        }
        answered
    }
}

/// The day's allowance as a refresh spends it.
struct Budget {
    left: u32,
    asked: u32,
    stopped: Option<String>,
}

impl Budget {
    fn stop(&mut self, why: &str) -> Asked {
        self.stopped.get_or_insert_with(|| why.to_owned());
        Asked::Stop
    }
}

/// The store's titles, as the keys TMDB's answers are matched against.
fn corpus_keys(store: &Path) -> Result<HashSet<Key>, String> {
    let mapped = crate::store::MappedStore::open(store)?;
    let view = mapped.view();
    let keys = view.per_row::<u64>("keys").map_err(|e| e.to_string())?;
    Ok(keys.iter().map(|&packed| (u8::from(packed >> 32 == 1), packed as u32)).collect())
}

/// A vote count and score from a title's record or a discover row; `None` for a title nobody has voted on.
fn vote_fields(row: &serde_json::Value) -> Option<(f32, u32)> {
    let count = u32::try_from(row["vote_count"].as_u64()?).ok()?;
    let average = row["vote_average"].as_f64()? as f32;
    (count > 0).then_some((average, count))
}

/// A discover page's titles with a count.
fn votes_of(body: &serde_json::Value, media: u8, fetched: u64) -> Vec<(Key, Votes)> {
    body["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = u32::try_from(row["id"].as_u64()?).ok()?;
            let (average, count) = vote_fields(row)?;
            Some(((media, id), Votes { average, count, fetched }))
        })
        .collect()
}

/// The named roles of a credits answer, the first `characters::BILLED` only. A series' aggregate credits give
/// each person a list of roles; each is kept, under that person's one billing position.
fn roles_of(body: &serde_json::Value, aggregate: bool) -> Vec<Role> {
    let mut roles = Vec::new();
    for member in body["cast"].as_array().into_iter().flatten() {
        let (Some(order), Some(person)) = (member["order"].as_u64(), member["id"].as_u64()) else { continue };
        let (Ok(order), Ok(person)) = (u32::try_from(order), u32::try_from(person)) else { continue };
        if order >= characters::BILLED {
            continue;
        }
        let named: Vec<&str> = if aggregate {
            member["roles"].as_array().into_iter().flatten().filter_map(|r| r["character"].as_str()).collect()
        } else {
            member["character"].as_str().into_iter().collect()
        };
        for character in named.into_iter().map(str::trim).filter(|c| !c.is_empty()) {
            roles.push(Role { order, person, character: character.into() });
        }
    }
    roles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("den-atlas-tmdb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(civil(0), "1970-01-01");
        assert_eq!(civil(days_from_civil(1870, 1, 1)), "1870-01-01");
        assert_eq!(civil(days_from_civil(2026, 9, 22)), "2026-09-22");
        assert_eq!(civil(days_from_civil(2024, 2, 29)), "2024-02-29");
    }

    /// What was kept reads back — and nothing past TMDB's six months does, counted from when TMDB answered.
    #[test]
    fn kept_files_round_trip_and_forget_what_is_six_months_old() {
        let dir = temp("files");
        let now = 400 * DAY;
        let votes = HashMap::from([
            ((0, 1), Votes { average: 8.4, count: 9000, fetched: now - DAY }),
            ((1, 2), Votes { average: 7.0, count: 20, fetched: now - MAX_AGE }),
        ]);
        write_votes(&dir.join(VOTES_FILE), &votes).unwrap();
        let read = read_votes(&dir.join(VOTES_FILE), now);
        assert_eq!(read.len(), 1, "the one fetched 180 days ago is gone");
        assert_eq!(read[&(0, 1)], votes[&(0, 1)]);
        assert_eq!(read_votes(&dir.join(VOTES_FILE), 0).len(), 2, "a tool may read everything");

        let credits = HashMap::from([
            (
                (0, 1),
                Credits {
                    fetched: now - DAY,
                    roles: vec![
                        Role { order: 0, person: 100, character: "Walter\tWhite".into() },
                        Role { order: 1, person: 101, character: "Jesse Pinkman".into() },
                    ],
                },
            ),
            ((1, 2), Credits { fetched: now - DAY, roles: Vec::new() }),
        ]);
        write_credits(&dir.join(CREDITS_FILE), &credits).unwrap();
        let read = read_credits(&dir.join(CREDITS_FILE), now);
        assert_eq!(read[&(0, 1)].roles[0].character.as_ref(), "Walter White", "a tab never splits a line");
        assert_eq!(read[&(0, 1)].roles.len(), 2);
        assert_eq!(read[&(1, 2)], credits[&(1, 2)], "a title with no named role is remembered as one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file in another layout is ignored whole, never read into the wrong fields.
    #[test]
    fn a_file_in_another_layout_is_ignored() {
        let dir = temp("layout");
        std::fs::write(dir.join(VOTES_FILE), "movie\t1\t8.4\t9000\t100\n").unwrap();
        assert!(read_votes(&dir.join(VOTES_FILE), 0).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sweep_state_round_trips() {
        let dir = temp("state");
        let state = State {
            day: 20_000,
            spent: 12,
            last_tick: 1_700_000_000,
            sweep_started: 1_699_000_000,
            slices: vec![Slice { media: 1, from: -36_524, to: 20_700, page: 7 }],
        };
        state.write(&dir.join(STATE_FILE)).unwrap();
        assert_eq!(State::read(&dir.join(STATE_FILE)), state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_discover_page_is_asked_by_date_at_the_corpus_floor() {
        let slice = Slice {
            media: 1,
            from: days_from_civil(2010, 11, 17),
            to: days_from_civil(2020, 12, 8),
            page: 3,
        };
        assert_eq!(
            discover_path(&slice),
            "/discover/tv?include_adult=false&page=3&sort_by=first_air_date.asc&vote_count.gte=15\
             &first_air_date.gte=2010-11-17&first_air_date.lte=2020-12-08"
        );
        let body = serde_json::json!({"results": [
            {"id": 1399, "vote_average": 8.5, "vote_count": 25000},
            {"id": 7, "vote_average": 0.0, "vote_count": 0}
        ]});
        assert_eq!(
            votes_of(&body, 1, 5),
            vec![((1, 1399), Votes { average: 8.5, count: 25000, fetched: 5 })]
        );
    }

    /// A film's cast names one character each; a series' aggregate credits a list per person. Past `BILLED`
    /// nothing is kept.
    #[test]
    fn credits_answers_become_roles() {
        let film = serde_json::json!({"cast": [
            {"id": 100, "order": 0, "character": "Bruce Wayne / Batman"},
            {"id": 101, "order": 1, "character": ""},
            {"id": 102, "order": characters::BILLED, "character": "Extra"}
        ]});
        assert_eq!(
            roles_of(&film, false),
            vec![Role { order: 0, person: 100, character: "Bruce Wayne / Batman".into() }]
        );
        let series = serde_json::json!({"cast": [
            {"id": 200, "order": 0, "roles": [{"character": "Walter White"}, {"character": "Heisenberg"}]}
        ]});
        let roles = roles_of(&series, true);
        assert_eq!(roles.len(), 2);
        assert!(roles.iter().all(|r| r.order == 0 && r.person == 200));
    }

    /// A stand-in for den-edge's proxy. `/discover/movie` claims 600 pages for a range longer than 40,000
    /// days, so a sweep's first slice must be halved; every other answer is one page. Counts each question.
    async fn fake_proxy() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let path = req.uri().path().trim_start_matches("/tmdb/3").to_owned();
                let query = req.uri().query().unwrap_or("").to_owned();
                let date = |name: &str| {
                    let value = query.split('&').find_map(|kv| kv.strip_prefix(&format!("{name}=")))?;
                    let (y, rest) = value.split_once('-')?;
                    let (m, d) = rest.split_once('-')?;
                    Some(days_from_civil(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?))
                };
                let body = match path.as_str() {
                    "/movie/changes" => serde_json::json!({"results": [{"id": 2}], "total_pages": 1}),
                    "/tv/changes" => serde_json::json!({"results": [], "total_pages": 1}),
                    "/discover/movie" => {
                        let span = date("primary_release_date.lte").zip(date("primary_release_date.gte"));
                        let pages = if span.is_some_and(|(to, from)| to - from > 40_000) { 600 } else { 1 };
                        serde_json::json!({"total_pages": pages, "results": [
                            {"id": 1, "vote_average": 8.0, "vote_count": 100},
                            {"id": 424_242, "vote_average": 7.0, "vote_count": 50}
                        ]})
                    }
                    "/discover/tv" => serde_json::json!({"total_pages": 1, "results": [
                        {"id": 4, "vote_average": 7.5, "vote_count": 30}
                    ]}),
                    p if p.ends_with("/credits") => serde_json::json!({"cast": [
                        {"id": 100, "order": 0, "character": "Walter White"}
                    ]}),
                    p if p.ends_with("/aggregate_credits") => serde_json::json!({"cast": [
                        {"id": 100, "order": 0, "roles": [{"character": "Walter White"}]}
                    ]}),
                    _ => serde_json::json!({"vote_average": 6.0, "vote_count": 20}),
                };
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .header("last-modified", "Tue, 22 Sep 2026 10:00:00 GMT")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}/tmdb"), asked)
    }

    /// One refresh end to end: the changes feed, a sweep that halves the slice discover would cap, credits
    /// for every corpus title, what is kept written to `CACHE_DIR`, both indexes rebuilt — and a title the
    /// store does not hold never kept.
    #[tokio::test]
    async fn a_refresh_sweeps_asks_credits_keeps_and_rebuilds() {
        let dir = temp("tick");
        let ds = crate::queries::write_fixture(&dir.join("ds"));
        let corpus = corpus_keys(&ds.store).unwrap();
        let (base, asked) = fake_proxy().await;
        let mut tmdb = Tmdb::new(ds.store.clone(), Some(dir.clone()), Some(base), 1000).unwrap();
        tmdb.proxy.as_mut().unwrap().pace = Duration::ZERO;
        let now = days_from_civil(2026, 9, 22) as u64 * DAY;

        let line = tmdb.tick(now).await;
        assert!(line.contains("changes: 1 titles, 1 in the corpus"), "{line}");
        assert!(line.contains("complete"), "the sweep finished: {line}");
        let fetched = days_from_civil(2026, 9, 22) as u64 * DAY + 10 * 3600;
        {
            let kept = crate::util::lock(&tmdb.kept);
            assert_eq!(kept.votes[&(0, 1)], Votes { average: 8.0, count: 100, fetched }, "Last-Modified");
            assert_eq!(kept.votes[&(1, 4)].count, 30);
            assert!(!kept.votes.contains_key(&(0, 424_242)), "a title the store lacks is never kept");
            assert_eq!(kept.credits.len(), corpus.len(), "every corpus title's credits");
        }
        let state = crate::util::lock(&tmdb.state).clone();
        assert!(state.slices.is_empty() && state.sweep_started == now && state.last_tick == now);
        assert_eq!(state.spent as usize, asked.load(std::sync::atomic::Ordering::SeqCst));
        // Two from the sweep; the rest, which no discover page named, asked for by title once it finished.
        assert_eq!(read_votes(&dir.join(VOTES_FILE), now).len(), corpus.len());
        assert_eq!(read_credits(&dir.join(CREDITS_FILE), now).len(), corpus.len());
        assert_eq!(State::read(&dir.join(STATE_FILE)), state);
        assert!(tmdb.ratings().index().is_some_and(|i| i.matched() == corpus.len()));
        assert!(tmdb.characters().index().is_some_and(|i| i.links() > 0), "Walter White everywhere");

        // The next day the sweep is idle, and only what is due is asked.
        let before = asked.load(std::sync::atomic::Ordering::SeqCst);
        let line = tmdb.tick(now + DAY).await;
        assert!(line.contains("idle, the next in 29 days"), "{line}");
        assert!(asked.load(std::sync::atomic::Ordering::SeqCst) - before < corpus.len(), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The day's ceiling holds: a refresh stops when it is spent and a sweep carries on from where it stopped.
    #[tokio::test]
    async fn a_refresh_stops_at_the_days_ceiling_and_resumes_the_sweep() {
        let dir = temp("ceiling");
        let ds = crate::queries::write_fixture(&dir.join("ds"));
        let (base, asked) = fake_proxy().await;
        let mut tmdb = Tmdb::new(ds.store.clone(), Some(dir.clone()), Some(base), 3).unwrap();
        tmdb.proxy.as_mut().unwrap().pace = Duration::ZERO;
        let now = days_from_civil(2026, 9, 22) as u64 * DAY;
        let line = tmdb.tick(now).await;
        assert!(line.contains("stopped early: the day's TMDB_DAILY_MAX is spent"), "{line}");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 3);
        let left = crate::util::lock(&tmdb.state).slices.clone();
        assert!(!left.is_empty(), "the sweep is under way");
        assert_eq!(State::read(&dir.join(STATE_FILE)).slices, left, "and kept, for a restart");
        let line = tmdb.tick(now + DAY).await;
        assert!(!line.contains("next in"), "a sweep under way is carried on, not restarted: {line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot step reads the kept files and builds both indexes, without a proxy and without the network.
    #[tokio::test]
    async fn loads_what_is_kept_and_builds_both_indexes() {
        let dir = temp("load");
        let ds = crate::queries::write_fixture(&dir.join("ds"));
        let now = now_secs();
        write_votes(
            &dir.join(VOTES_FILE),
            &HashMap::from([((0, 1), Votes { average: 8.4, count: 9000, fetched: now - DAY })]),
        )
        .unwrap();
        let tmdb = Tmdb::new(ds.store.clone(), Some(dir.clone()), None, DEFAULT_DAILY_MAX).unwrap();
        let line = tmdb.load().await;
        assert!(line.contains("vote counts for 1 of 12 store rows"), "{line}");
        assert_eq!(tmdb.ratings().index().and_then(|i| i.of(0)), Some((9000, 8.4)));
        assert!(!tmdb.asks());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
