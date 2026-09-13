//! The dataset's Wikidata facts (`factsSlimFile`, else `factsFile`): when a title came out, its genres, who
//! made it and who is in it, where and in what language — keyed by type and TMDB id, for `/recommend`. The
//! facts are CC0, which is what lets atlas rank on them while holding no TMDB data of its own.
//!
//! Wikidata is open-world: a field left out means unknown, and so does an empty list here. Nothing reading
//! these may treat "not known" as "none".

use den_index::MediaType;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// The file layout this reader understands (`"schema"` in the file).
const SCHEMA: u32 = 1;

/// When a title came out, as its first day (days since 1970-01-01) and how many days the statement covers:
/// one for a day, the whole month or year for a date known only that far. A year-only date is a year, not
/// the first of July.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    pub first_day: i64,
    pub span_days: i64,
}

impl Released {
    /// `YYYY-MM-DD` at `day`, `month` or `year` precision; the parts past the precision are ignored.
    pub fn parse(date: &str, precision: &str) -> Option<Released> {
        let mut parts = date.splitn(3, '-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        let day: u32 = parts.next().and_then(|d| d.get(..2)).and_then(|d| d.parse().ok()).unwrap_or(0);
        match precision {
            "day" if (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day) => {
                Some(Released { first_day: days_from_civil(year, month, day), span_days: 1 })
            }
            "month" if (1..=12).contains(&month) => Some(Released {
                first_day: days_from_civil(year, month, 1),
                span_days: i64::from(days_in_month(year, month)),
            }),
            "year" => Some(Released {
                first_day: days_from_civil(year, 1, 1),
                span_days: days_from_civil(year + 1, 1, 1) - days_from_civil(year, 1, 1),
            }),
            _ => None,
        }
    }

    /// A title known only by its year.
    pub fn year(year: i64) -> Released {
        let first_day = days_from_civil(year, 1, 1);
        Released { first_day, span_days: days_from_civil(year + 1, 1, 1) - first_day }
    }

    /// The calendar year it starts in.
    pub fn year_of(&self) -> i64 {
        civil_year(self.first_day)
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_year(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// One title's facts, ready to rank on. Entities are Wikidata Q-ids as numbers (`Q42` → 42).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    pub imdb_id: Option<String>,
    pub released: Option<Released>,
    /// TMDB genre ids through the file's `genreMap`, series' genres named as films' (see `recommend::fold_genre`).
    pub genres: Vec<u16>,
    /// Where it was made: its production companies' countries, else its country of origin (ISO 3166-1).
    pub countries: Vec<[u8; 2]>,
    /// ISO 639-1, every original language Wikidata gives.
    pub languages: Vec<[u8; 2]>,
    /// Its directors and creators.
    pub makers: Vec<u32>,
    /// Its cast, unordered: Wikidata rarely says who is billed first.
    pub cast: Vec<u32>,
    /// The series of works it belongs to (P179).
    pub franchise: Option<u32>,
}

pub struct Facts {
    records: HashMap<(MediaType, u32), Record>,
}

impl Facts {
    pub fn get(&self, tmdb_id: u32, media_type: MediaType) -> Option<&Record> {
        self.records.get(&(media_type, tmdb_id))
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Read a facts file, plain or gzipped.
    pub fn read(path: &Path) -> Result<Facts, String> {
        let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Facts::from_bytes(&raw).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn from_bytes(raw: &[u8]) -> Result<Facts, String> {
        let mut plain = Vec::new();
        let json = if raw.starts_with(&[0x1f, 0x8b]) {
            flate2::read::GzDecoder::new(raw).read_to_end(&mut plain).map_err(|e| format!("gunzip: {e}"))?;
            plain.as_slice()
        } else {
            raw
        };
        let file: RawFile = serde_json::from_slice(json).map_err(|e| format!("parse: {e}"))?;
        if file.schema != SCHEMA {
            return Err(format!("schema {} (this atlas reads {SCHEMA})", file.schema));
        }
        let genre_map: HashMap<u32, u16> = file
            .genre_map
            .iter()
            .filter_map(|(q, genre)| Some((qid(q)?, genre.movie.or(genre.tv)?)))
            .collect();
        let mut records = HashMap::with_capacity(file.records.len());
        for raw in file.records {
            let media_type = match raw.media_type.as_str() {
                "movie" => MediaType::Movie,
                "tv" => MediaType::Tv,
                _ => continue,
            };
            let entities = |ids: Option<Vec<String>>| -> Vec<u32> {
                let mut out: Vec<u32> = Vec::new();
                for id in ids.unwrap_or_default().iter().filter_map(|q| qid(q)) {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                out
            };
            let mut genres: Vec<u16> = Vec::new();
            for q in entities(raw.genres) {
                for &genre in genre_map.get(&q).map_or(&[][..], |&g| crate::recommend::fold_genre(g)) {
                    if !genres.contains(&genre) {
                        genres.push(genre);
                    }
                }
            }
            let mut makers = entities(raw.directors);
            for id in entities(raw.creators) {
                if !makers.contains(&id) {
                    makers.push(id);
                }
            }
            let production = codes(raw.production_countries, u8::to_ascii_uppercase);
            let record = Record {
                imdb_id: raw.imdb_id.and_then(OneOrMany::first).filter(|id| id.starts_with("tt")),
                released: raw.released.and_then(|r| Released::parse(&r.date, &r.precision)),
                genres,
                countries: if production.is_empty() {
                    codes(raw.countries, u8::to_ascii_uppercase)
                } else {
                    production
                },
                languages: codes(raw.languages, u8::to_ascii_lowercase),
                makers,
                cast: entities(raw.cast),
                franchise: raw.franchise.and_then(OneOrMany::first).as_deref().and_then(qid),
            };
            // The first record wins a duplicate, as in the labels index.
            records.entry((media_type, raw.tmdb_id)).or_insert(record);
        }
        Ok(Facts { records })
    }
}

/// `Q42` → 42.
fn qid(id: &str) -> Option<u32> {
    id.strip_prefix('Q')?.parse().ok()
}

/// Two-letter codes, cased by `case`; anything else dropped.
fn codes(values: Option<Vec<String>>, case: fn(&u8) -> u8) -> Vec<[u8; 2]> {
    let mut out: Vec<[u8; 2]> = Vec::new();
    for value in values.unwrap_or_default() {
        if let [a, b] = value.as_bytes() {
            let code = [case(a), case(b)];
            if code.iter().all(u8::is_ascii_alphabetic) && !out.contains(&code) {
                out.push(code);
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct RawFile {
    schema: u32,
    #[serde(default, rename = "genreMap")]
    genre_map: HashMap<String, RawGenre>,
    #[serde(default)]
    records: Vec<RawRecord>,
}

#[derive(Deserialize)]
struct RawGenre {
    movie: Option<u16>,
    tv: Option<u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord {
    media_type: String,
    tmdb_id: u32,
    imdb_id: Option<OneOrMany>,
    released: Option<RawReleased>,
    genres: Option<Vec<String>>,
    countries: Option<Vec<String>>,
    production_countries: Option<Vec<String>>,
    languages: Option<Vec<String>>,
    directors: Option<Vec<String>>,
    creators: Option<Vec<String>>,
    cast: Option<Vec<String>>,
    franchise: Option<OneOrMany>,
}

/// A statement Wikidata may make once or several times — an IMDb id, a franchise — written as a string or a
/// list of them. The first is read.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn first(self) -> Option<String> {
        match self {
            OneOrMany::One(value) => Some(value),
            OneOrMany::Many(values) => values.into_iter().next(),
        }
    }
}

#[derive(Deserialize)]
struct RawReleased {
    date: String,
    precision: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small facts file in the published layout. The ids are made up for the test.
    pub(crate) const SAMPLE: &str = r#"{
      "schema": 1,
      "datasetVersion": "v1",
      "entities": {"Q1": {"en": "A Director"}},
      "genreMap": {"Q100": {"movie": 80, "tv": 80}, "Q101": {"movie": 18, "tv": 18}, "Q102": {"tv": 10765}},
      "records": [
        {"mediaType": "movie", "tmdbId": 1, "imdbId": ["tt0000001", "tt9999999"],
         "released": {"date": "2026-09-01", "precision": "day"},
         "genres": ["Q100", "Q101", "Q999"], "directors": ["Q1"], "cast": ["Q2", "Q3", "Q2"],
         "productionCountries": ["se", "DK"], "countries": ["US"], "languages": ["SV"], "franchise": ["Q50"]},
        {"mediaType": "tv", "tmdbId": 1, "released": {"date": "2010-00-00", "precision": "year"},
         "genres": ["Q102"], "creators": ["Q7"], "countries": ["KR"], "hasVector": false},
        {"mediaType": "movie", "tmdbId": 2}
      ]
    }"#;

    #[test]
    fn reads_records_by_type_and_id_with_their_genres_mapped() {
        let facts = Facts::from_bytes(SAMPLE.as_bytes()).unwrap();
        assert_eq!(facts.len(), 3);
        let film = facts.get(1, MediaType::Movie).unwrap();
        assert_eq!(film.imdb_id.as_deref(), Some("tt0000001"));
        // An unmapped genre is simply not known; the map's own order is kept.
        assert_eq!(film.genres, vec![80, 18]);
        assert_eq!(film.makers, vec![1]);
        assert_eq!(film.cast, vec![2, 3], "a repeated statement counts once");
        assert_eq!(film.countries, vec![*b"SE", *b"DK"], "production countries win over origin");
        assert_eq!(film.languages, vec![*b"sv"]);
        assert_eq!(film.franchise, Some(50));
        assert_eq!(film.released, Some(Released { first_day: days_from_civil(2026, 9, 1), span_days: 1 }));

        // Movie 1 and series 1 are different titles.
        let series = facts.get(1, MediaType::Tv).unwrap();
        assert_eq!(series.genres, vec![878, 14], "a series-only genre is named as films'");
        assert_eq!(series.makers, vec![7]);
        assert_eq!(series.countries, vec![*b"KR"]);
        assert_eq!(series.released.unwrap().span_days, 365);

        // Nothing known is nothing known, not an error.
        assert_eq!(facts.get(2, MediaType::Movie), Some(&Record::default()));
    }

    #[test]
    fn reads_the_gzipped_file_too_and_refuses_another_schema() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(SAMPLE.as_bytes()).unwrap();
        assert_eq!(Facts::from_bytes(&gz.finish().unwrap()).unwrap().len(), 3);
        let next = SAMPLE.replacen(r#""schema": 1"#, r#""schema": 2"#, 1);
        assert!(Facts::from_bytes(next.as_bytes()).err().unwrap().contains("schema 2"));
    }

    #[test]
    fn dates_keep_their_precision() {
        assert_eq!(Released::parse("1999-03-31", "day").unwrap().first_day, 10_681);
        let month = Released::parse("2024-02-00", "month").unwrap();
        assert_eq!((month.first_day, month.span_days), (days_from_civil(2024, 2, 1), 29));
        let year = Released::parse("1999", "year").unwrap();
        assert_eq!(
            (year.first_day, year.span_days, year.year_of()),
            (days_from_civil(1999, 1, 1), 365, 1999)
        );
        assert_eq!(Released::parse("1999-02-30", "day"), None);
        assert_eq!(Released::parse("1999-03-31", "century"), None);
        assert_eq!(civil_year(days_from_civil(2000, 12, 31)), 2000);
        assert_eq!(civil_year(days_from_civil(1969, 1, 1)), 1969);
    }
}
