//! The facet index — each title's country, decade, type and vote count, from `facets.bin` — and the parser
//! that finds those facets in a search query ("spanish series", "80s korean horror"). Ports of the tvOS
//! app's `FacetIndex` and `FacetQuery`.

use crate::MediaType;
use std::collections::{HashMap, HashSet};

const MAGIC: &[u8] = b"DFI2";
/// One record: `[i32 tmdbId][u8 type (1 = tv)][2 lang][2 country][u16 year][u32 voteCount]`, little-endian.
const RECORD: usize = 15;

struct Row {
    tmdb_id: u32,
    media_type: MediaType,
    votes: u32,
    language: [u8; 2],
    country: [u8; 2],
    year: u16,
}

/// What the facet blob says about one title. Each field is `None` where the blob leaves it blank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TitleFacets {
    /// ISO 3166-1 alpha-2, uppercase.
    pub country: Option<[u8; 2]>,
    /// ISO 639-1, lowercase: the title's original language.
    pub language: Option<[u8; 2]>,
    pub year: Option<u16>,
    pub votes: u32,
}

pub struct FacetIndex {
    rows: Vec<Row>,
    /// (type, tmdb id) → row; the first row wins a duplicate, as in the labels index.
    by_title: HashMap<(MediaType, u32), u32>,
    by_country: HashMap<[u8; 2], Vec<u32>>,
    by_decade: HashMap<u16, Vec<u32>>,
    by_type: HashMap<MediaType, Vec<u32>>,
}

impl FacetIndex {
    /// `None` for a blob that isn't DFI2 or is shorter than its count says — facet search is then simply
    /// unavailable.
    pub fn from_blob(blob: &[u8]) -> Option<FacetIndex> {
        if blob.get(..4)? != MAGIC {
            return None;
        }
        let count = u32::from_le_bytes(blob.get(4..8)?.try_into().ok()?) as usize;
        let records = blob.get(8..8 + count.checked_mul(RECORD)?)?;
        let mut index = FacetIndex {
            rows: Vec::with_capacity(count),
            by_title: HashMap::with_capacity(count),
            by_country: HashMap::new(),
            by_decade: HashMap::new(),
            by_type: HashMap::new(),
        };
        for (position, r) in records.as_chunks::<RECORD>().0.iter().enumerate() {
            let position = position as u32;
            let media_type = if r[4] == 1 { MediaType::Tv } else { MediaType::Movie };
            let country = [r[7].to_ascii_uppercase(), r[8].to_ascii_uppercase()];
            let year = u16::from_le_bytes([r[9], r[10]]);
            if country != [0, 0] {
                index.by_country.entry(country).or_default().push(position);
            }
            if year >= 1870 {
                index.by_decade.entry(year / 10 * 10).or_default().push(position);
            }
            index.by_type.entry(media_type).or_default().push(position);
            let tmdb_id = i32::from_le_bytes([r[0], r[1], r[2], r[3]]) as u32;
            index.by_title.entry((media_type, tmdb_id)).or_insert(position);
            index.rows.push(Row {
                tmdb_id,
                media_type,
                votes: u32::from_le_bytes([r[11], r[12], r[13], r[14]]),
                language: [r[5].to_ascii_lowercase(), r[6].to_ascii_lowercase()],
                country,
                year,
            });
        }
        Some(index)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// One title's facets; `None` when the blob doesn't hold it. TMDB writes `xx` for "no language".
    pub fn title(&self, tmdb_id: u32, media_type: MediaType) -> Option<TitleFacets> {
        let row = &self.rows[*self.by_title.get(&(media_type, tmdb_id))? as usize];
        let letters = |code: [u8; 2]| code.iter().all(u8::is_ascii_alphabetic).then_some(code);
        Some(TitleFacets {
            country: letters(row.country),
            language: letters(row.language).filter(|code| code != b"xx"),
            year: (row.year >= 1870).then_some(row.year),
            votes: row.votes,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Titles matching every given constraint, most-voted first (ties in blob order). Empty when no constraint
    /// is given — the facet lane only fires on a real facet.
    pub fn filter(
        &self,
        media_type: Option<MediaType>,
        country: Option<&str>,
        decade: Option<u16>,
    ) -> Vec<(u32, MediaType)> {
        let empty: &[u32] = &[];
        let mut sets: Vec<&[u32]> = Vec::new();
        if let Some(country) = country {
            let key = country_key(country);
            sets.push(key.and_then(|k| self.by_country.get(&k)).map_or(empty, Vec::as_slice));
        }
        if let Some(decade) = decade {
            sets.push(self.by_decade.get(&decade).map_or(empty, Vec::as_slice));
        }
        if let Some(media_type) = media_type {
            sets.push(self.by_type.get(&media_type).map_or(empty, Vec::as_slice));
        }
        let Some((smallest, others)) = smallest_first(sets) else { return Vec::new() };
        let others: Vec<HashSet<u32>> = others.into_iter().map(|s| s.iter().copied().collect()).collect();
        let mut matched: Vec<u32> =
            smallest.iter().copied().filter(|p| others.iter().all(|s| s.contains(p))).collect();
        matched.sort_by_key(|&p| std::cmp::Reverse(self.rows[p as usize].votes));
        matched.iter().map(|&p| (self.rows[p as usize].tmdb_id, self.rows[p as usize].media_type)).collect()
    }
}

fn country_key(code: &str) -> Option<[u8; 2]> {
    let bytes = code.as_bytes();
    (bytes.len() == 2).then(|| [bytes[0].to_ascii_uppercase(), bytes[1].to_ascii_uppercase()])
}

fn smallest_first(mut sets: Vec<&[u32]>) -> Option<(&[u32], Vec<&[u32]>)> {
    let at = (0..sets.len()).min_by_key(|&i| sets[i].len())?;
    let smallest = sets.swap_remove(at);
    Some((smallest, sets))
}

/// A search query split into hard facets and the leftover words, which rank semantically.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FacetQuery {
    pub media_type: Option<MediaType>,
    /// ISO 3166-1 alpha-2.
    pub country: Option<&'static str>,
    /// The decade's first year, e.g. 1980.
    pub decade: Option<u16>,
    pub leftover: String,
}

impl FacetQuery {
    /// One pass over the words: the first country, type and decade each become a facet; everything else —
    /// themes, genres, stopwords — is leftover. Demonyms map to a country, so "spanish" means Spain, not the
    /// language.
    pub fn parse(text: &str) -> FacetQuery {
        let mut query = FacetQuery::default();
        let mut leftover: Vec<&str> = Vec::new();
        let lowered = text.to_lowercase();
        for token in lowered.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
            if query.country.is_none() {
                if let Some(country) = lookup(COUNTRIES, token) {
                    query.country = Some(country);
                    continue;
                }
            }
            if query.media_type.is_none() {
                if let Some(media_type) = lookup(MEDIA_TYPES, token) {
                    query.media_type = Some(media_type);
                    continue;
                }
            }
            if query.decade.is_none() {
                if let Some(decade) = lookup(DECADES, token) {
                    query.decade = Some(decade);
                    continue;
                }
            }
            leftover.push(token);
        }
        query.leftover = leftover.join(" ");
        query
    }

    pub fn has_facet(&self) -> bool {
        self.media_type.is_some() || self.country.is_some() || self.decade.is_some()
    }

    /// A facet ordinary search can't express — a country or a decade. A bare type ("batman movies") is not
    /// one: title and theme search already cover it.
    pub fn has_strong_facet(&self) -> bool {
        self.country.is_some() || self.decade.is_some()
    }
}

fn lookup<T: Copy>(table: &[(&str, T)], token: &str) -> Option<T> {
    table.iter().find(|(word, _)| *word == token).map(|&(_, value)| value)
}

const MEDIA_TYPES: &[(&str, MediaType)] = &[
    ("series", MediaType::Tv),
    ("show", MediaType::Tv),
    ("shows", MediaType::Tv),
    ("tv", MediaType::Tv),
    ("miniseries", MediaType::Tv),
    ("movie", MediaType::Movie),
    ("movies", MediaType::Movie),
    ("film", MediaType::Movie),
    ("films", MediaType::Movie),
];

const DECADES: &[(&str, u16)] = &[
    ("50s", 1950),
    ("1950s", 1950),
    ("60s", 1960),
    ("1960s", 1960),
    ("70s", 1970),
    ("1970s", 1970),
    ("80s", 1980),
    ("1980s", 1980),
    ("eighties", 1980),
    ("90s", 1990),
    ("1990s", 1990),
    ("nineties", 1990),
    ("2000s", 2000),
    ("2010s", 2010),
    ("2020s", 2020),
];

/// Demonym or place → ISO 3166-1 alpha-2, the tvOS app's curated set of common film and TV origins.
const COUNTRIES: &[(&str, &str)] = &[
    ("spanish", "ES"),
    ("spain", "ES"),
    ("mexican", "MX"),
    ("mexico", "MX"),
    ("argentine", "AR"),
    ("argentinian", "AR"),
    ("argentina", "AR"),
    ("korean", "KR"),
    ("korea", "KR"),
    ("japanese", "JP"),
    ("japan", "JP"),
    ("french", "FR"),
    ("france", "FR"),
    ("german", "DE"),
    ("germany", "DE"),
    ("italian", "IT"),
    ("italy", "IT"),
    ("british", "GB"),
    ("uk", "GB"),
    ("usa", "US"),
    ("indian", "IN"),
    ("india", "IN"),
    ("bollywood", "IN"),
    ("chinese", "CN"),
    ("china", "CN"),
    ("brazilian", "BR"),
    ("brazil", "BR"),
    ("swedish", "SE"),
    ("sweden", "SE"),
    ("norwegian", "NO"),
    ("norway", "NO"),
    ("danish", "DK"),
    ("denmark", "DK"),
    ("dutch", "NL"),
    ("netherlands", "NL"),
    ("russian", "RU"),
    ("russia", "RU"),
    ("turkish", "TR"),
    ("turkey", "TR"),
    ("thai", "TH"),
    ("thailand", "TH"),
    ("filipino", "PH"),
    ("philippines", "PH"),
    ("canadian", "CA"),
    ("canada", "CA"),
    ("australian", "AU"),
    ("australia", "AU"),
    ("irish", "IE"),
    ("ireland", "IE"),
    ("polish", "PL"),
    ("poland", "PL"),
];

/// A `facets.bin` for tests: (id, type, country, year, votes) per title.
#[cfg(test)]
pub(crate) fn blob(rows: &[(u32, MediaType, &str, u16, u32)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for &(id, media_type, country, year, votes) in rows {
        out.extend_from_slice(&(id as i32).to_le_bytes());
        out.push(u8::from(media_type == MediaType::Tv));
        out.extend_from_slice(b"xx");
        out.extend_from_slice(country.as_bytes());
        out.extend_from_slice(&year.to_le_bytes());
        out.extend_from_slice(&votes.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FacetIndex {
        FacetIndex::from_blob(&blob(&[
            (1, MediaType::Movie, "KR", 1985, 100),
            (2, MediaType::Movie, "KR", 1995, 500),
            (3, MediaType::Movie, "ES", 1985, 50),
            (4, MediaType::Tv, "KR", 2010, 300),
        ]))
        .unwrap()
    }

    #[test]
    fn parses_facets_and_keeps_the_rest_as_leftover() {
        let q = FacetQuery::parse("Spanish heist SERIES");
        assert_eq!(
            (q.country, q.media_type, q.leftover.as_str()),
            (Some("ES"), Some(MediaType::Tv), "heist")
        );
        let q = FacetQuery::parse("80s korean horror");
        assert_eq!((q.decade, q.country, q.leftover.as_str()), (Some(1980), Some("KR"), "horror"));
        assert!(q.has_strong_facet());
        let q = FacetQuery::parse("batman movies");
        assert!(q.has_facet() && !q.has_strong_facet(), "a bare type is not a strong facet");
    }

    #[test]
    fn filters_on_every_constraint_most_voted_first() {
        let idx = sample();
        assert_eq!(
            idx.filter(Some(MediaType::Movie), Some("kr"), None),
            vec![(2, MediaType::Movie), (1, MediaType::Movie)]
        );
        assert_eq!(idx.filter(None, Some("KR"), Some(1980)), vec![(1, MediaType::Movie)]);
        assert_eq!(idx.filter(None, Some("KR"), None).len(), 3);
        assert!(idx.filter(None, Some("SE"), None).is_empty());
        assert!(idx.filter(None, None, None).is_empty(), "no facet, no lane");
    }

    #[test]
    fn reads_one_title_by_type_and_id() {
        let idx = sample();
        assert_eq!(
            idx.title(4, MediaType::Tv),
            Some(TitleFacets { country: Some(*b"KR"), language: None, year: Some(2010), votes: 300 })
        );
        // Ids collide across types: movie 4 is not series 4.
        assert_eq!(idx.title(4, MediaType::Movie), None);
    }

    #[test]
    fn a_blob_that_is_not_dfi2_or_is_short_is_refused() {
        assert!(FacetIndex::from_blob(b"DFI1\0\0\0\0").is_none());
        let mut short = blob(&[(1, MediaType::Movie, "KR", 1985, 1)]);
        short.pop();
        assert!(FacetIndex::from_blob(&short).is_none());
    }
}
