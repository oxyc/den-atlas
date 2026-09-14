//! Fuzzy title search (the `den-titlesearch` crate), served as a Stremio search catalog. The index is built
//! in memory from TMDB's daily ID exports and rebuilt once a day; nothing is written to disk, so it needs no
//! writable mount. Its size is logged at every build.

use crate::util::lock;
use den_titlesearch::{ExportScanner, MediaType, TitleIndex};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CATALOG_ID: &str = "den-titles";
pub const CATALOG_NAME: &str = "Titles";
/// TMDB's daily ID exports: public, unauthenticated, published around 07:00 UTC.
pub const EXPORT_BASE: &str = "https://files.tmdb.org/p/exports/";
/// The most popular titles kept, movies and series together — the tvOS app's own cut.
const TOP_N: usize = 100_000;
/// Results per search — the tvOS app's own limit.
const RESULT_LIMIT: usize = 30;
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 3600);
const RETRY_AFTER: Duration = Duration::from_secs(3600);

pub struct TitleSearch {
    index: RwLock<Option<Arc<TitleIndex>>>,
    client: reqwest::Client,
    base: String,
    /// Exports that downloaded, by URL, while a build waits on the other one. A retry an hour later asks only
    /// for the file that failed rather than both again; cleared once a build lands.
    downloaded: Mutex<HashMap<String, bytes::Bytes>>,
}

impl TitleSearch {
    /// The two exports come to ~33 MB, hence the generous timeout.
    pub fn new(base: &str) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(180)).build()?;
        Ok(TitleSearch {
            index: RwLock::new(None),
            client,
            base: base.to_owned(),
            downloaded: Mutex::default(),
        })
    }

    #[cfg(test)]
    pub fn with_index(index: TitleIndex) -> Self {
        TitleSearch {
            index: RwLock::new(Some(Arc::new(index))),
            client: reqwest::Client::new(),
            base: String::new(),
            downloaded: Mutex::default(),
        }
    }

    /// The current index; `None` until the first build lands.
    pub fn index(&self) -> Option<Arc<TitleIndex>> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Rebuild from today's exports — yesterday's if today's aren't published yet — and swap the result
    /// in; the previous index serves until then. Returns a line for the log.
    pub async fn refresh(&self) -> Result<String, String> {
        let started = Instant::now();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let stamps: Vec<String> =
            (0..2).map(|days_back| utc_stamp(now.saturating_sub(days_back * 86_400))).collect();
        // A download from a day no longer tried is of no use to this build.
        lock(&self.downloaded).retain(|url, _| stamps.iter().any(|stamp| url.contains(stamp.as_str())));
        let mut failure = String::new();
        for stamp in stamps {
            let (movies, series) = match self.fetch_pair(&stamp).await {
                Ok(pair) => pair,
                Err(e) => {
                    failure = e;
                    continue;
                }
            };
            let index = tokio::task::spawn_blocking(move || build(&movies, &series))
                .await
                .map_err(|e| format!("build task: {e}"))??;
            let line = format!(
                "title index: {} titles from the {stamp} exports, ~{} MB, in {:.1}s",
                index.len(),
                index.approx_bytes() / 1_000_000,
                started.elapsed().as_secs_f64()
            );
            *self.index.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(index));
            lock(&self.downloaded).clear();
            return Ok(line);
        }
        Err(failure)
    }

    /// Both files are asked for even when the first fails, so the one that downloads is kept for the retry.
    async fn fetch_pair(&self, stamp: &str) -> Result<(bytes::Bytes, bytes::Bytes), String> {
        let movies = self.fetch_kept(&format!("{}movie_ids_{stamp}.json.gz", self.base)).await;
        let series = self.fetch_kept(&format!("{}tv_series_ids_{stamp}.json.gz", self.base)).await;
        Ok((movies?, series?))
    }

    async fn fetch_kept(&self, url: &str) -> Result<bytes::Bytes, String> {
        if let Some(kept) = lock(&self.downloaded).get(url) {
            return Ok(kept.clone());
        }
        let body = self.fetch(url).await?;
        lock(&self.downloaded).insert(url.to_owned(), body.clone());
        Ok(body)
    }

    async fn fetch(&self, url: &str) -> Result<bytes::Bytes, String> {
        let resp = self.client.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url}: HTTP {}", resp.status()));
        }
        resp.bytes().await.map_err(|e| format!("{url}: {e}"))
    }
}

/// Build now, then daily. A failed build retries hourly and the previous index keeps serving.
pub async fn refresh_forever(search: Arc<TitleSearch>) {
    loop {
        let wait = match search.refresh().await {
            Ok(line) => {
                eprintln!("{line}");
                REFRESH_EVERY
            }
            Err(e) => {
                let serving = if search.index().is_some() {
                    "keeping the previous index"
                } else {
                    "title search answers empty until a build lands"
                };
                eprintln!("title index refresh failed ({e}); {serving}; retrying in an hour");
                RETRY_AFTER
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// Gunzip both exports straight into one scanner — the ~140 MB they inflate to is never held — and index
/// the most popular titles.
fn build(movies: &[u8], series: &[u8]) -> Result<TitleIndex, String> {
    let mut scanner =
        inflate_into(ExportScanner::new(TOP_N), movies).map_err(|e| format!("movie export: {e}"))?;
    scanner.start(MediaType::Tv);
    let scanner = inflate_into(scanner, series).map_err(|e| format!("series export: {e}"))?;
    let records = scanner.finish();
    if records.is_empty() {
        return Err("the exports held no titles".to_owned());
    }
    Ok(TitleIndex::build(records))
}

fn inflate_into(scanner: ExportScanner, gz: &[u8]) -> std::io::Result<ExportScanner> {
    let mut decoder = flate2::write::GzDecoder::new(scanner);
    decoder.write_all(gz)?;
    decoder.finish()
}

/// A search catalog body of `{id,type,name,moviedb_id}` metas. The TMDB id is what the Den apps map a row
/// through, and they draw posters from TMDB themselves, so atlas serves none; the name is the export's
/// original title, at most a day old.
pub fn metas_json(index: &TitleIndex, query: &str, media_type: MediaType) -> String {
    let metas: Vec<serde_json::Value> = index
        .search(query, Some(media_type), RESULT_LIMIT)
        .iter()
        .map(|hit| {
            serde_json::json!({
                "id": format!("tmdb:{}", hit.tmdb_id),
                "type": media_type.stremio_type(),
                "name": hit.title,
                "moviedb_id": hit.tmdb_id,
            })
        })
        .collect();
    serde_json::json!({ "metas": metas }).to_string()
}

/// TMDB's export file date — `MM_DD_YYYY` in UTC — for a Unix time (Howard Hinnant's civil-from-days).
fn utc_stamp(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{month:02}_{day:02}_{year:04}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gz(text: &str) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn the_export_date_is_utc_month_day_year() {
        assert_eq!(utc_stamp(0), "01_01_1970");
        assert_eq!(utc_stamp(1_788_998_400), "09_10_2026");
        assert_eq!(utc_stamp(1_709_164_800), "02_29_2024", "a leap day");
        assert_eq!(utc_stamp(1_788_998_400 + 86_399), "09_10_2026", "the last second of the day");
    }

    #[test]
    fn builds_from_both_gzipped_exports_and_answers_by_type() {
        let movies = gz("{\"adult\":false,\"id\":603,\"original_title\":\"The Matrix\",\"popularity\":80}\n");
        let series = gz("{\"id\":1399,\"original_name\":\"Game of Thrones\",\"popularity\":300}\n");
        let index = build(&movies, &series).unwrap();
        assert_eq!(index.len(), 2);

        let body = metas_json(&index, "matrx", MediaType::Movie);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["metas"][0]["id"], "tmdb:603");
        assert_eq!(v["metas"][0]["type"], "movie");
        assert_eq!(v["metas"][0]["moviedb_id"], 603);
        assert_eq!(v["metas"][0]["name"], "The Matrix");
        assert_eq!(metas_json(&index, "matrix", MediaType::Tv), r#"{"metas":[]}"#);
    }

    /// The series export fails for every day tried, then answers. The retry downloads only the series file:
    /// the movie export that already arrived is not fetched again.
    #[tokio::test]
    async fn a_retry_downloads_only_the_export_that_failed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let movies = gz("{\"adult\":false,\"id\":603,\"original_title\":\"The Matrix\",\"popularity\":80}\n");
        let series = gz("{\"id\":1399,\"original_name\":\"Game of Thrones\",\"popularity\":300}\n");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let movie_asks = Arc::new(AtomicUsize::new(0));
        let series_asks = Arc::new(AtomicUsize::new(0));
        let (m, s) = (Arc::clone(&movie_asks), Arc::clone(&series_asks));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let (status, body) = if head.contains("movie_ids_") {
                    m.fetch_add(1, Ordering::SeqCst);
                    ("200 OK", movies.clone())
                } else if s.fetch_add(1, Ordering::SeqCst) < 2 {
                    ("500 Internal Server Error", Vec::new())
                } else {
                    ("200 OK", series.clone())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            }
        });
        let search = TitleSearch::new(&format!("http://{addr}/")).unwrap();

        assert!(search.refresh().await.is_err(), "both days' series exports failed");
        assert_eq!(movie_asks.load(Ordering::SeqCst), 2, "one movie export per day tried");
        search.refresh().await.expect("the retry builds");
        assert_eq!(
            movie_asks.load(Ordering::SeqCst),
            2,
            "the retry downloaded a movie export it already had"
        );
        assert_eq!(series_asks.load(Ordering::SeqCst), 3);
        assert_eq!(search.index().unwrap().len(), 2);
        assert!(lock(&search.downloaded).is_empty(), "the downloads outlived the build");
    }

    #[test]
    fn an_export_that_is_not_gzip_fails_the_build() {
        assert!(build(b"not gzip", b"").is_err());
    }
}
