//! When a series' episodes come out, from TVmaze (https://www.tvmaze.com, a community TV database whose data is
//! CC BY-SA 4.0). TMDB dates an episode by its broadcaster's slot, so a streaming release that lands on a Friday
//! can read as the following Sunday; TVmaze dates it by the service that releases it. `GET /airing/tv/<tmdbId>`
//! answers from what is kept here, one series per request so every cache between atlas and a household can share
//! it.
//!
//! Nothing is asked on a request's behalf. A request names the series it wants (`Tvmaze::want`), and a
//! background pass (`refresh_forever`) resolves each IMDb id to a TVmaze show once (`/lookup/shows`, kept for
//! months), fetches the show with its episodes once, and asks again only when the daily `/updates/shows` read
//! (one request covering all of TVmaze) says TVmaze changed it, or a month on (`due`). One request at a time, `PACE` apart, so atlas stays well inside TVmaze's 20 requests per 10 s; a
//! 429 or 5xx is waited out (`Retry-After` when sent) before anything else is asked. A series nobody has asked for
//! in `WANTED_FOR` is dropped. What is kept lives in `CACHE_DIR`, so a restart asks for nothing it already has.
//!
//! TVmaze's `airstamp` for a streaming release is a placeholder noon UTC, so it gives the date and the service,
//! not the hour. An episode carries a `time` only for a broadcast network, with that network's time zone.

use crate::util::{backs_off, lock, retry_after, Backoff};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BASE: &str = "https://api.tvmaze.com";
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
/// How long a found TVmaze id is trusted before it is looked up again.
const LOOKUP_FOR: i64 = 90 * DAY;
/// How long a lookup TVmaze had no show for stands before it is asked again: a new series is added there in days.
const MISS_FOR: i64 = 7 * DAY;
/// A kept show the updates feed never named is fetched again after this, in case a change slipped past it.
const SAFETY_REFRESH: i64 = 30 * DAY;
/// How long a client or shared cache may keep an answer: a day's updates can move a date.
pub const MAX_AGE: i64 = 6 * HOUR;
/// How often `/updates/shows?since=day` is read, to refresh the kept shows TVmaze changed: a moved date shows
/// within a day whatever the show's own cadence (`cadence`).
const UPDATES_EVERY: i64 = DAY;
/// A series nobody has asked about in this long is no longer fetched or kept.
const WANTED_FOR: i64 = 30 * DAY;
/// How far ahead `upcoming` looks, in days.
const UPCOMING_DAYS: i64 = 14;
/// How often a pass looks for what is due. A request naming a series not yet kept wakes it at once.
const PASS_EVERY: Duration = Duration::from_secs(300);
/// Between two requests: at most ~17 in TVmaze's 10 s window of 20.
const PACE: Duration = Duration::from_millis(600);
/// How long a request naming a series atlas has never looked up waits for the pass to fetch it. Bounded, so a
/// slow or refusing TVmaze costs the request at most this.
pub const FIRST_WAIT: Duration = Duration::from_secs(2);
/// The pause after a refusal with no `Retry-After`, doubling with each refusal in a row up to the longest.
const REFUSED_PAUSE: Duration = Duration::from_secs(60);
const MAX_REFUSED_PAUSE: Duration = Duration::from_secs(3600);
/// The file under `CACHE_DIR`. A new format takes a new name, so an old file is ignored, not misread.
const FILE: &str = "tvmaze.v1.json";

/// One episode with a known date.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Episode {
    season: u32,
    number: u32,
    /// `None` for TVmaze's "TBA".
    name: Option<String>,
    /// `YYYY-MM-DD`, the day it comes out where it is released.
    date: String,
    /// `HH:MM` in the show's `zone`, only for a broadcast network that names one.
    time: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Show {
    /// When atlas fetched it; 0 marks it due.
    fetched: i64,
    /// TVmaze's own `updated`, compared with `/updates/shows`.
    updated: i64,
    /// `running`, `ended`, `tbd`, `development` or `unknown`.
    status: String,
    service: Option<String>,
    /// The broadcast network's IANA time zone; `None` for a streaming service.
    zone: Option<String>,
    /// From the season of the last episode out when it was fetched onwards, in TVmaze's order.
    episodes: Vec<Episode>,
}

/// The TVmaze id an IMDb id resolved to, `None` when TVmaze had no show, and when it was asked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Lookup {
    show: Option<u32>,
    at: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Kept {
    /// By IMDb id.
    ids: BTreeMap<String, Lookup>,
    shows: BTreeMap<u32, Show>,
    /// When each IMDb id was last asked about, to the day.
    wanted: BTreeMap<String, i64>,
    /// When `/updates/shows` was last read.
    updates_at: i64,
}

impl Kept {
    /// Whether nothing is kept for the series yet, neither a show nor TVmaze's answer that it has none.
    fn unanswered(&self, imdb: &str) -> bool {
        match self.ids.get(imdb) {
            None => true,
            Some(Lookup { show: Some(id), .. }) => !self.shows.contains_key(id),
            Some(Lookup { show: None, .. }) => false,
        }
    }
}

/// What `GET /airing/tv/<tmdbId>` answers for one series.
#[derive(Debug, PartialEq)]
pub enum Airing {
    /// The series' schedule.
    Show(Value),
    /// TVmaze has no show for it.
    Unknown,
    /// Not looked up or fetched yet.
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Job {
    Lookup(String),
    Fetch(u32),
}

enum Answer {
    Json(Value),
    /// A lookup answers with a redirect to the show it found.
    Moved(String),
    NotFound,
}

pub struct Tvmaze {
    enabled: bool,
    base: String,
    pace: Duration,
    http: reqwest::Client,
    kept: Mutex<Kept>,
    file: Option<PathBuf>,
    persisted: Mutex<Option<Vec<u8>>>,
    backoff: Backoff,
    /// Wakes the pass when a request names a series not yet kept.
    wake: tokio::sync::Notify,
    /// Told after each job, so a request waiting on a first lookup answers as soon as it lands.
    progress: tokio::sync::Notify,
    requests: AtomicU64,
    refusals: AtomicU64,
}

impl Tvmaze {
    /// Off: nothing is wanted, asked or kept, and `POST /airing` is a 404.
    pub fn off() -> Tvmaze {
        Tvmaze::with_base(false, BASE, None)
    }

    pub fn new(dir: Option<&Path>) -> Tvmaze {
        Tvmaze::with_base(true, BASE, dir)
    }

    fn with_base(enabled: bool, base: &str, dir: Option<&Path>) -> Tvmaze {
        let file = dir.map(|dir| dir.join(FILE));
        let loaded: Option<Kept> = file.as_ref().and_then(|file| match std::fs::read(file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| eprintln!("tvmaze: {} is unreadable ({e}) — starting empty", file.display()))
                .ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                eprintln!("tvmaze: could not read {} ({e}) — starting empty", file.display());
                None
            }
        });
        let persisted = loaded.as_ref().and_then(|kept| serde_json::to_vec(kept).ok());
        // No redirects: a lookup's answer is the redirect, and its Location names the show without a second request.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("den-atlas/{}", crate::release_version()))
            .build()
            .unwrap_or_default();
        Tvmaze {
            enabled,
            base: base.to_owned(),
            pace: PACE,
            http,
            kept: Mutex::new(loaded.unwrap_or_default()),
            file,
            persisted: Mutex::new(persisted),
            backoff: Backoff::new(REFUSED_PAUSE, MAX_REFUSED_PAUSE),
            wake: tokio::sync::Notify::new(),
            progress: tokio::sync::Notify::new(),
            requests: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Note that a household follows the series (by IMDb id), and wake the pass when it is not yet kept. Whether
    /// it is still to be fetched.
    pub fn want(&self, imdb: &str, now: i64) -> bool {
        if !self.enabled {
            return false;
        }
        let mut kept = lock(&self.kept);
        // To the day, so a pass after a busy hour has nothing new to write.
        if kept.wanted.get(imdb).is_none_or(|at| now - at >= DAY) {
            kept.wanted.insert(imdb.to_owned(), now);
        }
        let pending = kept.unanswered(imdb);
        drop(kept);
        if pending {
            self.wake.notify_one();
        }
        pending
    }

    /// Wait at most `wait` for the pass to answer a series not yet kept; at once while TVmaze is refusing.
    pub async fn settle(&self, imdb: &str, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.progress.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !lock(&self.kept).unanswered(imdb) || self.backoff.paused().is_some() {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// What is kept for the series as of `now`.
    pub fn airing(&self, imdb: &str, now: i64) -> Airing {
        let today = crate::tmdb::civil(now.div_euclid(DAY));
        let horizon = crate::tmdb::civil(now.div_euclid(DAY) + UPCOMING_DAYS);
        let kept = lock(&self.kept);
        if kept.unanswered(imdb) {
            return Airing::Pending;
        }
        match kept.ids.get(imdb).and_then(|l| l.show).and_then(|id| kept.shows.get(&id)) {
            Some(show) => Airing::Show(summary(show, &today, &horizon)),
            None => Airing::Unknown,
        }
    }

    pub async fn refresh_forever(self: Arc<Self>) {
        loop {
            self.pass(now()).await;
            tokio::select! {
                _ = tokio::time::sleep(PASS_EVERY) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    /// Look up, fetch and refresh what is wanted and due, one request at a time, and keep it.
    async fn pass(&self, now: i64) {
        if !self.enabled {
            return;
        }
        let (read_updates, since) = {
            let mut kept = lock(&self.kept);
            prune(&mut kept, now);
            // With nothing kept there is nothing the feed could make due; what this pass fetches is current.
            if kept.shows.is_empty() {
                kept.updates_at = now;
            }
            let gap = now - kept.updates_at;
            // A missed day (atlas down, TVmaze refusing) is covered by the week's feed; past that, `due`'s monthly
            // refresh is the net.
            (gap >= UPDATES_EVERY, if gap > 2 * DAY { "week" } else { "day" })
        };
        let started = self.requests.load(Ordering::Relaxed);
        let (mut lookups, mut fetches, mut changed) = (0, 0, 0);
        let mut first = true;
        if read_updates {
            first = false;
            if let Some(Answer::Json(updates)) = self.ask(&format!("/updates/shows?since={since}")).await {
                let mut kept = lock(&self.kept);
                for (id, show) in kept.shows.iter_mut() {
                    let updated = updates.get(id.to_string()).and_then(Value::as_i64).unwrap_or(0);
                    if updated > show.updated && show.fetched != 0 {
                        show.fetched = 0;
                        changed += 1;
                    }
                }
                kept.updates_at = now;
            }
        }
        let mut tried = HashSet::new();
        loop {
            let job = next_job(&lock(&self.kept), now, &tried);
            let Some(job) = job else { break };
            if self.backoff.paused().is_some() {
                break;
            }
            if !first {
                tokio::time::sleep(self.pace).await;
            }
            first = false;
            tried.insert(job.clone());
            match job {
                Job::Lookup(imdb) => {
                    lookups += 1;
                    let found = match self.ask(&format!("/lookup/shows?imdb={imdb}")).await {
                        Some(Answer::Moved(location)) => show_id(&location),
                        Some(Answer::Json(show)) => show["id"].as_u64().and_then(|id| u32::try_from(id).ok()),
                        Some(Answer::NotFound) => None,
                        None => continue,
                    };
                    lock(&self.kept).ids.insert(imdb, Lookup { show: found, at: now });
                }
                Job::Fetch(id) => {
                    fetches += 1;
                    match self.ask(&format!("/shows/{id}?embed[]=episodes")).await {
                        Some(Answer::Json(body)) => {
                            if let Some(show) = parse_show(&body, now) {
                                lock(&self.kept).shows.insert(id, show);
                            }
                        }
                        // The show is gone from TVmaze: every series that resolved to it is a miss until looked up again.
                        Some(Answer::NotFound) => {
                            let mut kept = lock(&self.kept);
                            kept.shows.remove(&id);
                            for lookup in kept.ids.values_mut().filter(|l| l.show == Some(id)) {
                                *lookup = Lookup { show: None, at: now };
                            }
                        }
                        Some(Answer::Moved(_)) | None => continue,
                    }
                }
            }
            self.progress.notify_waiters();
        }
        self.progress.notify_waiters();
        self.save();
        let asked = self.requests.load(Ordering::Relaxed) - started;
        if asked > 0 {
            let kept = lock(&self.kept);
            eprintln!(
                "tvmaze: {asked} requests ({lookups} lookups, {fetches} shows, {changed} changed upstream); \
                 {} shows kept for {} series{}",
                kept.shows.len(),
                kept.wanted.len(),
                self.backoff.paused().map_or(String::new(), |p| format!("; paused {}s", p.as_secs())),
            );
        }
    }

    /// One request, unless TVmaze is being left alone; `None` on any failure, which is logged.
    async fn ask(&self, path: &str) -> Option<Answer> {
        if self.backoff.paused().is_some() {
            return None;
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        let response = match self.http.get(format!("{}{path}", self.base)).send().await {
            Ok(response) => response,
            Err(e) => {
                self.refusals.fetch_add(1, Ordering::Relaxed);
                let pause = self.backoff.refused(None);
                eprintln!("tvmaze: {path}: {e} — asking nothing for {}s", pause.as_secs());
                return None;
            }
        };
        let status = response.status();
        if status.is_redirection() {
            self.backoff.answered();
            let location = response.headers().get(reqwest::header::LOCATION)?.to_str().ok()?.to_owned();
            return Some(Answer::Moved(location));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            self.backoff.answered();
            return Some(Answer::NotFound);
        }
        if !status.is_success() {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            let asked = retry_after(response.headers());
            let pause = if backs_off(status) {
                format!(" — asking nothing for {}s", self.backoff.refused(asked).as_secs())
            } else {
                String::new()
            };
            eprintln!("tvmaze: {path}: HTTP {status}{pause}");
            return None;
        }
        self.backoff.answered();
        let body = response.bytes().await.ok()?;
        serde_json::from_slice(&body)
            .map(Answer::Json)
            .map_err(|e| eprintln!("tvmaze: {path}: unreadable answer ({e})"))
            .ok()
    }

    /// Persist a changed snapshot; unchanged, nothing is written.
    fn save(&self) {
        let Some(file) = &self.file else { return };
        let snapshot = lock(&self.kept).clone();
        let Ok(bytes) = serde_json::to_vec(&snapshot) else { return };
        let mut persisted = lock(&self.persisted);
        if persisted.as_deref() == Some(bytes.as_slice()) {
            return;
        }
        match crate::motn::write_atomically(file, &bytes) {
            Ok(()) => *persisted = Some(bytes),
            Err(e) => eprintln!("tvmaze: could not keep what was fetched in {} ({e})", file.display()),
        }
    }

    /// `(series wanted, shows kept, requests made, refusals)` for `/metrics`.
    pub fn counts(&self) -> (u64, u64, u64, u64) {
        let kept = lock(&self.kept);
        (
            kept.wanted.len() as u64,
            kept.shows.len() as u64,
            self.requests.load(Ordering::Relaxed),
            self.refusals.load(Ordering::Relaxed),
        )
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Drop what nobody has asked about in `WANTED_FOR`, the shows no wanted series resolves to, and lookups past
/// their time that nobody wants.
fn prune(kept: &mut Kept, now: i64) {
    kept.wanted.retain(|_, at| now - *at < WANTED_FOR);
    let Kept { ids, shows, wanted, .. } = kept;
    ids.retain(|imdb, lookup| wanted.contains_key(imdb) || now - lookup.at < LOOKUP_FOR);
    let used: HashSet<u32> =
        wanted.keys().filter_map(|imdb| ids.get(imdb).and_then(|lookup| lookup.show)).collect();
    shows.retain(|id, _| used.contains(id));
}

/// What to ask next: a series never looked up, then a found show never fetched, then a lookup past its time, then
/// the most overdue show. Each at most once a pass.
fn next_job(kept: &Kept, now: i64, tried: &HashSet<Job>) -> Option<Job> {
    let untried = |job: &Job| !tried.contains(job);
    let lookup = |imdb: &String| Job::Lookup(imdb.clone());
    if let Some(job) =
        kept.wanted.keys().filter(|imdb| !kept.ids.contains_key(*imdb)).map(lookup).find(untried)
    {
        return Some(job);
    }
    let found: Vec<u32> = kept.wanted.keys().filter_map(|imdb| kept.ids.get(imdb)?.show).collect();
    if let Some(job) =
        found.iter().filter(|id| !kept.shows.contains_key(*id)).map(|id| Job::Fetch(*id)).find(untried)
    {
        return Some(job);
    }
    let stale = |l: &Lookup| now - l.at >= if l.show.is_some() { LOOKUP_FOR } else { MISS_FOR };
    if let Some(job) =
        kept.wanted.keys().filter(|imdb| kept.ids.get(*imdb).is_some_and(stale)).map(lookup).find(untried)
    {
        return Some(job);
    }
    found
        .iter()
        .filter_map(|id| Some((*id, kept.shows.get(id)?)))
        .filter(|(id, show)| due(show, now) && untried(&Job::Fetch(*id)))
        .min_by_key(|(_, show)| show.fetched)
        .map(|(id, _)| Job::Fetch(id))
}

/// Whether a kept show is fetched again: when the daily `/updates/shows` read marked it changed (`fetched` 0), or
/// as a safety refresh `SAFETY_REFRESH` after it was fetched. Nothing else: dates are known weeks ahead, the
/// updates feed catches a moved one within a day, and the apps' source search sees the release itself.
fn due(show: &Show, now: i64) -> bool {
    show.fetched == 0 || now - show.fetched >= SAFETY_REFRESH
}

/// The show id at the end of a lookup's `Location: …/shows/<id>`.
fn show_id(location: &str) -> Option<u32> {
    location.trim_end_matches('/').rsplit_once("/shows/")?.1.parse().ok()
}

/// A `/shows/<id>?embed[]=episodes` answer as what is kept: its episodes with a date, from the season of the last
/// one out as of `now` onwards (all of them before the first is out).
fn parse_show(value: &Value, now: i64) -> Option<Show> {
    value["id"].as_u64()?;
    let status = match value["status"].as_str() {
        Some("Running") => "running",
        Some("Ended") => "ended",
        Some("To Be Determined") => "tbd",
        Some("In Development") => "development",
        _ => "unknown",
    };
    let name = |v: &Value| v["name"].as_str().filter(|n| !n.is_empty()).map(str::to_owned);
    // The network a show premieres on decides its dates and hours; a streaming service has no broadcast hour.
    let (service, zone) = if value["network"].is_object() {
        let zone =
            value["network"]["country"]["timezone"].as_str().filter(|z| !z.is_empty()).map(str::to_owned);
        (name(&value["network"]), zone)
    } else {
        (name(&value["webChannel"]), None)
    };
    let mut episodes: Vec<Episode> = value["_embedded"]["episodes"]
        .as_array()
        .map(|episodes| {
            episodes
                .iter()
                .filter_map(|e| {
                    let date = e["airdate"].as_str().filter(|d| d.len() == 10)?.to_owned();
                    let time =
                        zone.as_ref().and(e["airtime"].as_str()).filter(|t| !t.is_empty()).map(str::to_owned);
                    Some(Episode {
                        season: u32::try_from(e["season"].as_u64()?).ok()?,
                        number: u32::try_from(e["number"].as_u64()?).ok()?,
                        name: e["name"].as_str().filter(|n| !n.is_empty() && *n != "TBA").map(str::to_owned),
                        date,
                        time,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let today = crate::tmdb::civil(now.div_euclid(DAY));
    if let Some(current) = episodes.iter().filter(|e| e.date < today).map(|e| e.season).max() {
        episodes.retain(|e| e.season >= current);
    }
    Some(Show {
        fetched: now,
        updated: value["updated"].as_i64().unwrap_or(0),
        status: status.to_owned(),
        service,
        zone,
        episodes,
    })
}

/// One series' answer as of `today`: the first episode out on or after it (`next`; on its release day an episode
/// is still `next`, so a client compares its date with its own local day), the last one before it, those out within
/// `UPCOMING_DAYS`, and the first episode of `next`'s season (else `previous`'s).
fn summary(show: &Show, today: &str, horizon: &str) -> Value {
    let mut episodes: Vec<&Episode> = show.episodes.iter().collect();
    episodes.sort_by(|a, b| (&a.date, a.season, a.number).cmp(&(&b.date, b.season, b.number)));
    let entry = |e: &Episode, named: bool| {
        let mut v = json!({ "season": e.season, "episode": e.number });
        if named {
            v["name"] = json!(e.name);
        }
        v["date"] = json!(e.date);
        if let (Some(time), Some(zone)) = (&e.time, &show.zone) {
            v["time"] = json!(time);
            v["zone"] = json!(zone);
        }
        v
    };
    let next = episodes.iter().copied().find(|e| e.date.as_str() >= today);
    let previous = episodes.iter().copied().rev().find(|e| e.date.as_str() < today);
    let upcoming: Vec<Value> = episodes
        .iter()
        .copied()
        .filter(|e| e.date.as_str() >= today && e.date.as_str() <= horizon)
        .map(|e| entry(e, false))
        .collect();
    let premiere = next.or(previous).and_then(|current| {
        episodes
            .iter()
            .filter(|e| e.season == current.season)
            .min_by_key(|e| e.number)
            .map(|e| json!({ "season": e.season, "date": e.date }))
    });
    json!({
        "source": "tvmaze",
        "status": show.status,
        "service": show.service,
        "next": next.map(|e| entry(e, true)),
        "previous": previous.map(|e| entry(e, true)),
        "upcoming": upcoming,
        "seasonPremiere": premiere,
    })
}

#[cfg(test)]
impl Tvmaze {
    /// On, asking `base` with no pacing, keeping nothing on disk.
    pub(crate) fn at(base: &str) -> Tvmaze {
        let mut tvmaze = Tvmaze::with_base(true, base, None);
        tvmaze.pace = Duration::ZERO;
        tvmaze
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::Router;

    const DEXTER: &str = include_str!("../tests/fixtures/tvmaze/78665.json");
    const ABBOTT: &str = include_str!("../tests/fixtures/tvmaze/55350.json");
    const BREAKING_BAD: &str = include_str!("../tests/fixtures/tvmaze/169.json");
    /// 2026-10-31T12:00:00Z: the day after Dexter: Resurrection's second season began on Paramount+.
    const NOW: i64 = 1_793_448_000;

    const DEXTER_IMDB: &str = "tt33043892";
    const ABBOTT_IMDB: &str = "tt14218830";
    const BREAKING_BAD_IMDB: &str = "tt0903747";

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("den-atlas-tvmaze-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in for TVmaze answering from the recorded fixtures as TVmaze does: a lookup is a 301 to the show,
    /// an unknown id a 404. `refuse` answers the first that many requests with a 429 and `Retry-After: 120`.
    pub(crate) async fn fake_tvmaze(
        refuse: usize,
        updates: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let counted = Arc::clone(&requests);
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let counted = Arc::clone(&counted);
            async move {
                let uri = request.uri().to_string();
                let seen = {
                    let mut requests = lock(&counted);
                    requests.push(uri.clone());
                    requests.len()
                };
                let respond = |status: u16, body: &'static str| {
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                };
                if seen <= refuse {
                    let mut refused = respond(429, "{}");
                    refused.headers_mut().insert("retry-after", "120".parse().unwrap());
                    return refused;
                }
                let lookups = [("tt33043892", 78665), ("tt14218830", 55350), ("tt0903747", 169)];
                if let Some(imdb) = uri.strip_prefix("/lookup/shows?imdb=") {
                    return match lookups.iter().find(|(id, _)| *id == imdb) {
                        Some((_, show)) => axum::response::Response::builder()
                            .status(301)
                            .header("location", format!("https://api.tvmaze.com/shows/{show}"))
                            .body(axum::body::Body::from("null"))
                            .unwrap(),
                        None => respond(404, "null"),
                    };
                }
                if uri.starts_with("/updates/shows") {
                    return respond(200, updates);
                }
                match uri.split('?').next().unwrap() {
                    "/shows/78665" => respond(200, DEXTER),
                    "/shows/55350" => respond(200, ABBOTT),
                    "/shows/169" => respond(200, BREAKING_BAD),
                    _ => respond(404, "null"),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), requests)
    }

    async fn fake(dir: Option<&Path>) -> (Tvmaze, Arc<Mutex<Vec<String>>>) {
        let (base, requests) = fake_tvmaze(0, "{}").await;
        let mut tvmaze = Tvmaze::with_base(true, &base, dir);
        tvmaze.pace = Duration::ZERO;
        (tvmaze, requests)
    }

    fn answer_for(tvmaze: &Tvmaze, imdb: &str, now: i64) -> Value {
        match tvmaze.airing(imdb, now) {
            Airing::Show(answer) => answer,
            other => panic!("{imdb}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_streaming_release_is_dated_by_its_service_without_an_hour() {
        let (tvmaze, requests) = fake(None).await;
        assert!(tvmaze.want(DEXTER_IMDB, NOW), "never looked up, so the answer is still to come");
        assert_eq!(tvmaze.airing(DEXTER_IMDB, NOW), Airing::Pending);
        tvmaze.pass(NOW).await;
        assert_eq!(
            lock(&requests).clone(),
            vec!["/lookup/shows?imdb=tt33043892", "/shows/78665?embed[]=episodes"],
            "one lookup, read off its redirect, and one show"
        );
        assert_eq!(
            answer_for(&tvmaze, DEXTER_IMDB, NOW),
            json!({
                "source": "tvmaze",
                "status": "running",
                "service": "Paramount+",
                "next": {"season": 2, "episode": 2, "name": "Young Blood", "date": "2026-11-06"},
                "previous": {"season": 2, "episode": 1, "name": "Risky Business", "date": "2026-10-30"},
                "upcoming": [
                    {"season": 2, "episode": 2, "date": "2026-11-06"},
                    {"season": 2, "episode": 3, "date": "2026-11-13"},
                ],
                "seasonPremiere": {"season": 2, "date": "2026-10-30"},
            })
        );
    }

    #[tokio::test]
    async fn on_its_release_day_an_episode_is_still_next() {
        let (tvmaze, _) = fake(None).await;
        tvmaze.want(DEXTER_IMDB, NOW);
        tvmaze.pass(NOW).await;
        let release_day = NOW + 6 * DAY;
        let answer = answer_for(&tvmaze, DEXTER_IMDB, release_day);
        assert_eq!(answer["next"]["date"], "2026-11-06");
        assert_eq!(answer["previous"]["episode"], 1);
    }

    #[tokio::test]
    async fn a_broadcast_episode_carries_its_networks_hour_and_zone() {
        let (tvmaze, _) = fake(None).await;
        tvmaze.want(ABBOTT_IMDB, NOW);
        tvmaze.pass(NOW).await;
        let answer = answer_for(&tvmaze, ABBOTT_IMDB, NOW);
        assert_eq!(answer["service"], "ABC");
        assert_eq!(
            answer["next"],
            json!({"season": 6, "episode": 5, "name": "Courtney", "date": "2026-11-04",
                   "time": "20:30", "zone": "America/New_York"})
        );
        assert_eq!(answer["previous"]["name"], "Halloween Party");
        assert_eq!(
            answer["upcoming"].as_array().unwrap().len(),
            2,
            "4 and 11 November; 2 December is too far"
        );
        assert_eq!(answer["seasonPremiere"], json!({"season": 6, "date": "2026-10-07"}));
        let kept = lock(&tvmaze.kept);
        assert!(kept.shows[&55350].episodes.iter().all(|e| e.season >= 6), "only the current season onwards");
    }

    #[tokio::test]
    async fn an_ended_show_has_no_next() {
        let (tvmaze, _) = fake(None).await;
        tvmaze.want(BREAKING_BAD_IMDB, NOW);
        tvmaze.pass(NOW).await;
        let answer = answer_for(&tvmaze, BREAKING_BAD_IMDB, NOW);
        assert_eq!(answer["status"], "ended");
        assert_eq!(answer["next"], Value::Null);
        assert_eq!(answer["upcoming"], json!([]));
        assert_eq!(answer["previous"]["name"], "Felina");
        assert_eq!(answer["seasonPremiere"], json!({"season": 5, "date": "2012-07-15"}));
    }

    #[tokio::test]
    async fn a_lookup_miss_is_kept_as_an_answer_and_asked_again_after_a_week() {
        let (tvmaze, requests) = fake(None).await;
        let unknown = "tt0000001";
        tvmaze.want(unknown, NOW);
        tvmaze.pass(NOW).await;
        assert_eq!(tvmaze.airing(unknown, NOW), Airing::Unknown, "answered: TVmaze has no show");
        assert!(!tvmaze.want(unknown, NOW + HOUR), "nothing more to wait for");
        tvmaze.pass(NOW + HOUR).await;
        assert_eq!(lock(&requests).len(), 1, "a miss is not asked again within the week");
        tvmaze.pass(NOW + 8 * DAY).await;
        assert_eq!(lock(&requests).len(), 2);
    }

    #[tokio::test]
    async fn a_429_is_waited_out_before_anything_else_is_asked() {
        let (base, requests) = fake_tvmaze(1, "{}").await;
        let mut tvmaze = Tvmaze::with_base(true, &base, None);
        tvmaze.pace = Duration::ZERO;
        tvmaze.want(DEXTER_IMDB, NOW);
        tvmaze.want(ABBOTT_IMDB, NOW);
        tvmaze.pass(NOW).await;
        assert_eq!(lock(&requests).len(), 1, "the pass stops at the refusal");
        let paused = tvmaze.backoff.paused().expect("Retry-After starts a pause");
        assert!(paused > Duration::from_secs(100), "as long as Retry-After asked: {paused:?}");
        tvmaze.pass(NOW + 60).await;
        assert_eq!(lock(&requests).len(), 1, "nothing is asked while the pause runs");
        let started = std::time::Instant::now();
        tvmaze.settle(DEXTER_IMDB, FIRST_WAIT).await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a request does not wait on a refusing TVmaze"
        );
        assert_eq!(tvmaze.counts().3, 1);
    }

    #[tokio::test]
    async fn a_show_is_fetched_again_only_when_the_daily_updates_feed_names_it() {
        let (base, requests) = fake_tvmaze(0, r#"{"78665": 1893387167, "1": 5}"#).await;
        let mut tvmaze = Tvmaze::with_base(true, &base, None);
        tvmaze.pace = Duration::ZERO;
        tvmaze.want(DEXTER_IMDB, NOW);
        tvmaze.want(ABBOTT_IMDB, NOW);
        tvmaze.pass(NOW).await;
        let asked = |path: &str| lock(&requests).iter().filter(|p| p.starts_with(path)).count();
        // Dexter's second episode is six days off and Abbott's four: neither is polled for it.
        tvmaze.pass(NOW + 12 * HOUR).await;
        assert_eq!(lock(&requests).len(), 4, "within the day nothing is asked at all");
        tvmaze.pass(NOW + DAY).await;
        assert_eq!(asked("/updates/shows?since=day"), 1, "one feed read a day covers every show");
        assert_eq!(asked("/shows/78665?"), 2, "TVmaze changed it, so it is fetched again");
        assert_eq!(asked("/shows/55350?"), 1, "an unchanged show is not");
        tvmaze.pass(NOW + 4 * DAY).await;
        assert_eq!(asked("/updates/shows?since=week"), 1, "a missed day is read from the week's feed");
    }

    #[test]
    fn a_show_is_due_when_marked_changed_or_a_month_after_its_fetch() {
        let show = |fetched| Show { fetched, ..Show::default() };
        assert!(due(&show(0), NOW), "the updates feed named it");
        assert!(!due(&show(NOW - 29 * DAY), NOW));
        assert!(due(&show(NOW - 30 * DAY), NOW), "the monthly safety refresh");
        let ended = Show { status: "ended".into(), ..show(NOW - 29 * DAY) };
        assert!(!due(&ended, NOW), "an ended show has nothing beyond the monthly refresh");
    }

    #[tokio::test]
    async fn what_was_fetched_survives_a_restart_without_asking_again() {
        let dir = temp("restart");
        {
            let (tvmaze, _) = fake(Some(&dir)).await;
            tvmaze.want(DEXTER_IMDB, NOW);
            tvmaze.pass(NOW).await;
        }
        assert!(dir.join(FILE).exists());
        let (restarted, requests) = fake(Some(&dir)).await;
        assert!(!restarted.want(DEXTER_IMDB, NOW + 60), "kept, so nothing to wait for");
        assert_eq!(answer_for(&restarted, DEXTER_IMDB, NOW + 60)["next"]["name"], "Young Blood");
        restarted.pass(NOW + 60).await;
        assert!(lock(&requests).is_empty(), "nothing is due after a restart: {:?}", lock(&requests));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_series_unasked_for_a_month_is_dropped() {
        let (tvmaze, _) = fake(None).await;
        tvmaze.want(DEXTER_IMDB, NOW);
        tvmaze.pass(NOW).await;
        tvmaze.pass(NOW + WANTED_FOR).await;
        let kept = lock(&tvmaze.kept);
        assert!(
            kept.wanted.is_empty() && kept.shows.is_empty(),
            "the show goes with the last series wanting it"
        );
        assert!(kept.ids.contains_key("tt33043892"), "its id is cheap and kept for its months");
    }

    #[tokio::test]
    async fn a_first_lookup_is_waited_for_briefly_and_answered_when_it_lands() {
        let (tvmaze, _) = fake(None).await;
        let tvmaze = Arc::new(tvmaze);
        tvmaze.want(DEXTER_IMDB, now());
        tokio::spawn(Tvmaze::refresh_forever(Arc::clone(&tvmaze)));
        tvmaze.settle(DEXTER_IMDB, Duration::from_secs(5)).await;
        assert!(matches!(tvmaze.airing(DEXTER_IMDB, now()), Airing::Show(_)), "answered within the wait");
    }

    #[test]
    fn a_lookup_redirect_names_the_show() {
        assert_eq!(show_id("https://api.tvmaze.com/shows/78665"), Some(78665));
        assert_eq!(show_id("/shows/169/"), Some(169));
        assert_eq!(show_id("https://api.tvmaze.com/people/1"), None);
    }

    #[test]
    fn off_wants_nothing() {
        let tvmaze = Tvmaze::off();
        assert!(!tvmaze.want(DEXTER_IMDB, NOW));
        assert_eq!(tvmaze.counts(), (0, 0, 0, 0));
    }
}
