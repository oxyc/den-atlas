//! IMDb's public `title.ratings` dump, joined onto the store's rows (oxyc/den#118).
//!
//! Browse row order is a vote count, and atlas's only source for one has been the store's `votes`
//! column — TMDB's count, written by the producer. That column is going away. IMDb publishes the same
//! fact daily, for free and unauthenticated, keyed on the `tt…` id the store already carries in its
//! `imdb` column: 1.7 M rows, 8 MB gzipped, of which 47,562 of our 47,618 name a row here.
//!
//! The dump is downloaded on the same 24 h loop as the title index (`titles::refresh_forever`) and for the
//! same reasons — a failed refresh keeps the previous index and retries in an hour — but it is joined
//! against the store on the way in, so what stays resident is one `u32` and one `f32` per STORE row
//! (~190 KB each) rather than the 1.7 M rows the file holds. Everything the dump says about a title the
//! corpus does not contain is discarded at parse time and never allocated.
//!
//! The store is re-opened for each refresh rather than held: the join needs the `imdb` column, and a
//! mapping kept between refreshes would pin the generation `atlas-dataset-sync` may since have replaced.
//! A refresh costs one mmap + hash check (~0.1 s) once a day.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// IMDb's daily dump: public, unauthenticated, refreshed every morning.
pub const RATINGS_URL: &str = "https://datasets.imdbws.com/title.ratings.tsv.gz";
/// The dump's header line. CHECKED rather than skipped: a file whose columns moved would otherwise be
/// read with `numVotes` in the rating's place, and the join would report a confident wrong answer instead
/// of failing.
const HEADER: &str = "tconst\taverageRating\tnumVotes";
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 3600);
const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// IMDb's vote count and score for the store rows the dump names, indexed BY STORE ROW so a lookup is an
/// array read beside the row lookup `votes_of` already does.
pub struct RatingsIndex {
    /// `numVotes` per store row; 0 where the dump named no rating for that row's IMDb id.
    votes: Vec<u32>,
    /// `averageRating` per store row, on IMDb's 1-10 scale; 0.0 where absent.
    ratings: Vec<f32>,
    /// How many rows the dump named. Never 0 — `join` refuses an index that matched nothing.
    matched: usize,
}

/// Its coverage, never its contents: two vectors the length of the corpus help nobody in a test failure —
/// the same reason `MappedStore` names the store rather than printing it.
impl std::fmt::Debug for RatingsIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RatingsIndex").field("matched", &self.matched).field("rows", &self.rows()).finish()
    }
}

impl RatingsIndex {
    /// IMDb's vote count for a store row, `None` where the dump named none.
    pub fn votes(&self, row: usize) -> Option<u32> {
        self.votes.get(row).copied().filter(|&votes| votes > 0)
    }

    /// IMDb's score for a store row, `None` where the dump named none.
    pub fn rating(&self, row: usize) -> Option<f32> {
        self.ratings.get(row).copied().filter(|&rating| rating > 0.0)
    }

    /// Both at once, for a caller that wants a rating only when a count stands behind it.
    pub fn of(&self, row: usize) -> Option<(u32, f32)> {
        Some((self.votes(row)?, self.rating(row)?))
    }

    /// How many store rows the dump named.
    pub fn matched(&self) -> usize {
        self.matched
    }

    /// The store rows this was built over — the denominator for `matched`.
    pub fn rows(&self) -> usize {
        self.votes.len()
    }
}

pub struct Ratings {
    index: RwLock<Option<Arc<RatingsIndex>>>,
    client: reqwest::Client,
    url: String,
    /// The store the dump is joined onto, by path: opened per refresh, never held.
    store: PathBuf,
}

impl Ratings {
    pub fn new(store: PathBuf, url: &str) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
        Ok(Ratings { index: RwLock::new(None), client, url: url.to_owned(), store })
    }

    #[cfg(test)]
    pub fn with_index(index: RatingsIndex) -> Self {
        Ratings {
            index: RwLock::new(Some(Arc::new(index))),
            client: reqwest::Client::new(),
            url: String::new(),
            store: PathBuf::new(),
        }
    }

    /// The current index; `None` until the first refresh lands, and after a refresh that failed with no
    /// previous index to keep.
    pub fn index(&self) -> Option<Arc<RatingsIndex>> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Download today's dump, join it onto the store and swap the result in; the previous index serves
    /// until then. Returns a line for the log.
    pub async fn refresh(&self) -> Result<String, String> {
        let started = Instant::now();
        let gz = self.fetch().await?;
        let downloaded = gz.len();
        let store = self.store.clone();
        let index = tokio::task::spawn_blocking(move || {
            let mapped = crate::store::MappedStore::open(&store)?;
            build(&mapped.view(), &gz)
        })
        .await
        .map_err(|e| format!("ratings join task: {e}"))??;
        let line = format!(
            "imdb ratings: {} of {} store rows, {:.1} MB downloaded, in {:.1}s",
            index.matched(),
            index.rows(),
            downloaded as f64 / 1_000_000.0,
            started.elapsed().as_secs_f64()
        );
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(index));
        Ok(line)
    }

    async fn fetch(&self) -> Result<bytes::Bytes, String> {
        let resp = self.client.get(&self.url).send().await.map_err(|e| format!("{}: {e}", self.url))?;
        if !resp.status().is_success() {
            return Err(format!("{}: HTTP {}", self.url, resp.status()));
        }
        resp.bytes().await.map_err(|e| format!("{}: {e}", self.url))
    }
}

/// How long the boot gate waits for the first dump before serving without one.
///
/// A bound, not a target: the join takes ~3 s on the box against an 8.2 MB file, so this is the point at
/// which "the network is not answering" beats "rows would be ordered by nothing", and starting degraded
/// and saying so beats not starting at all.
const FIRST_JOIN_WITHIN: Duration = Duration::from_secs(45);

/// The BOOT GATE: wait for the first join before the server answers anything.
///
/// It has to be a real wait rather than a log line. The store no longer carries `votes`
/// (oxyc/den#118), so between binding the listener and the first dump landing there is no vote count
/// from any source at all, and every browse row comes back in tmdb-id order — *La Job* beside *Game of
/// Thrones*, which reads as data rather than as an error.
///
/// The window is not rare and it is not only a startup detail: `atlas-dataset-sync` restarts this
/// process every time a new dataset lands, so without the gate every publish serves a few seconds of
/// id-ordered browse rows to whoever is looking.
///
/// It is NOT needed to survive `den-update`: `wait_probe` retries `probe_atlas` every 2 s for 120 s, so
/// a degraded `/health` during the join would clear itself well inside the deadline. 45 s sits inside
/// that budget with room to spare, which is the only thing the probe requires of this.
pub async fn wait_for_first_join(ratings: &Ratings) {
    match tokio::time::timeout(FIRST_JOIN_WITHIN, ratings.refresh()).await {
        Ok(Ok(line)) => eprintln!("{line}"),
        Ok(Err(e)) => eprintln!(
            "imdb ratings: first join failed ({e}) — serving with whatever the store holds; /health says \
             whether that is anything"
        ),
        Err(_) => eprintln!(
            "imdb ratings: no dump within {}s — serving without one; /health says what that costs",
            FIRST_JOIN_WITHIN.as_secs()
        ),
    }
}

/// Daily, after the boot gate above has taken the first one. A failed join retries hourly and the
/// previous index keeps serving.
pub async fn refresh_forever(ratings: Arc<Ratings>) {
    let mut wait = REFRESH_EVERY;
    loop {
        tokio::time::sleep(wait).await;
        wait = match ratings.refresh().await {
            Ok(line) => {
                eprintln!("{line}");
                REFRESH_EVERY
            }
            Err(e) => {
                let serving = if ratings.index().is_some() {
                    "keeping the previous index"
                } else {
                    "browse rows have no vote count from any source — see /health"
                };
                eprintln!("imdb ratings refresh failed ({e}); {serving}; retrying in an hour");
                RETRY_AFTER
            }
        };
    }
}

/// The gzipped dump joined onto a store: its `imdb` column names the rows.
pub(crate) fn build(view: &den_store::Store<'_>, gz: &[u8]) -> Result<RatingsIndex, String> {
    let (rows, row_count) = imdb_rows(view)?;
    join(&rows, row_count, std::io::BufReader::new(flate2::read::GzDecoder::new(gz)))
}

/// The store's IMDb ids as a lookup from numeric id to row, and the store's row count.
///
/// Two rows claiming one IMDb id would be a dataset fault, not something to reconcile here: the FIRST row
/// keeps the id, so the join is deterministic whatever order the rows are read in.
pub(crate) fn imdb_rows(view: &den_store::Store<'_>) -> Result<(HashMap<u32, u32>, usize), String> {
    let err = |e: den_store::StoreError| e.to_string();
    let imdb = view.per_row::<u32>("imdb").map_err(err)?;
    let strings = view.strings().map_err(err)?;
    let mut rows: HashMap<u32, u32> = HashMap::with_capacity(imdb.len());
    for (row, &id) in imdb.iter().enumerate() {
        let Some(numeric) = strings.get(id).and_then(tconst) else { continue };
        let Ok(row) = u32::try_from(row) else { continue };
        rows.entry(numeric).or_insert(row);
    }
    if rows.is_empty() {
        return Err(format!("the store's `imdb` column named no title across {} rows", imdb.len()));
    }
    Ok((rows, imdb.len()))
}

/// The numeric part of an IMDb TITLE id. A non-`tt` id names nothing in this dump — the corpus carries a
/// person (`nm…`) and an event (`ev…`) id in this column — so it reads as absent rather than being coerced
/// into whatever digits follow.
pub(crate) fn tconst(id: &str) -> Option<u32> {
    let digits = id.strip_prefix("tt")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One `title.ratings.tsv` stream against a store's ids. Everything the dump holds for a title the store
/// does not is dropped as it is read, so the 1.7 M-row file never becomes 1.7 M entries in memory.
fn join(rows: &HashMap<u32, u32>, row_count: usize, tsv: impl BufRead) -> Result<RatingsIndex, String> {
    let mut lines = tsv.lines();
    match lines.next() {
        Some(Ok(head)) if head.trim_end() == HEADER => {}
        Some(Ok(head)) => return Err(format!("unexpected header {head:?}, wanted {HEADER:?}")),
        Some(Err(e)) => return Err(format!("reading the dump: {e}")),
        None => return Err("the dump was empty".to_owned()),
    }
    let mut votes = vec![0u32; row_count];
    let mut ratings = vec![0f32; row_count];
    let mut matched = 0usize;
    for line in lines {
        let line = line.map_err(|e| format!("reading the dump: {e}"))?;
        let mut fields = line.split('\t');
        let Some(numeric) = fields.next().and_then(tconst) else { continue };
        let Some(&row) = rows.get(&numeric) else { continue };
        let Some(rating) = fields.next().and_then(|r| r.parse::<f32>().ok()) else { continue };
        let Some(count) = fields.next().and_then(|v| v.parse::<u32>().ok()) else { continue };
        if count == 0 || !rating.is_finite() || rating <= 0.0 {
            continue;
        }
        let row = row as usize;
        if votes[row] == 0 {
            matched += 1;
        }
        votes[row] = count;
        ratings[row] = rating;
    }
    // Loud, not empty. An index that names no row would order every browse row by nothing while every
    // request still answered 200 — the shape of failure this whole join exists to take a source away from.
    if matched == 0 {
        return Err(format!("the dump named none of the store's {row_count} rows"));
    }
    Ok(RatingsIndex { votes, ratings, matched })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// The store's twelve-title route fixture carries one `tt…` id (movie 1, `tt0000001`), which is enough
    /// to prove the join and the fallback; the ids below that it does not hold prove the discard.
    const DUMP: &str = "tconst\taverageRating\tnumVotes\n\
        tt0000001\t8.4\t9000\n\
        tt0111161\t9.3\t2900000\n\
        nm0000001\t5.0\t10\n";

    fn rows() -> HashMap<u32, u32> {
        HashMap::from([(1, 0), (7, 2)])
    }

    #[test]
    fn names_the_rows_the_dump_holds_and_discards_the_rest() {
        let index = join(&rows(), 4, Cursor::new(DUMP)).expect("the dump names row 0");
        assert_eq!(index.matched(), 1, "one of the two known ids is in the dump");
        assert_eq!(index.rows(), 4);
        assert_eq!(index.of(0), Some((9000, 8.4)));
        assert_eq!(index.votes(2), None, "a known id the dump does not name stays absent");
        assert_eq!(index.rating(2), None);
        assert_eq!(index.votes(3), None, "a row with no IMDb id at all");
    }

    /// `tt0111161` is in the dump and not in the store: 1.7 M such rows are why the join filters as it
    /// reads rather than building a map of the file.
    #[test]
    fn a_title_the_store_does_not_hold_is_never_allocated() {
        let index = join(&rows(), 4, Cursor::new(DUMP)).unwrap();
        assert_eq!(index.votes.len(), 4, "one slot per STORE row, not per dump row");
        assert!(!index.votes.contains(&2_900_000));
    }

    #[test]
    fn a_non_title_id_names_no_row() {
        assert_eq!(tconst("tt0000001"), Some(1));
        assert_eq!(tconst("tt46327618"), Some(46_327_618), "the dump's largest id fits a u32");
        assert_eq!(tconst("nm19818812"), None, "a person id");
        assert_eq!(tconst("ev0000003"), None, "an event id");
        assert_eq!(tconst("tt"), None);
        assert_eq!(tconst("tt00x1"), None);
        assert_eq!(tconst(""), None);
    }

    /// A dump whose columns moved must FAIL, not be read with the count in the rating's place.
    #[test]
    fn a_changed_header_fails_the_join() {
        let moved = "tconst\tnumVotes\taverageRating\ntt0000001\t9000\t8.4\n";
        let e = join(&rows(), 4, Cursor::new(moved)).expect_err("a moved column must fail");
        assert!(e.contains("unexpected header"), "{e}");
        assert!(join(&rows(), 4, Cursor::new("")).is_err(), "an empty dump");
    }

    /// The silent-zero failure, at its source: a dump that names none of the corpus is an error, never an
    /// index of zeros that would sort every browse row by tmdb id with nothing logged.
    #[test]
    fn a_dump_that_names_nothing_is_an_error_not_an_index_of_zeros() {
        let none = "tconst\taverageRating\tnumVotes\ntt0111161\t9.3\t2900000\n";
        let e = join(&rows(), 4, Cursor::new(none)).expect_err("no match must fail");
        assert!(e.contains("named none of the store's 4 rows"), "{e}");
    }

    #[test]
    fn a_row_with_no_votes_or_no_score_is_absent_not_zero() {
        let thin = "tconst\taverageRating\tnumVotes\n\
            tt0000001\t8.4\t9000\n\
            tt0000007\t0\t0\n";
        let index = join(&rows(), 4, Cursor::new(thin)).unwrap();
        assert_eq!(index.matched(), 1);
        assert_eq!(index.of(2), None, "a zero-vote row is not a row with zero votes");
    }

    /// End to end over a real store: the gzipped dump, the store's own `imdb` column, one index.
    #[test]
    fn builds_from_the_gzipped_dump_against_a_real_store() {
        let dir = std::env::temp_dir().join(format!("den-atlas-ratings-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let mapped = crate::store::MappedStore::open(&ds.store).expect("the fixture store maps");

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(DUMP.as_bytes()).unwrap();
        let gz = encoder.finish().unwrap();

        let index = build(&mapped.view(), &gz).expect("the fixture's tt0000001 is in the dump");
        assert_eq!(index.rows(), 12, "one slot per fixture row");
        assert_eq!(index.matched(), 1);
        // Movie 1 is the fixture's only row with an IMDb id, and the store orders rows by packed key, so
        // it is row 0.
        assert_eq!(index.of(0), Some((9000, 8.4)));

        assert!(build(&mapped.view(), b"not gzip").is_err());
    }

    /// The boot gate RETURNS rather than hanging when no dump can be had.
    ///
    /// Serving late is a bug too: `atlas-dataset-sync` restarts this process on every publish, so a gate
    /// that waited forever on an unreachable host would take browse down instead of degrading it. The
    /// bound is what makes "wait for the join" safe to put in front of the server.
    #[tokio::test(start_paused = true)]
    async fn the_boot_gate_gives_up_rather_than_holding_the_server_down() {
        // A URL nothing answers: the join can only fail or time out, and either must return.
        let dir = std::env::temp_dir().join(format!("den-atlas-gate-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let ratings = Ratings::new(ds.store.clone(), "http://127.0.0.1:1/title.ratings.tsv.gz")
            .expect("a client builds");
        let started = tokio::time::Instant::now();
        wait_for_first_join(&ratings).await;
        assert!(started.elapsed() <= FIRST_JOIN_WITHIN, "the gate outlived its own bound");
        assert!(ratings.index().is_none(), "nothing landed, so nothing is served as if it had");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
