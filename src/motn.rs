//! Movie of the Night's Streaming Availability API (`MOTN_KEY`): each service's real daily Top 10, and the titles
//! added to it, per country. JustWatch's "Popular on <service>" ranks by what people click on JustWatch and mixes old
//! catalogue in, so it matched nothing a service shows its own viewers; these lists lead those rows, and a billboard
//! reads a Top 10 place as attention. What is leaving a service or coming to it in the next month fills rows of its
//! own, which JustWatch has no list for.
//!
//! The free plan allows 1,000 requests a month, so nothing here is asked on a request's behalf. A request only says
//! which list for which service and country a household wants (`Motn::want*`). A background pass (`refresh_forever`)
//! fetches those on their own schedules, within `DAILY_REQUESTS`, and keeps the answers in `CACHE_DIR`; rows and
//! billboards read what is kept. A market/list nobody has asked for in a month is no longer fetched.

use crate::catalog::Provider;
use crate::justwatch::TrendingItem;
use crate::util::{backs_off, lock, retry_after, Backoff};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BASE: &str = "https://api.movieofthenight.com/v4";
/// Requests a day at most: 30 a day stays under the free plan's 1,000 a month in the longest month.
const DAILY_REQUESTS: u32 = 30;
const DAY: i64 = 86_400;
/// How old a kept list gets before the next pass asks again.
const REFRESH_AFTER: i64 = DAY;
/// How long a kept list is still served while it can't be refreshed.
const SERVE_FOR: i64 = 7 * DAY;
/// How far back the first look at a market's additions goes.
const FIRST_LOOK_BACK: i64 = 7 * DAY;
/// How long an addition stays on its "New on" list.
const KEEP_ADDED: i64 = 30 * DAY;
/// Pages of additions one pass reads for a country, 25 changes each. A read the pages don't finish is picked up where
/// it stopped by the next pass (`read_until`), so a busy country takes a few passes rather than losing its tail.
const ADDED_PAGES: u32 = 4;
/// A market nobody has asked for in this long is no longer fetched.
const WANTED_FOR: i64 = 30 * DAY;
/// How often the services each country offers are read again.
const SERVICES_AFTER: i64 = 7 * DAY;
/// How often a pass looks for what is due. A pass with nothing due asks nothing, so a market a household has just
/// asked for is fetched within minutes rather than the hour.
const PASS_EVERY: Duration = Duration::from_secs(600);
/// How often a country's leaving and coming lists are read again: they look a month ahead and change slowly.
const SOON_AFTER: i64 = 3 * DAY;
/// How far ahead leaving and coming lists look; the API allows 31 days.
const SOON_AHEAD: i64 = 30 * DAY;
/// Pages of leaving or coming titles one pass reads for a country.
const SOON_PAGES: u32 = 2;
/// The services the API knows what is coming to.
const UPCOMING: &[&str] = &["netflix", "prime", "disney", "apple", "hbo"];
/// The services the API has a Top 10 for.
const CHARTED: &[&str] = &["netflix", "prime", "disney", "apple", "hbo", "hulu"];
/// Countries the API doesn't cover, read as a neighbour whose catalogs are nearly the same: Argentina's Netflix Top 10
/// matched Uruguay's official one title for title.
const STAND_INS: &[(&str, &str)] = &[("UY", "AR")];
/// How long nothing is asked after the API refuses (429, 5xx) or can't be reached and says nothing of when to come
/// back, doubling with each refusal in a row up to the longest.
const REFUSED_PAUSE: Duration = Duration::from_secs(600);
const MAX_REFUSED_PAUSE: Duration = Duration::from_secs(6 * 3600);
/// The file under `CACHE_DIR`. A new format takes a new name, so an old file is ignored, not misread.
const FILE: &str = "motn.v1.json";

/// A title from one of the lists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Show {
    pub series: bool,
    pub tmdb: u32,
    pub imdb: Option<String>,
    pub title: String,
    pub year: Option<i64>,
    /// The API's 0–100 score as a 0–10 rating; `None` when it has none.
    pub rating: Option<f64>,
    /// When it was added to the service, leaves it or comes to it, for a change.
    #[serde(alias = "added")]
    pub at: Option<i64>,
}

/// What the passes have fetched. A market is `service@COUNTRY`, the country after `STAND_INS`.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Kept {
    /// Each market's titles leaving within `SOON_AHEAD`, soonest first, and those coming.
    leaving: BTreeMap<String, Vec<Show>>,
    coming: BTreeMap<String, Vec<Show>>,
    /// When each country's leaving and coming lists were read.
    soon_at: BTreeMap<String, i64>,
    /// Each market's Top 10, and when it was fetched.
    top: BTreeMap<String, (i64, Vec<Show>)>,
    /// Each market's additions, newest first.
    added: BTreeMap<String, Vec<Show>>,
    /// Each country's additions are read up to this moment: one read covers every service wanted there.
    added_until: BTreeMap<String, i64>,
    /// How far each individual market's additions have been read. A country's cursor may advance while a service is
    /// inactive, so this is the source of truth for whether that service needs a catch-up read when it returns.
    added_covered_until: BTreeMap<String, i64>,
    /// The services each country offers, by the API's lowercase country code, and when they were read.
    services: BTreeMap<String, Vec<String>>,
    services_at: i64,
    /// When each market's Top 10 was last asked for.
    wanted: BTreeMap<String, i64>,
    /// When each market's additions row was last asked for. Top 10 and additions have separate demand so the global
    /// Netflix chart used by recommendations does not also buy an unread US additions feed every day.
    added_wanted: BTreeMap<String, i64>,
    /// When each market's leaving/coming rows were last asked for. These are separate from Top 10/additions wants:
    /// reading an ordinary service row must not spend two more requests every three days warming rows nobody reads.
    soon_wanted: BTreeMap<String, i64>,
    /// Markets included in the last successful leaving/coming read for their country. Empty answers need an explicit
    /// marker too; absence means the row has not been read yet and must keep falling back until the next pass.
    soon_markets: BTreeSet<String>,
    /// Old v1 files had one `wanted` map for Top 10 and additions. Migrate it once, then keep those interests split.
    /// The filename remains v1 because all new fields have serde defaults and old readers ignore them.
    wants_split: bool,
    /// Migration marker kept separate because an intermediate v1 writer split Top/additions but not soon demand.
    soon_wants_split: bool,
    /// Migration marker for the per-market additions cursor, likewise independent of the demand migration.
    added_coverage_split: bool,
    /// In-progress `/changes` reads. Exact bounds, accumulated pages and cursor history are durable so a page limit
    /// or restart resumes the same snapshot rather than approximating progress with timestamps.
    changes: BTreeMap<String, ChangeProgress>,
    /// The day requests are counted for, and how many were spent on it.
    spent: (i64, u32),
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ChangeProgress {
    from: i64,
    to: i64,
    catalogs: Vec<String>,
    cursor: Option<String>,
    seen: BTreeSet<String>,
    read: Vec<(String, Show)>,
    /// A malformed cursor chain can never make progress safely. Keep that exact snapshot inert across passes and
    /// restarts rather than spending one more request every day. A materially different catalog set replaces it.
    quarantined: bool,
    /// Kept for migration from the short-lived daily-backoff format. Any non-zero value is promoted to `quarantined`
    /// on load, so an already observed bad chain does not get one more paid retry after upgrading.
    blocked_until: i64,
    complete: bool,
}

struct ChangeRead {
    key: String,
    to: i64,
    catalogs: Vec<String>,
    read: Vec<(String, Show)>,
}

pub struct Motn {
    key: Option<String>,
    base: String,
    http: reqwest::Client,
    kept: Mutex<Kept>,
    file: Option<PathBuf>,
    /// The canonical body represented by the file. Comparing snapshots makes a due-but-idle pass a true filesystem
    /// no-op without spreading a fallible dirty flag over every place that changes `Kept`.
    persisted: Mutex<Option<Vec<u8>>>,
    /// The day a request was last refused for the budget, so that is said once a day.
    refused_on: AtomicI64,
    /// A pause after the API refused or couldn't be reached: nothing is asked, or spent, until it ends.
    backoff: Backoff,
}

impl Motn {
    /// With no key it fetches nothing and every list is `None`, so rows and billboards read JustWatch alone.
    pub fn new(key: Option<String>, dir: Option<&Path>) -> Motn {
        let file = dir.map(|dir| dir.join(FILE));
        let loaded: Option<Kept> = file.as_ref().and_then(|file| match std::fs::read(file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| eprintln!("motn: {} is unreadable ({e}) — starting empty", file.display()))
                .ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                eprintln!("motn: could not read {} ({e}) — starting empty", file.display());
                None
            }
        });
        // Compare logical state, not the original spelling: an older valid file with omitted default fields or the
        // old `added` alias does not need a normalization-only rewrite.
        let persisted = loaded.as_ref().and_then(|kept| serde_json::to_vec(kept).ok());
        let mut kept = loaded.unwrap_or_default();
        if !kept.wants_split {
            kept.added_wanted.clone_from(&kept.wanted);
            kept.wants_split = true;
        }
        if !kept.soon_wants_split {
            kept.soon_wanted.clone_from(&kept.wanted);
            // A legacy country-wide soon read covered every wanted market, including markets with an empty answer
            // that consequently have no entry in `leaving` or `coming`.
            for market in kept.wanted.keys() {
                if kept.soon_at.contains_key(market_country(market)) {
                    kept.soon_markets.insert(market.clone());
                }
            }
            let cached_soon: Vec<String> = kept.leaving.keys().chain(kept.coming.keys()).cloned().collect();
            kept.soon_markets.extend(cached_soon);
            kept.soon_wants_split = true;
        }
        if !kept.added_coverage_split {
            // `keep_added` created an entry even for an empty answer, so a legacy entry proves this market took part
            // in the country read. Markets without one deliberately backfill on first demand.
            let cached_added: Vec<String> = kept.added.keys().cloned().collect();
            for market in cached_added {
                if let Some(until) = kept.added_until.get(market_country(&market)) {
                    kept.added_covered_until.insert(market, *until);
                }
            }
            kept.added_coverage_split = true;
        }
        for progress in kept.changes.values_mut() {
            if progress.blocked_until != 0 {
                progress.quarantined = true;
                progress.blocked_until = 0;
            }
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default();
        Motn {
            key: key.filter(|k| !k.is_empty()),
            base: BASE.to_owned(),
            http,
            kept: Mutex::new(kept),
            file,
            persisted: Mutex::new(persisted),
            refused_on: AtomicI64::new(-1),
            backoff: Backoff::new(REFUSED_PAUSE, MAX_REFUSED_PAUSE),
        }
    }

    pub fn enabled(&self) -> bool {
        self.key.is_some()
    }

    /// Note demand for only the service's Top 10.
    pub fn want_top(&self, provider: &Provider, country: &str) {
        if let (true, Some(market)) = (self.enabled(), market(provider, country)) {
            lock(&self.kept).wanted.insert(market, now());
        }
    }

    /// Note demand for only the service's additions.
    pub fn want_added(&self, provider: &Provider, country: &str) {
        if let (true, Some(market)) = (self.enabled(), market(provider, country)) {
            lock(&self.kept).added_wanted.insert(market, now());
        }
    }

    /// Note that a household wants the provider's leaving/coming rows. Kept separately so ordinary Top 10 and
    /// additions traffic does not implicitly subscribe every country to two unrelated change feeds.
    pub fn want_soon(&self, provider: &Provider, country: &str) {
        if let (true, Some(market)) = (self.enabled(), market(provider, country)) {
            lock(&self.kept).soon_wanted.insert(market, now());
        }
    }

    #[cfg(test)]
    pub(crate) fn wanted_lists(&self, provider: &Provider, country: &str) -> (bool, bool, bool) {
        let Some(market) = market(provider, country) else { return (false, false, false) };
        let kept = lock(&self.kept);
        (
            kept.wanted.contains_key(&market),
            kept.added_wanted.contains_key(&market),
            kept.soon_wanted.contains_key(&market),
        )
    }

    /// The service's Top 10 in the country, best first; `None` when none is kept, or the one kept is too old.
    pub fn top(&self, provider: &Provider, country: &str) -> Option<Vec<Show>> {
        let market = market(provider, country)?;
        let kept = lock(&self.kept);
        let (at, shows) = kept.top.get(&market)?;
        (now() - at < SERVE_FOR).then(|| shows.clone())
    }

    /// What was added to the service in the country, newest first; `None` when its additions aren't kept or are too
    /// old. An empty list is an answer: nothing was added.
    pub fn added(&self, provider: &Provider, country: &str) -> Option<Vec<Show>> {
        let market = market(provider, country)?;
        let kept = lock(&self.kept);
        let until = kept.added_covered_until.get(&market)?;
        (now() - until < SERVE_FOR).then(|| kept.added.get(&market).cloned()).flatten()
    }

    /// The service's titles leaving the country within a month, soonest first; `None` when that country's lists
    /// aren't kept or are too old.
    pub fn leaving(&self, provider: &Provider, country: &str) -> Option<Vec<Show>> {
        self.soon(provider, country, |kept| &kept.leaving)
    }

    /// The titles coming to the service in the country within a month, soonest first; as `leaving`.
    pub fn coming(&self, provider: &Provider, country: &str) -> Option<Vec<Show>> {
        self.soon(provider, country, |kept| &kept.coming)
    }

    fn soon(
        &self,
        provider: &Provider,
        country: &str,
        lists: fn(&Kept) -> &BTreeMap<String, Vec<Show>>,
    ) -> Option<Vec<Show>> {
        let market = market(provider, country)?;
        let kept = lock(&self.kept);
        if !kept.soon_markets.contains(&market) {
            return None;
        }
        let at = kept.soon_at.get(market_country(&market))?;
        (now() - at < SERVE_FOR).then(|| lists(&kept).get(&market).cloned().unwrap_or_default())
    }

    pub async fn refresh_forever(self: Arc<Self>) {
        loop {
            self.pass(now()).await;
            tokio::time::sleep(PASS_EVERY).await;
        }
    }

    /// Fetch what is wanted and due, within the day's budget, and keep it.
    async fn pass(&self, now: i64) -> bool {
        if !self.enabled() {
            return false;
        }
        let (wanted, added_wanted, soon_wanted, services_due) = {
            let mut kept = lock(&self.kept);
            kept.wanted.retain(|_, at| now - *at < WANTED_FOR);
            kept.added_wanted.retain(|_, at| now - *at < WANTED_FOR);
            kept.soon_wanted.retain(|_, at| now - *at < WANTED_FOR);
            let wanted = kept.wanted.keys().cloned().collect::<Vec<_>>();
            let added_wanted = kept.added_wanted.keys().cloned().collect::<Vec<_>>();
            let soon_wanted = kept.soon_wanted.keys().cloned().collect::<Vec<_>>();
            (wanted, added_wanted, soon_wanted, now - kept.services_at >= SERVICES_AFTER)
        };
        if wanted.is_empty() && added_wanted.is_empty() && soon_wanted.is_empty() {
            // `retain` may have removed the last expired market. Persist that real state change rather than
            // resurrecting it after a restart; an unchanged empty pass compares equal and writes nothing.
            return self.save();
        }
        if services_due {
            if let Some(countries) = self.ask("/countries?output_language=en").await {
                let services = offered_services(&countries);
                if !services.is_empty() {
                    let mut kept = lock(&self.kept);
                    kept.services = services;
                    kept.services_at = now;
                }
            }
        }
        let services = lock(&self.kept).services.clone();
        let wanted: Vec<String> = wanted.into_iter().filter(|m| offered(&services, m)).collect();
        let added_wanted: Vec<String> = added_wanted.into_iter().filter(|m| offered(&services, m)).collect();
        let soon_wanted: Vec<String> = soon_wanted.into_iter().filter(|m| offered(&services, m)).collect();

        for market in wanted.iter().filter(|m| CHARTED.contains(&market_service(m))) {
            if lock(&self.kept).top.get(market).is_some_and(|(at, _)| now - at < REFRESH_AFTER) {
                continue;
            }
            let path = format!(
                "/shows/top?country={}&service={}",
                market_country(market).to_ascii_lowercase(),
                market_service(market)
            );
            let Some(answer) = self.ask(&path).await else { continue };
            let shows = answer
                .as_array()
                .map(|a| a.iter().filter_map(|s| show(s, None)).collect())
                .unwrap_or_default();
            lock(&self.kept).top.insert(market.clone(), (now, shows));
        }

        let countries: BTreeSet<&str> = added_wanted.iter().map(|m| market_country(m)).collect();
        for &country in &countries {
            let catalogs = services_in(&added_wanted, country, None);
            let until = lock(&self.kept).added_until.get(country).copied();
            // A returning service may have missed reads while the country's cursor advanced. Catch only that market
            // up to the country cursor first. An unfinished paginated read advances its own cursor and resumes there.
            if let Some(until) = until {
                for &service in &catalogs {
                    let market = format!("{service}@{country}");
                    let covered = lock(&self.kept).added_covered_until.get(&market).copied();
                    if covered.is_some_and(|covered| covered >= until) {
                        continue;
                    }
                    let from = covered.unwrap_or((now - FIRST_LOOK_BACK).min(until));
                    let slot = format!("added-market:{market}");
                    if let Some(done) =
                        self.changes(&slot, country, "new", &[service], from, until, ADDED_PAGES).await
                    {
                        let mut kept = lock(&self.kept);
                        keep_added(&mut kept, std::slice::from_ref(&market), done.read, now);
                        kept.added_covered_until.insert(market, done.to);
                        kept.changes.remove(&done.key);
                    }
                }
            }
            let catalogs: Vec<&str> = catalogs
                .into_iter()
                .filter(|service| {
                    let market = format!("{service}@{country}");
                    until.is_none_or(|until| {
                        lock(&self.kept).added_covered_until.get(&market).is_some_and(|covered| *covered >= until)
                    })
                })
                .collect();
            let since = until.unwrap_or(0).max(now - FIRST_LOOK_BACK);
            if catalogs.is_empty() || now - since < REFRESH_AFTER {
                continue;
            }
            let slot = format!("added-country:{country}");
            if let Some(done) = self.changes(&slot, country, "new", &catalogs, since, now, ADDED_PAGES).await {
                let markets: Vec<String> =
                    done.catalogs.iter().map(|service| format!("{service}@{country}")).collect();
                let mut kept = lock(&self.kept);
                keep_added(&mut kept, &markets, done.read, now);
                kept.added_until.insert(country.to_owned(), done.to);
                for market in markets {
                    kept.added_covered_until.insert(market, done.to);
                }
                kept.changes.remove(&done.key);
            }
        }
        let soon_countries: BTreeSet<&str> = soon_wanted.iter().map(|m| market_country(m)).collect();
        for &country in &soon_countries {
            let all = services_in(&soon_wanted, country, None);
            let fresh = {
                let kept = lock(&self.kept);
                kept.soon_at.get(country).is_some_and(|at| now - at < SOON_AFTER)
                    && all.iter().all(|service| kept.soon_markets.contains(&format!("{service}@{country}")))
            };
            if fresh {
                continue;
            }
            let leaving = self
                .changes(
                    &format!("soon-expiring:{country}"),
                    country,
                    "expiring",
                    &all,
                    now,
                    now + SOON_AHEAD,
                    SOON_PAGES,
                )
                .await;
            let upcoming = services_in(&soon_wanted, country, Some(UPCOMING));
            let coming = if upcoming.is_empty() {
                Some(ChangeRead {
                    key: String::new(),
                    to: now + SOON_AHEAD,
                    catalogs: Vec::new(),
                    read: Vec::new(),
                })
            } else {
                self.changes(
                    &format!("soon-upcoming:{country}"),
                    country,
                    "upcoming",
                    &upcoming,
                    now,
                    now + SOON_AHEAD,
                    SOON_PAGES,
                )
                .await
            };
            let (Some(leaving), Some(coming)) = (leaving, coming) else { continue };
            let mut kept = lock(&self.kept);
            let ChangeRead { key: leaving_key, catalogs: read_catalogs, read: leaving, .. } = leaving;
            let ChangeRead { key: coming_key, read: coming, .. } = coming;
            keep_soon(&mut kept.leaving, country, leaving);
            keep_soon(&mut kept.coming, country, coming);
            kept.soon_markets.retain(|market| market_country(market) != country);
            kept.soon_markets.extend(read_catalogs.iter().map(|service| format!("{service}@{country}")));
            kept.soon_at.insert(country.to_owned(), now);
            kept.changes.remove(&leaving_key);
            if !coming_key.is_empty() {
                kept.changes.remove(&coming_key);
            }
        }
        self.save()
    }

    /// A country's complete changes snapshot. At most `pages` are bought by one pass; an unfinished query, its pages,
    /// cursor and every cursor already seen remain in `Kept` and resume on the next pass or after a restart.
    async fn changes(
        &self,
        slot: &str,
        country: &str,
        kind: &str,
        catalogs: &[&str],
        from: i64,
        to: i64,
        pages: u32,
    ) -> Option<ChangeRead> {
        let key = slot.to_owned();
        {
            let mut kept = lock(&self.kept);
            let requested_catalogs: Vec<String> = catalogs.iter().map(|catalog| (*catalog).to_owned()).collect();
            let progress = kept.changes.entry(key.clone()).or_insert_with(|| ChangeProgress {
                from,
                to,
                catalogs: requested_catalogs.clone(),
                ..ChangeProgress::default()
            });
            if progress.quarantined && progress.catalogs != requested_catalogs {
                *progress = ChangeProgress { from, to, catalogs: requested_catalogs, ..ChangeProgress::default() };
            }
            if progress.complete {
                return Some(ChangeRead {
                    key,
                    to: progress.to,
                    catalogs: progress.catalogs.clone(),
                    read: progress.read.clone(),
                });
            }
            if progress.quarantined {
                return None;
            }
        }
        for _ in 0..pages {
            let (query_from, query_to, catalogs, cursor) = {
                let kept = lock(&self.kept);
                let progress = kept.changes.get(&key)?;
                (progress.from, progress.to, progress.catalogs.clone(), progress.cursor.clone())
            };
            let mut query = vec![
                ("country", country.to_ascii_lowercase()),
                ("change_type", kind.to_owned()),
                ("item_type", "show".to_owned()),
                ("catalogs", catalogs.join(",")),
                ("from", query_from.to_string()),
                ("to", query_to.to_string()),
            ];
            if let Some(cursor) = cursor {
                query.push(("cursor", cursor));
            }
            let Some(page) = self.ask_query("/changes", &query).await else { return None };
            let page_read = changed(&page, country);
            let mut finished = None;
            {
                let mut kept = lock(&self.kept);
                let progress = kept.changes.get_mut(&key)?;
                for (market, show) in page_read {
                    if !progress.read.iter().any(|(held_market, held)| {
                        held_market == &market && held.series == show.series && held.tmdb == show.tmdb
                    }) {
                        progress.read.push((market, show));
                    }
                }
                if page["hasMore"].as_bool() != Some(true) {
                    progress.complete = true;
                    finished = Some(ChangeRead {
                        key: key.clone(),
                        to: progress.to,
                        catalogs: progress.catalogs.clone(),
                        read: progress.read.clone(),
                    });
                } else if let Some(next) = page["nextCursor"].as_str().filter(|next| !next.is_empty()) {
                    if progress.seen.insert(next.to_owned()) {
                        progress.cursor = Some(next.to_owned());
                    } else {
                        progress.quarantined = true;
                        eprintln!("motn: {slot}: cursor cycle — quarantining this changes snapshot");
                    }
                } else {
                    progress.quarantined = true;
                    eprintln!("motn: {slot}: hasMore without nextCursor — quarantining this changes snapshot");
                }
            }
            if !self.checkpoint() {
                return None;
            }
            if let Some(done) = finished {
                return Some(done);
            }
            if lock(&self.kept).changes.get(&key).is_some_and(|progress| progress.quarantined) {
                return None;
            }
        }
        None
    }

    /// One request, if the day's budget allows it and the API isn't being left alone; `None` on any failure, which is
    /// logged.
    async fn ask(&self, path: &str) -> Option<serde_json::Value> {
        self.ask_query(path, &[]).await
    }

    async fn ask_query(&self, path: &str, query: &[(&str, String)]) -> Option<serde_json::Value> {
        let key = self.key.as_deref()?;
        // Before the budget: a request never sent spends nothing, and the rest of a pass falls through here at once.
        if self.backoff.paused().is_some() {
            return None;
        }
        let now = now();
        if !self.reserve(now) {
            // Once a day: every pass until midnight is refused the same way.
            let day = now.div_euclid(DAY);
            if self.refused_on.swap(day, Ordering::Relaxed) != day {
                eprintln!("motn: the day's {DAILY_REQUESTS} requests are spent; what is still due waits for tomorrow");
            }
            return None;
        }
        let answer = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("X-API-Key", key)
            .send()
            .await;
        let response = match answer {
            Ok(response) => response,
            Err(e) => {
                let pause = self.backoff.refused(None);
                eprintln!("motn: {path}: {e} — asking nothing for {}s", pause.as_secs());
                return None;
            }
        };
        let status = response.status();
        let asked = retry_after(response.headers());
        let exhausted = crate::util::exhausted_for(response.headers());
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // A refusal of this one request (an unsupported country, say) leaves the others to be asked.
            let pause = if backs_off(status) {
                format!(" — asking nothing for {}s", self.backoff.refused(asked).as_secs())
            } else {
                String::new()
            };
            eprintln!("motn: {path}: HTTP {status}{pause}: {}", body.chars().take(200).collect::<String>());
            return None;
        }
        // An answer that also says the allowance is spent is waited out from here, rather than found out by a 429.
        match exhausted {
            Some(wait) => {
                self.backoff.refused(Some(wait));
                eprintln!("motn: the API's allowance is spent — asking nothing for {}s", wait.as_secs());
            }
            None => self.backoff.answered(),
        }
        serde_json::from_str(&body).map_err(|e| eprintln!("motn: {path}: unreadable answer ({e})")).ok()
    }

    /// Reserve one paid request and durably advance the daily counter before putting it on the wire. Without this
    /// checkpoint, a crash halfway through a pass restores the counter from the preceding pass and can buy the same
    /// work again after restart. With no configured cache this is an in-memory per-process ceiling. With a configured
    /// cache, inability to persist the reservation fails closed before network I/O.
    fn reserve(&self, now: i64) -> bool {
        if !spend(&mut lock(&self.kept), now) {
            return false;
        }
        self.file.is_none() || self.save()
    }

    fn checkpoint(&self) -> bool {
        self.file.is_none() || self.save()
    }

    /// Persist a changed logical snapshot. Returns whether the file was replaced.
    ///
    /// Request paths take `kept` briefly to read a list or refresh `wanted`; clone under that mutex, then serialize,
    /// sync and rename after releasing it. `persisted` serializes writers only and is never read by a request.
    fn save(&self) -> bool {
        let Some(file) = &self.file else { return false };
        let snapshot = lock(&self.kept).clone();
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("motn: could not serialise what was fetched ({e})");
                return false;
            }
        };
        let mut persisted = lock(&self.persisted);
        if persisted.as_deref() == Some(bytes.as_slice()) {
            return false;
        }
        match write_atomically(file, &bytes) {
            Ok(()) => {
                *persisted = Some(bytes);
                true
            }
            Err(e) => {
                eprintln!("motn: could not keep what was fetched in {} ({e})", file.display());
                false
            }
        }
    }
}

/// Write beside the current state, sync the complete replacement, then atomically publish it. Syncing the parent
/// makes the rename durable across a host crash, matching the stronger persistent-cache path used by maturity data.
fn write_atomically(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("json.partial");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output = options.open(&partial)?;
    #[cfg(unix)]
    output.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    output.write_all(body)?;
    output.sync_all()?;
    std::fs::rename(&partial, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// `shows` of one type as catalog items, in order.
pub fn trending(shows: &[Show], series: bool) -> Vec<TrendingItem> {
    shows
        .iter()
        .filter(|s| s.series == series)
        .filter_map(|s| Some((s.imdb.clone()?, s)))
        .enumerate()
        .map(|(rank, (imdb, s))| TrendingItem {
            imdb,
            moviedb: Some(i64::from(s.tmdb)),
            title: s.title.clone(),
            rank,
            // Movie of the Night's score is an average of several unnamed online sources. The catalog field is
            // explicitly `imdbRating`, so only JustWatch's sourced IMDb score may occupy it.
            rating: None,
            year: s.year,
            // The one source here that knows a date: when this title lands on the service, or leaves it.
            at: s.at,
        })
        .collect()
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn market(provider: &Provider, country: &str) -> Option<String> {
    let country = country.to_ascii_uppercase();
    let country =
        STAND_INS.iter().find(|(lacking, _)| *lacking == country).map_or(country.as_str(), |(_, s)| s);
    (!provider.motn.is_empty()).then(|| format!("{}@{country}", provider.motn))
}

fn market_service(market: &str) -> &str {
    market.split_once('@').map_or(market, |(service, _)| service)
}

fn market_country(market: &str) -> &str {
    market.split_once('@').map_or("", |(_, country)| country)
}

/// The services wanted in a country, only those in `only` when given.
fn services_in<'a>(wanted: &'a [String], country: &str, only: Option<&[&str]>) -> Vec<&'a str> {
    wanted
        .iter()
        .filter(|m| market_country(m) == country)
        .map(|m| market_service(m))
        .filter(|service| only.is_none_or(|only| only.contains(service)))
        .collect()
}

/// Whether the API offers the market's service in its country, as last read; unknown until then.
fn offered(services: &BTreeMap<String, Vec<String>>, market: &str) -> bool {
    services
        .get(&market_country(market).to_ascii_lowercase())
        .is_some_and(|offered| offered.iter().any(|s| s == market_service(market)))
}

/// `/countries`: each country's service ids.
fn offered_services(countries: &serde_json::Value) -> BTreeMap<String, Vec<String>> {
    countries
        .as_object()
        .map(|countries| {
            countries
                .iter()
                .map(|(code, country)| {
                    let services = country["services"].as_array().map_or_else(Vec::new, |services| {
                        services.iter().filter_map(|s| s["id"].as_str().map(str::to_owned)).collect()
                    });
                    (code.clone(), services)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Spend one request of the day's budget; `false` when it is spent.
fn spend(kept: &mut Kept, now: i64) -> bool {
    let day = now.div_euclid(DAY);
    if kept.spent.0 != day {
        kept.spent = (day, 0);
    }
    if kept.spent.1 >= DAILY_REQUESTS {
        return false;
    }
    kept.spent.1 += 1;
    true
}

/// A show object as a `Show`; `None` without a TMDB id or title.
fn show(value: &serde_json::Value, at: Option<i64>) -> Option<Show> {
    let (kind, id) = value["tmdbId"].as_str()?.split_once('/')?;
    Some(Show {
        series: kind == "tv",
        tmdb: id.parse().ok()?,
        imdb: value["imdbId"].as_str().filter(|id| id.starts_with("tt")).map(str::to_owned),
        title: value["title"].as_str()?.to_owned(),
        year: value["releaseYear"].as_i64().or_else(|| value["firstAirYear"].as_i64()),
        rating: value["rating"].as_f64().filter(|r| *r > 0.0).map(|r| r / 10.0),
        at,
    })
}

/// A `/changes` page's changes to a subscription, by market, each dated when the page dates it. A title to rent or
/// buy isn't on a service.
fn changed(page: &serde_json::Value, country: &str) -> Vec<(String, Show)> {
    page["changes"]
        .as_array()
        .map(|changes| {
            changes
                .iter()
                .filter(|c| {
                    matches!(c["streamingOptionType"].as_str(), Some("subscription" | "addon" | "free"))
                })
                .filter_map(|c| {
                    let service = c["service"]["id"].as_str()?;
                    let shown = show(&page["shows"][c["showId"].as_str()?], c["timestamp"].as_i64())?;
                    Some((format!("{service}@{country}"), shown))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fold newly read additions into what is kept: each title once per market, newest first, and nothing older than
/// `KEEP_ADDED`. Every market in `markets` was read, so each is kept even when nothing was added to it.
fn keep_added(kept: &mut Kept, markets: &[String], read: Vec<(String, Show)>, now: i64) {
    for market in markets {
        kept.added.entry(market.clone()).or_default();
    }
    for (market, show) in read {
        let list = kept.added.entry(market).or_default();
        if !list.iter().any(|held| held.series == show.series && held.tmdb == show.tmdb) {
            list.push(show);
        }
    }
    for list in kept.added.values_mut() {
        list.retain(|s| s.at.is_none_or(|at| now - at < KEEP_ADDED));
        list.sort_by_key(|s| std::cmp::Reverse(s.at.unwrap_or(0)));
    }
}

/// Replace a country's leaving or coming lists with what was just read: each title once per market, soonest first,
/// and one the API gave no date last.
fn keep_soon(lists: &mut BTreeMap<String, Vec<Show>>, country: &str, read: Vec<(String, Show)>) {
    lists.retain(|market, _| market_country(market) != country);
    for (market, show) in read {
        let list = lists.entry(market).or_default();
        if !list.iter().any(|held| held.series == show.series && held.tmdb == show.tmdb) {
            list.push(show);
        }
    }
    for (market, list) in lists.iter_mut() {
        if market_country(market) == country {
            list.sort_by_key(|s| s.at.unwrap_or(i64::MAX));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("den-atlas-motn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn provider(motn: &'static str) -> Provider {
        Provider { code: "x", id: "jw-x", name: "Popular on X", package_ids: &[], motn }
    }

    fn at(title: &str, tmdb: u32, when: i64) -> Show {
        Show {
            series: false,
            tmdb,
            imdb: None,
            title: title.into(),
            year: None,
            rating: None,
            at: Some(when),
        }
    }

    async fn counting_motn_with_changes(changes: &'static str) -> (Motn, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let counted = Arc::clone(&requests);
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let counted = Arc::clone(&counted);
            async move {
                let uri = request.uri();
                lock(&counted).push(uri.to_string());
                let body = if uri.path() == "/countries" {
                    r#"{"us":{"services":[{"id":"netflix"},{"id":"hbo"}]}}"#
                } else if uri.path() == "/shows/top" {
                    "[]"
                } else {
                    changes
                };
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut motn = Motn::new(Some("test-key".into()), None);
        motn.base = format!("http://{address}");
        (motn, requests)
    }

    async fn counting_motn() -> (Motn, Arc<Mutex<Vec<String>>>) {
        counting_motn_with_changes(r#"{"changes":[],"shows":{},"hasMore":false}"#).await
    }

    async fn scripted_server(pages: Vec<serde_json::Value>) -> (String, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let counted = Arc::clone(&requests);
        let pages = Arc::new(pages);
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let counted = Arc::clone(&counted);
            let pages = Arc::clone(&pages);
            async move {
                let mut requests = lock(&counted);
                let at = requests.len().min(pages.len().saturating_sub(1));
                requests.push(request.uri().to_string());
                let body = serde_json::to_vec(&pages[at]).unwrap();
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), requests)
    }

    fn change_page(first: u32, count: u32, timestamp: i64, next: Option<&str>) -> serde_json::Value {
        let changes: Vec<_> = (first..first + count)
            .map(|id| {
                serde_json::json!({"showId": id.to_string(), "service": {"id": "netflix"},
                    "streamingOptionType": "subscription", "timestamp": timestamp})
            })
            .collect();
        let shows: serde_json::Map<String, serde_json::Value> = (first..first + count)
            .map(|id| {
                (id.to_string(), serde_json::json!({"tmdbId": format!("movie/{id}"), "title": format!("Show {id}")}))
            })
            .collect();
        serde_json::json!({"changes": changes, "shows": shows, "hasMore": next.is_some(), "nextCursor": next})
    }

    #[test]
    fn leaving_and_coming_lists_are_replaced_per_country_soonest_first() {
        let mut lists = BTreeMap::from([
            ("netflix@FI".to_owned(), vec![at("Gone by now", 1, 5)]),
            ("netflix@US".to_owned(), vec![at("Another country", 2, 5)]),
        ]);
        let undated = Show { at: None, ..at("Some day", 3, 0) };
        keep_soon(
            &mut lists,
            "FI",
            vec![
                ("netflix@FI".into(), undated),
                ("netflix@FI".into(), at("Friday", 4, 300)),
                ("netflix@FI".into(), at("Tomorrow", 5, 100)),
                ("netflix@FI".into(), at("Tomorrow again", 5, 100)),
            ],
        );
        let titles: Vec<&str> = lists["netflix@FI"].iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Tomorrow", "Friday", "Some day"]);
        assert_eq!(lists["netflix@US"].len(), 1, "another country's list is left as it was");
    }

    #[test]
    fn a_file_kept_before_leaving_and_coming_lists_still_reads() {
        let kept: Kept = serde_json::from_str(
            r#"{"top":{},"added":{"hbo@FI":[{"series":false,"tmdb":1,"imdb":null,"title":"A","year":null,
                "rating":null,"added":7}]},"added_until":{},"services":{},"services_at":0,"wanted":{},"spent":[0,0]}"#,
        )
        .unwrap();
        assert_eq!(kept.added["hbo@FI"][0].at, Some(7));
        assert!(kept.leaving.is_empty());
    }

    #[test]
    fn legacy_single_demand_is_migrated_without_hiding_cached_empty_lists() {
        let dir = temp("legacy-demand");
        let at = now();
        let body = serde_json::json!({
            "leaving": {},
            "coming": {},
            "soon_at": {"US": at},
            "top": {},
            "added": {"netflix@US": []},
            "added_until": {"US": at},
            "services": {"us": ["netflix"]},
            "services_at": at,
            "wanted": {"netflix@US": at},
            "spent": [at.div_euclid(DAY), 0]
        });
        std::fs::write(dir.join(FILE), serde_json::to_vec(&body).unwrap()).unwrap();

        let motn = Motn::new(Some("unused".into()), Some(&dir));
        let kept = lock(&motn.kept);
        assert_eq!(kept.added_wanted.get("netflix@US"), Some(&at));
        assert_eq!(kept.soon_wanted.get("netflix@US"), Some(&at));
        assert!(kept.changes.is_empty(), "old files default to no in-progress pagination state");
        assert!(kept.soon_markets.contains("netflix@US"), "a cached empty soon answer remains an answer");
        assert_eq!(kept.added_covered_until.get("netflix@US"), Some(&at));
        drop(kept);
        assert_eq!(motn.leaving(&provider("netflix"), "US"), Some(Vec::new()));
        assert_eq!(motn.added(&provider("netflix"), "US"), Some(Vec::new()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_a_show_and_leaves_out_what_it_cannot_name() {
        let series = serde_json::json!({"tmdbId": "tv/331616", "imdbId": "tt44113382",
            "title": "Death of the Pastor's Wife", "firstAirYear": 2026, "rating": 69});
        assert_eq!(
            show(&series, None),
            Some(Show {
                series: true,
                tmdb: 331_616,
                imdb: Some("tt44113382".into()),
                title: "Death of the Pastor's Wife".into(),
                year: Some(2026),
                rating: Some(6.9),
                at: None,
            })
        );
        let unrated = serde_json::json!({"tmdbId": "movie/860508", "title": "The Whisper Man", "rating": 0});
        assert_eq!(show(&unrated, Some(5)).map(|s| (s.series, s.rating, s.at)), Some((false, None, Some(5))));
        assert_eq!(show(&serde_json::json!({"title": "No id"}), None), None);
    }

    #[test]
    fn a_page_of_changes_reads_as_subscription_additions_by_market() {
        let page = serde_json::json!({
            "changes": [
                {"showId": "1", "service": {"id": "netflix"}, "streamingOptionType": "subscription", "timestamp": 100},
                {"showId": "2", "service": {"id": "prime"}, "streamingOptionType": "rent", "timestamp": 100},
                {"showId": "9", "service": {"id": "hbo"}, "streamingOptionType": "subscription", "timestamp": 100}
            ],
            "shows": {"1": {"tmdbId": "movie/752", "title": "V for Vendetta"}, "2": {"tmdbId": "movie/1", "title": "Rented"}}
        });
        let read = changed(&page, "AR");
        assert_eq!(read.len(), 1, "a rental and a change without its show are left out");
        assert_eq!((read[0].0.as_str(), read[0].1.tmdb, read[0].1.at), ("netflix@AR", 752, Some(100)));
    }

    #[test]
    fn additions_are_kept_once_newest_first_for_a_month() {
        let mut kept = Kept::default();
        let now = 100 * DAY;
        let markets = ["hbo@FI".to_owned(), "netflix@FI".to_owned()];
        keep_added(
            &mut kept,
            &markets,
            vec![("hbo@FI".into(), at("Old", 1, now - 40 * DAY))],
            now - 10 * DAY,
        );
        keep_added(
            &mut kept,
            &markets,
            vec![("hbo@FI".into(), at("Newer", 2, now - DAY)), ("hbo@FI".into(), at("Newer again", 2, now))],
            now,
        );
        let titles: Vec<&str> = kept.added["hbo@FI"].iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Newer"], "a repeat is dropped and an addition past a month ages out");
        assert_eq!(
            kept.added["netflix@FI"],
            vec![],
            "a market read with nothing added is kept as nothing added"
        );
    }

    #[test]
    fn a_service_is_served_its_additions_only_once_they_have_been_read() {
        let motn = Motn::new(None, None);
        {
            let mut kept = lock(&motn.kept);
            kept.added_until.insert("FI".into(), now());
            kept.added_covered_until.insert("netflix@FI".into(), now());
            kept.added.insert("netflix@FI".into(), vec![at("New", 1, now())]);
        }
        assert_eq!(motn.added(&provider("netflix"), "fi").map(|a| a.len()), Some(1));
        assert_eq!(motn.added(&provider("hbo"), "fi"), None, "not read yet, so JustWatch's row stands");
    }

    #[test]
    fn the_budget_is_counted_per_day() {
        let mut kept = Kept::default();
        let day = 10 * DAY;
        assert!((0..DAILY_REQUESTS).all(|_| spend(&mut kept, day)));
        assert!(!spend(&mut kept, day + 60));
        assert!(spend(&mut kept, day + DAY), "a new day starts a new budget");
    }

    #[test]
    fn each_reserved_call_survives_a_restart_before_the_pass_finishes() {
        let dir = temp("reserved-budget");
        let at = 10 * DAY;
        let motn = Motn::new(Some("unused".into()), Some(&dir));
        assert!(motn.reserve(at));
        assert_eq!(lock(&motn.kept).spent, (10, 1));
        let restarted = Motn::new(Some("unused".into()), Some(&dir));
        assert_eq!(lock(&restarted.kept).spent, (10, 1), "a restart cannot buy that call a second time");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_unwritable_configured_cache_refuses_before_network_io() {
        let (mut motn, requests) = counting_motn().await;
        let dir = temp("unwritable-reservation");
        motn.file = Some(dir.join(FILE));
        std::fs::create_dir(motn.file.as_ref().unwrap().with_extension("json.partial")).unwrap();

        assert_eq!(motn.ask("/countries").await, None);
        assert!(lock(&requests).is_empty(), "a failed durable reservation must not reach the server");
        assert_eq!(lock(&motn.kept).spent.1, 1, "the failed attempt remains conservatively spent in memory");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_country_the_api_lacks_is_read_as_its_stand_in_and_a_service_without_an_id_is_not_asked() {
        assert_eq!(market(&provider("netflix"), "uy").as_deref(), Some("netflix@AR"));
        assert_eq!(market(&provider("hbo"), "FI").as_deref(), Some("hbo@FI"));
        assert_eq!(market(&provider(""), "FI"), None);
        let services = BTreeMap::from([("ar".to_owned(), vec!["netflix".to_owned()])]);
        assert!(offered(&services, "netflix@AR"));
        assert!(!offered(&services, "skyshowtime@AR"));
    }

    #[test]
    fn a_list_of_one_type_becomes_catalog_items_with_ids() {
        let mut series = at("Moria", 322_428, 0);
        series.series = true;
        series.imdb = Some("tt41559147".into());
        let film =
            Show { imdb: Some("tt11561116".into()), rating: Some(8.9), ..at("The Whisper Man", 860_508, 0) };
        let items = trending(&[series.clone(), film, at("No imdb", 5, 0)], false);
        assert_eq!(
            items.iter().map(|i| (i.imdb.as_str(), i.rank)).collect::<Vec<_>>(),
            vec![("tt11561116", 0)]
        );
        assert_eq!(items[0].rating, None, "a multi-source aggregate is not relabeled as IMDb");
        assert_eq!(trending(&[series], true)[0].moviedb, Some(322_428));
    }

    #[test]
    fn without_a_key_nothing_is_wanted_or_kept() {
        let motn = Motn::new(None, None);
        let netflix = provider("netflix");
        motn.want_top(&netflix, "US");
        motn.want_added(&netflix, "US");
        motn.want_soon(&netflix, "US");
        assert_eq!(motn.wanted_lists(&netflix, "US"), (false, false, false));
        assert_eq!(motn.top(&provider("netflix"), "US"), None);
    }

    #[tokio::test]
    async fn ordinary_rows_call_once_per_chart_and_once_per_country_not_the_unread_soon_feeds() {
        let (motn, requests) = counting_motn().await;
        let at = 100 * DAY;
        {
            let mut kept = lock(&motn.kept);
            kept.wanted.insert("netflix@US".into(), at);
            kept.wanted.insert("hbo@US".into(), at);
            kept.added_wanted.insert("netflix@US".into(), at);
            kept.added_wanted.insert("hbo@US".into(), at);
        }

        motn.pass(at).await;
        let asked = lock(&requests).clone();
        assert_eq!(asked.len(), 4, "countries + two Top 10s + one country-wide additions read");
        assert_eq!(asked.iter().filter(|path| path.starts_with("/shows/top?")).count(), 2);
        assert_eq!(asked.iter().filter(|path| path.contains("change_type=new")).count(), 1);
        assert_eq!(asked.iter().filter(|path| path.contains("change_type=expiring")).count(), 0);
        assert_eq!(asked.iter().filter(|path| path.contains("change_type=upcoming")).count(), 0);

        motn.pass(at + DAY).await;
        assert_eq!(lock(&requests).len(), 7, "the next day adds only two Top 10s and one additions read");
    }

    #[tokio::test]
    async fn a_top_only_market_never_calls_the_additions_feed() {
        let (motn, requests) = counting_motn().await;
        let at = 100 * DAY;
        lock(&motn.kept).wanted.insert("netflix@US".into(), at);

        motn.pass(at).await;
        let asked = lock(&requests).clone();
        assert_eq!(asked.len(), 2, "countries + Top 10 only: {asked:?}");
        assert_eq!(asked.iter().filter(|path| path.starts_with("/shows/top?")).count(), 1);
        assert_eq!(asked.iter().filter(|path| path.contains("/changes?")).count(), 0);
    }

    #[tokio::test]
    async fn a_reactivated_market_reads_only_its_missing_additions_interval_before_joining_the_country() {
        let (motn, requests) = counting_motn().await;
        let at = 100 * DAY;
        let covered = at - 3 * DAY;
        let country_until = at - DAY;
        {
            let mut kept = lock(&motn.kept);
            kept.services.insert("us".into(), vec!["netflix".into()]);
            kept.services_at = at;
            kept.added_wanted.insert("netflix@US".into(), at);
            kept.added.insert("netflix@US".into(), Vec::new());
            kept.added_covered_until.insert("netflix@US".into(), covered);
            kept.added_until.insert("US".into(), country_until);
        }

        motn.pass(at).await;
        let asked = lock(&requests).clone();
        assert_eq!(asked.len(), 2, "one catch-up and one current country read: {asked:?}");
        assert!(
            asked[0].contains(&format!("change_type=new&item_type=show&catalogs=netflix&from={covered}&to={country_until}")),
            "the first call covers exactly the inactive interval: {}",
            asked[0]
        );
        assert!(
            asked[1].contains(&format!("change_type=new&item_type=show&catalogs=netflix&from={country_until}&to={at}")),
            "only after catch-up does the market join the current read: {}",
            asked[1]
        );
        let kept = lock(&motn.kept);
        assert_eq!(kept.added_covered_until.get("netflix@US"), Some(&at));
        assert_eq!(kept.added_until.get("US"), Some(&at));
    }

    #[tokio::test]
    async fn a_page_claiming_more_without_a_cursor_is_quarantined_until_the_operation_changes() {
        let dir = temp("missing-cursor");
        let (base, requests) = scripted_server(vec![
            serde_json::json!({"changes": [], "shows": {}, "hasMore": true}),
            serde_json::json!({"changes": [], "shows": {}, "hasMore": false}),
        ])
        .await;
        let mut motn = Motn::new(Some("test-key".into()), Some(&dir));
        motn.base.clone_from(&base);
        assert!(motn.changes("test", "US", "new", &["netflix"], 1, 2, ADDED_PAGES).await.is_none());
        for _ in 0..100 {
            assert!(motn.changes("test", "US", "new", &["netflix"], 1, 2, ADDED_PAGES).await.is_none());
        }
        assert_eq!(lock(&requests).len(), 1, "arbitrarily many passes spend nothing on the quarantined snapshot");

        let mut restarted = Motn::new(Some("test-key".into()), Some(&dir));
        restarted.base.clone_from(&base);
        for _ in 0..100 {
            assert!(restarted.changes("test", "US", "new", &["netflix"], 1, 2, ADDED_PAGES).await.is_none());
        }
        assert_eq!(lock(&requests).len(), 1, "restarts do not revive the quarantined snapshot");

        let recovered = restarted.changes("test", "US", "new", &["hbo"], 1, 2, ADDED_PAGES).await.unwrap();
        assert_eq!(recovered.catalogs, vec!["hbo"]);
        assert_eq!(lock(&requests).len(), 2, "a materially different catalog set starts one new operation");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_non_adjacent_cursor_cycle_stops_before_rebuying_a_page_and_stays_quarantined() {
        let dir = temp("cursor-cycle");
        let (base, requests) = scripted_server(vec![
            change_page(1, 1, 10, Some("A")),
            change_page(2, 1, 10, Some("B")),
            change_page(3, 1, 10, Some("A")),
        ])
        .await;
        let mut motn = Motn::new(Some("test-key".into()), Some(&dir));
        motn.base = base;
        assert!(motn.changes("test", "US", "new", &["netflix"], 1, 20, ADDED_PAGES).await.is_none());
        assert_eq!(lock(&requests).len(), 3);
        assert!(motn.changes("test", "US", "new", &["netflix"], 1, 20, ADDED_PAGES).await.is_none());
        assert_eq!(lock(&requests).len(), 3, "A must not be requested a second time");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_cursor_is_encoded_as_one_query_value() {
        let (base, requests) = scripted_server(vec![
            change_page(1, 1, 10, Some("A?part=one&part=two")),
            change_page(2, 1, 10, None),
        ])
        .await;
        let mut motn = Motn::new(Some("test-key".into()), None);
        motn.base = base;

        let done = motn.changes("test", "US", "new", &["netflix"], 1, 20, ADDED_PAGES).await.unwrap();
        assert_eq!(done.read.len(), 2);
        let asked = lock(&requests);
        assert_eq!(asked.len(), 2);
        assert!(asked[1].contains("cursor=A%3Fpart%3Done%26part%3Dtwo"), "cursor was not encoded: {}", asked[1]);
        assert!(!asked[1].contains("&part=two"), "cursor escaped into a second query parameter: {}", asked[1]);
    }

    #[tokio::test]
    async fn a_page_limited_same_timestamp_read_resumes_by_cursor_after_restart_without_losing_titles() {
        let dir = temp("cursor-restart");
        let timestamp = 1234;
        let (base, requests) = scripted_server(vec![
            change_page(1, 25, timestamp, Some("A")),
            change_page(26, 25, timestamp, Some("B")),
            change_page(51, 25, timestamp, Some("C")),
            change_page(76, 25, timestamp, Some("D")),
            change_page(101, 25, timestamp, None),
        ])
        .await;
        let mut motn = Motn::new(Some("test-key".into()), Some(&dir));
        motn.base.clone_from(&base);
        assert!(motn.changes("test", "US", "new", &["netflix"], 1, 2000, ADDED_PAGES).await.is_none());
        assert_eq!(lock(&requests).len(), 4, "one pass observes its page ceiling");

        let mut restarted = Motn::new(Some("test-key".into()), Some(&dir));
        restarted.base = base;
        let done = restarted
            .changes("test", "US", "new", &["hbo", "netflix"], 20, 3000, ADDED_PAGES)
            .await
            .unwrap();
        assert_eq!(lock(&requests).len(), 5, "the restart resumes at cursor D");
        assert!(lock(&requests)[4].contains("cursor=D"));
        assert!(lock(&requests)[4].contains("catalogs=netflix&from=1&to=2000"));
        assert_eq!(done.catalogs, vec!["netflix"]);
        assert_eq!(done.read.len(), 125);
        assert!(done.read.iter().all(|(_, show)| show.at == Some(timestamp)));
        assert_eq!(done.to, 2000, "coverage advances only when the exact snapshot completes");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn soon_rows_make_exactly_two_country_wide_calls_every_three_days() {
        let (motn, requests) = counting_motn().await;
        let at = 100 * DAY;
        {
            let mut kept = lock(&motn.kept);
            kept.soon_wanted.insert("netflix@US".into(), at);
            kept.soon_wanted.insert("hbo@US".into(), at);
        }

        motn.pass(at).await;
        let asked = lock(&requests).clone();
        assert_eq!(
            asked.len(),
            3,
            "countries + one expiring + one upcoming read for both providers: {asked:?}"
        );
        assert_eq!(asked.iter().filter(|path| path.contains("change_type=expiring")).count(), 1);
        assert_eq!(asked.iter().filter(|path| path.contains("change_type=upcoming")).count(), 1);
        assert!(asked.iter().filter(|path| path.contains("catalogs=hbo%2Cnetflix")).count() >= 1);

        motn.pass(at + SOON_AFTER).await;
        assert_eq!(lock(&requests).len(), 5, "the next due pass adds the same two country-wide calls");
    }

    #[test]
    fn a_new_soon_market_is_unknown_until_a_pass_has_read_it() {
        let motn = Motn::new(Some("unused".into()), None);
        let at = now();
        {
            let mut kept = lock(&motn.kept);
            kept.soon_at.insert("US".into(), at);
            kept.soon_markets.insert("netflix@US".into());
        }
        assert_eq!(motn.leaving(&provider("netflix"), "US"), Some(Vec::new()));
        assert_eq!(motn.leaving(&provider("hbo"), "US"), None, "an unread service keeps its fallback");
    }

    #[tokio::test]
    async fn an_unchanged_pass_does_not_replace_the_state_file() {
        let dir = temp("unchanged");
        let at = 100 * DAY;
        let motn = Motn::new(Some("unused".into()), Some(&dir));
        {
            let mut kept = lock(&motn.kept);
            kept.wanted.insert("netflix@US".into(), at);
            kept.services.insert("us".into(), vec!["netflix".into()]);
            kept.services_at = at;
            kept.top.insert("netflix@US".into(), (at, Vec::new()));
            kept.added.insert("netflix@US".into(), Vec::new());
            kept.added_until.insert("US".into(), at);
            kept.soon_at.insert("US".into(), at);
        }
        assert!(motn.save());
        let before = std::fs::read(dir.join(FILE)).unwrap();
        assert!(!motn.pass(at).await, "a due pass with identical logical state must not touch the file");
        assert_eq!(std::fs::read(dir.join(FILE)).unwrap(), before);
        assert!(!dir.join("motn.v1.json.partial").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_wanted_timestamp_is_persisted_and_read_after_restart() {
        let dir = temp("wanted");
        let motn = Motn::new(Some("unused".into()), Some(&dir));
        lock(&motn.kept).wanted.insert("netflix@US".into(), 1234);
        assert!(motn.save());
        assert!(!motn.save(), "the same snapshot is not written twice");
        lock(&motn.kept).wanted.insert("netflix@US".into(), 5678);
        assert!(motn.save(), "a timestamp change is state, not an idle pass");
        let restarted = Motn::new(Some("unused".into()), Some(&dir));
        assert_eq!(lock(&restarted.kept).wanted["netflix@US"], 5678);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pruning_the_last_expired_market_is_persisted() {
        let dir = temp("pruned");
        let now = 100 * DAY;
        let motn = Motn::new(Some("unused".into()), Some(&dir));
        lock(&motn.kept).wanted.insert("netflix@US".into(), now - WANTED_FOR);
        assert!(motn.save());
        assert!(
            motn.pass(now).await,
            "the pass removed persisted state even though nothing remained to fetch"
        );
        let restarted = Motn::new(Some("unused".into()), Some(&dir));
        assert!(lock(&restarted.kept).wanted.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_atomic_replacement_keeps_the_old_file_and_can_retry() {
        let dir = temp("atomic");
        let file = dir.join(FILE);
        let partial = file.with_extension("json.partial");
        let motn = Motn::new(Some("unused".into()), Some(&dir));
        lock(&motn.kept).wanted.insert("netflix@US".into(), 1);
        assert!(motn.save());
        let old = std::fs::read(&file).unwrap();

        lock(&motn.kept).wanted.insert("netflix@US".into(), 2);
        std::fs::create_dir(&partial).unwrap();
        assert!(!motn.save(), "a partial path that cannot be opened must fail closed");
        assert_eq!(std::fs::read(&file).unwrap(), old, "the published file remains whole and old");
        assert_eq!(lock(&Motn::new(None, Some(&dir)).kept).wanted["netflix@US"], 1);

        std::fs::remove_dir(&partial).unwrap();
        assert!(motn.save(), "the unchanged dirty snapshot is retried after the filesystem recovers");
        assert_eq!(lock(&Motn::new(None, Some(&dir)).kept).wanted["netflix@US"], 2);
        assert!(!partial.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Manual release-mode measurement for the only request-visible critical section in persistence.
    #[test]
    #[ignore = "microbenchmark: cargo test --release motn_snapshot_cost -- --ignored --nocapture"]
    fn motn_snapshot_cost() {
        let motn = Motn::new(None, None);
        let mut kept = Kept::default();
        for market_no in 0..100 {
            let market = format!("service{market_no}@US");
            let shows: Vec<Show> = (0..60)
                .map(|title_no| at(&format!("Title {market_no}-{title_no}"), title_no, title_no as i64))
                .collect();
            kept.top.insert(market.clone(), (1234, shows[..10].to_vec()));
            kept.added.insert(market.clone(), shows.clone());
            kept.leaving.insert(market.clone(), shows[..20].to_vec());
            kept.coming.insert(market, shows[..20].to_vec());
        }
        *lock(&motn.kept) = kept;
        let mut clone_time = Duration::ZERO;
        let mut serialize_time = Duration::ZERO;
        let mut bytes = 0;
        for _ in 0..100 {
            let started = std::time::Instant::now();
            let snapshot = lock(&motn.kept).clone();
            clone_time += started.elapsed();
            let started = std::time::Instant::now();
            let body = serde_json::to_vec(&snapshot).unwrap();
            serialize_time += started.elapsed();
            bytes = std::hint::black_box(body.len());
        }
        eprintln!(
            "motn snapshot: {bytes} bytes, clone-under-lock {:?}/pass, serialize {:?}/pass",
            clone_time / 100,
            serialize_time / 100
        );
    }
}
