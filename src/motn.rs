//! Movie of the Night's Streaming Availability API (`MOTN_KEY`): each service's real daily Top 10, and the titles
//! added to it, per country. JustWatch's "Popular on <service>" ranks by what people click on JustWatch and mixes old
//! catalogue in, so it matched nothing a service shows its own viewers; these lists lead those rows, and a billboard
//! reads a Top 10 place as attention.
//!
//! The free plan allows 1,000 requests a month, so nothing here is asked on a request's behalf. A request only says
//! which service in which country a household wants (`Motn::want`). A background pass (`refresh_forever`) fetches
//! those at most once a day, within `DAILY_REQUESTS`, and keeps the answers in `CACHE_DIR`; rows and billboards read
//! what is kept. A market nobody has asked for in a month is no longer fetched.

use crate::catalog::Provider;
use crate::justwatch::TrendingItem;
use crate::util::lock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
/// How far back the first look at a country's additions goes.
const FIRST_LOOK_BACK: i64 = 7 * DAY;
/// How long an addition stays on its "New on" list.
const KEEP_ADDED: i64 = 30 * DAY;
/// Pages of additions one pass reads for a country, 25 changes each.
const ADDED_PAGES: u32 = 4;
/// A market nobody has asked for in this long is no longer fetched.
const WANTED_FOR: i64 = 30 * DAY;
/// How often the services each country offers are read again.
const SERVICES_AFTER: i64 = 7 * DAY;
/// How often a pass looks for what is due. A pass with nothing due asks nothing, so a market a household has just
/// asked for is fetched within minutes rather than the hour.
const PASS_EVERY: Duration = Duration::from_secs(600);
/// The services the API has a Top 10 for.
const CHARTED: &[&str] = &["netflix", "prime", "disney", "apple", "hbo", "hulu"];
/// Countries the API doesn't cover, read as a neighbour whose catalogs are nearly the same: Argentina's Netflix Top 10
/// matched Uruguay's official one title for title.
const STAND_INS: &[(&str, &str)] = &[("UY", "AR")];
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
    /// When it was added to the service, for an addition.
    pub added: Option<i64>,
}

/// What the passes have fetched. A market is `service@COUNTRY`, the country after `STAND_INS`.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    /// Each market's Top 10, and when it was fetched.
    top: BTreeMap<String, (i64, Vec<Show>)>,
    /// Each market's additions, newest first.
    added: BTreeMap<String, Vec<Show>>,
    /// Each country's additions are read up to this moment.
    added_until: BTreeMap<String, i64>,
    /// The services each country offers, by the API's lowercase country code, and when they were read.
    services: BTreeMap<String, Vec<String>>,
    services_at: i64,
    /// When each market was last asked for.
    wanted: BTreeMap<String, i64>,
    /// The day requests are counted for, and how many were spent on it.
    spent: (i64, u32),
}

pub struct Motn {
    key: Option<String>,
    http: reqwest::Client,
    kept: Mutex<Kept>,
    file: Option<PathBuf>,
}

impl Motn {
    /// With no key it fetches nothing and every list is `None`, so rows and billboards read JustWatch alone.
    pub fn new(key: Option<String>, dir: Option<&Path>) -> Motn {
        let file = dir.map(|dir| dir.join(FILE));
        let kept = file
            .as_ref()
            .and_then(|file| match std::fs::read(file) {
                Ok(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|e| eprintln!("motn: {} is unreadable ({e}) — starting empty", file.display()))
                    .ok(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    eprintln!("motn: could not read {} ({e}) — starting empty", file.display());
                    None
                }
            })
            .unwrap_or_default();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default();
        Motn { key: key.filter(|k| !k.is_empty()), http, kept: Mutex::new(kept), file }
    }

    pub fn enabled(&self) -> bool {
        self.key.is_some()
    }

    /// Note that a household wants `provider` in `country`, so the next pass fetches its lists.
    pub fn want(&self, provider: &Provider, country: &str) {
        if let (true, Some(market)) = (self.enabled(), market(provider, country)) {
            lock(&self.kept).wanted.insert(market, now());
        }
    }

    /// The service's Top 10 in the country, best first; `None` when none is kept, or the one kept is too old.
    pub fn top(&self, provider: &Provider, country: &str) -> Option<Vec<Show>> {
        let market = market(provider, country)?;
        let kept = lock(&self.kept);
        let (at, shows) = kept.top.get(&market)?;
        (now() - at < SERVE_FOR).then(|| shows.clone())
    }

    /// What was added to the service in the country, newest first; `None` when the country's additions aren't kept or
    /// are too old. An empty list is an answer: nothing was added.
    pub fn added(&self, provider: &Provider, country: &str) -> Option<Vec<Show>> {
        let market = market(provider, country)?;
        let kept = lock(&self.kept);
        let until = kept.added_until.get(market_country(&market))?;
        (now() - until < SERVE_FOR).then(|| kept.added.get(&market).cloned().unwrap_or_default())
    }

    pub async fn refresh_forever(self: Arc<Self>) {
        loop {
            self.pass(now()).await;
            tokio::time::sleep(PASS_EVERY).await;
        }
    }

    /// Fetch what is wanted and due, within the day's budget, and keep it.
    async fn pass(&self, now: i64) {
        if !self.enabled() {
            return;
        }
        let (wanted, services_due) = {
            let mut kept = lock(&self.kept);
            kept.wanted.retain(|_, at| now - *at < WANTED_FOR);
            (kept.wanted.keys().cloned().collect::<Vec<_>>(), now - kept.services_at >= SERVICES_AFTER)
        };
        if wanted.is_empty() {
            return;
        }
        if services_due {
            if let Some(countries) = self.ask("/countries?output_language=en", now).await {
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

        for market in wanted.iter().filter(|m| CHARTED.contains(&market_service(m))) {
            if lock(&self.kept).top.get(market).is_some_and(|(at, _)| now - at < REFRESH_AFTER) {
                continue;
            }
            let path = format!(
                "/shows/top?country={}&service={}",
                market_country(market).to_ascii_lowercase(),
                market_service(market)
            );
            let Some(answer) = self.ask(&path, now).await else { continue };
            let shows = answer
                .as_array()
                .map(|a| a.iter().filter_map(|s| show(s, None)).collect())
                .unwrap_or_default();
            lock(&self.kept).top.insert(market.clone(), (now, shows));
        }

        let countries: BTreeSet<&str> = wanted.iter().map(|m| market_country(m)).collect();
        for country in countries {
            let since =
                lock(&self.kept).added_until.get(country).copied().unwrap_or(0).max(now - FIRST_LOOK_BACK);
            if now - since < REFRESH_AFTER {
                continue;
            }
            let catalogs: Vec<&str> =
                wanted.iter().filter(|m| market_country(m) == country).map(|m| market_service(m)).collect();
            let mut read: Vec<(String, Show)> = Vec::new();
            let mut cursor: Option<String> = None;
            let mut answered = false;
            for _ in 0..ADDED_PAGES {
                let path = format!(
                    "/changes?country={}&change_type=new&item_type=show&catalogs={}&from={since}&to={now}{}",
                    country.to_ascii_lowercase(),
                    catalogs.join(","),
                    cursor.as_deref().map(|c| format!("&cursor={c}")).unwrap_or_default()
                );
                let Some(page) = self.ask(&path, now).await else { break };
                answered = true;
                read.extend(additions(&page, country));
                if page["hasMore"].as_bool() != Some(true) {
                    break;
                }
                cursor = page["nextCursor"].as_str().map(str::to_owned);
            }
            if answered {
                keep_added(&mut lock(&self.kept), country, read, now);
            }
        }
        self.save();
    }

    /// One request, if the day's budget allows it; `None` on any failure, which is logged.
    async fn ask(&self, path: &str, now: i64) -> Option<serde_json::Value> {
        let key = self.key.as_deref()?;
        if !spend(&mut lock(&self.kept), now) {
            eprintln!("motn: the day's {DAILY_REQUESTS} requests are spent; {path} waits for tomorrow");
            return None;
        }
        let answer = self.http.get(format!("{BASE}{path}")).header("X-API-Key", key).send().await;
        let response = match answer {
            Ok(response) => response,
            Err(e) => {
                eprintln!("motn: {path}: {e}");
                return None;
            }
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            eprintln!("motn: {path}: HTTP {status}: {}", body.chars().take(200).collect::<String>());
            return None;
        }
        serde_json::from_str(&body).map_err(|e| eprintln!("motn: {path}: unreadable answer ({e})")).ok()
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        let bytes = match serde_json::to_vec(&*lock(&self.kept)) {
            Ok(bytes) => bytes,
            Err(e) => return eprintln!("motn: could not serialise what was fetched ({e})"),
        };
        let written =
            file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(file, bytes));
        if let Err(e) = written {
            eprintln!("motn: could not keep what was fetched in {} ({e})", file.display());
        }
    }
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
            rating: s.rating,
            year: s.year,
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
fn show(value: &serde_json::Value, added: Option<i64>) -> Option<Show> {
    let (kind, id) = value["tmdbId"].as_str()?.split_once('/')?;
    Some(Show {
        series: kind == "tv",
        tmdb: id.parse().ok()?,
        imdb: value["imdbId"].as_str().filter(|id| id.starts_with("tt")).map(str::to_owned),
        title: value["title"].as_str()?.to_owned(),
        year: value["releaseYear"].as_i64().or_else(|| value["firstAirYear"].as_i64()),
        rating: value["rating"].as_f64().filter(|r| *r > 0.0).map(|r| r / 10.0),
        added,
    })
}

/// A `/changes` page's additions to a subscription, by market. A title to rent or buy isn't on a service.
fn additions(page: &serde_json::Value, country: &str) -> Vec<(String, Show)> {
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

/// Fold a country's newly read additions into what is kept: each title once per market, newest first, and nothing
/// older than `KEEP_ADDED`.
fn keep_added(kept: &mut Kept, country: &str, read: Vec<(String, Show)>, now: i64) {
    for (market, show) in read {
        let list = kept.added.entry(market).or_default();
        if !list.iter().any(|held| held.series == show.series && held.tmdb == show.tmdb) {
            list.push(show);
        }
    }
    for (market, list) in kept.added.iter_mut() {
        if market_country(market) == country {
            list.retain(|s| s.added.is_none_or(|at| now - at < KEEP_ADDED));
            list.sort_by_key(|s| std::cmp::Reverse(s.added.unwrap_or(0)));
        }
    }
    kept.added_until.insert(country.to_owned(), now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(motn: &'static str) -> Provider {
        Provider { code: "x", id: "jw-x", name: "Popular on X", package_ids: &[], motn }
    }

    fn at(title: &str, tmdb: u32, added: i64) -> Show {
        Show {
            series: false,
            tmdb,
            imdb: None,
            title: title.into(),
            year: None,
            rating: None,
            added: Some(added),
        }
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
                added: None,
            })
        );
        let unrated = serde_json::json!({"tmdbId": "movie/860508", "title": "The Whisper Man", "rating": 0});
        assert_eq!(
            show(&unrated, Some(5)).map(|s| (s.series, s.rating, s.added)),
            Some((false, None, Some(5)))
        );
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
        let read = additions(&page, "AR");
        assert_eq!(read.len(), 1, "a rental and a change without its show are left out");
        assert_eq!((read[0].0.as_str(), read[0].1.tmdb, read[0].1.added), ("netflix@AR", 752, Some(100)));
    }

    #[test]
    fn additions_are_kept_once_newest_first_for_a_month() {
        let mut kept = Kept::default();
        let now = 100 * DAY;
        keep_added(&mut kept, "FI", vec![("hbo@FI".into(), at("Old", 1, now - 40 * DAY))], now - 10 * DAY);
        keep_added(
            &mut kept,
            "FI",
            vec![("hbo@FI".into(), at("Newer", 2, now - DAY)), ("hbo@FI".into(), at("Newer again", 2, now))],
            now,
        );
        let titles: Vec<&str> = kept.added["hbo@FI"].iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Newer"], "a repeat is dropped and an addition past a month ages out");
        assert_eq!(kept.added_until["FI"], now);
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
        let film = Show { imdb: Some("tt11561116".into()), ..at("The Whisper Man", 860_508, 0) };
        let items = trending(&[series.clone(), film, at("No imdb", 5, 0)], false);
        assert_eq!(
            items.iter().map(|i| (i.imdb.as_str(), i.rank)).collect::<Vec<_>>(),
            vec![("tt11561116", 0)]
        );
        assert_eq!(trending(&[series], true)[0].moviedb, Some(322_428));
    }

    #[test]
    fn without_a_key_nothing_is_wanted_or_kept() {
        let motn = Motn::new(None, None);
        motn.want(&provider("netflix"), "US");
        assert!(lock(&motn.kept).wanted.is_empty());
        assert_eq!(motn.top(&provider("netflix"), "US"), None);
    }
}
