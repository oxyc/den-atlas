//! The facet index — each title's country, language, decade, type and vote count — and the parser that
//! finds those facets in a search query ("spanish series", "80s korean horror").
//!
//! One source builds it: `insert`, which takes the five facts from anywhere. den-atlas fills it from the
//! store's own columns, covering the whole corpus. It used to be filled from `facets.bin` too, by a
//! `from_blob` reader that covered 38,532 titles and fell 9,086 behind; that blob is no longer read.
//! This crate must keep compiling for wasm32 and aarch64-apple-tvos, so it cannot depend on `den-store` —
//! hence a constructor that takes facts rather than a reader that takes a store.

use crate::MediaType;
use std::collections::{HashMap, HashSet};

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
    /// ISO 639-1 → its titles. The blob has carried the byte since DFI2; nothing could ask for it.
    ///
    /// Country is not a usable stand-in. Over the corpus, 1,138 Spanish-LANGUAGE titles are made outside
    /// Spain — 53% of its Spanish-language cinema — and asking for "spanish" as a country loses every
    /// Mexican, Argentine, Chilean and Colombian film. The two facts are genuinely different questions.
    by_language: HashMap<[u8; 2], Vec<u32>>,
    by_decade: HashMap<u16, Vec<u32>>,
    by_type: HashMap<MediaType, Vec<u32>>,
}

impl FacetIndex {
    /// An index with no titles, to `insert` into.
    pub fn empty() -> FacetIndex {
        FacetIndex {
            rows: Vec::new(),
            by_title: HashMap::new(),
            by_country: HashMap::new(),
            by_decade: HashMap::new(),
            by_language: HashMap::new(),
            by_type: HashMap::new(),
        }
    }

    /// Add one title's facets, in the order the caller wants them ranked among equals.
    ///
    /// `country` and `language` are taken as written and cased here, so a caller need not know which way
    /// each goes; `[0, 0]` means the fact is absent. A `year` under 1870 is not a year — unknown is
    /// written 0 and cinema does not predate it.
    pub fn insert(
        &mut self,
        tmdb_id: u32,
        media_type: MediaType,
        country: [u8; 2],
        language: [u8; 2],
        year: u16,
        votes: u32,
    ) {
        let position = self.rows.len() as u32;
        let country = [country[0].to_ascii_uppercase(), country[1].to_ascii_uppercase()];
        if country != [0, 0] {
            self.by_country.entry(country).or_default().push(position);
        }
        // TMDB writes `xx` for "no language"; that is an absence, not a language.
        let language = [language[0].to_ascii_lowercase(), language[1].to_ascii_lowercase()];
        if language != [0, 0] && &language != b"xx" {
            self.by_language.entry(language).or_default().push(position);
        }
        if year >= 1870 {
            self.by_decade.entry(year / 10 * 10).or_default().push(position);
        }
        self.by_type.entry(media_type).or_default().push(position);
        self.by_title.entry((media_type, tmdb_id)).or_insert(position);
        self.rows.push(Row { tmdb_id, media_type, votes, language, country, year });
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// One title's facets; `None` when the index doesn't hold it. TMDB writes `xx` for "no language".
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

    /// Titles for which a filterable field is known. `mediaType` is intrinsic to every row; country,
    /// language and year may be absent.
    pub fn coverage(&self, field: &str) -> usize {
        self.coverage_for(field, None)
    }

    /// As `coverage`, among the titles of one type when `media_type` is given.
    pub fn coverage_for(&self, field: &str, media_type: Option<MediaType>) -> usize {
        let known: fn(&Row) -> bool = match field {
            "mediaType" => |_| true,
            "country" => |row| row.country.iter().all(u8::is_ascii_alphabetic),
            "language" => |row| row.language.iter().all(u8::is_ascii_alphabetic) && &row.language != b"xx",
            "year" | "decade" => |row| row.year >= 1870,
            _ => return 0,
        };
        self.rows
            .iter()
            .filter(|row| media_type.is_none_or(|want| row.media_type == want) && known(row))
            .count()
    }

    /// Distinct values and their title counts for the finite-valued facet fields. Year is intentionally exposed
    /// as a typed range by the schema rather than thousands of values; decade is the enumerable form clients use.
    pub fn value_counts(&self, field: &str) -> Vec<(String, usize)> {
        let mut values: Vec<(String, usize)> = match field {
            "mediaType" => self
                .by_type
                .iter()
                .map(|(kind, rows)| {
                    (if *kind == MediaType::Tv { "series" } else { "movie" }.to_owned(), rows.len())
                })
                .collect(),
            "country" => self
                .by_country
                .iter()
                .map(|(code, rows)| (String::from_utf8_lossy(code).into_owned(), rows.len()))
                .collect(),
            "language" => self
                .by_language
                .iter()
                .map(|(code, rows)| (String::from_utf8_lossy(code).into_owned(), rows.len()))
                .collect(),
            "decade" => self.by_decade.iter().map(|(year, rows)| (year.to_string(), rows.len())).collect(),
            _ => Vec::new(),
        };
        values.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        values
    }

    /// Titles matching every given constraint, most-voted first (ties in blob order). Empty when no constraint
    /// is given — the facet lane only fires on a real facet.
    pub fn filter(
        &self,
        media_type: Option<MediaType>,
        country: Option<&str>,
        decade: Option<u16>,
    ) -> Vec<(u32, MediaType)> {
        self.filter_years(media_type, country, decade, None, None)
    }

    /// As `filter`, bounded by an inclusive release-year window.
    ///
    /// A range cannot be a bucket lookup the way a decade is, so it filters the matched set instead — and
    /// when the window is the ONLY thing given, the whole row table is the starting set. That is the price
    /// of letting "recent" propose candidates at all: without it a year window could only refine titles some
    /// other lane had already found, and `q=recent` would answer with whatever the plot vectors made of the
    /// word. A year of 0 means unknown in the blob, and unknown is never a mismatch, so it is kept.
    pub fn filter_years(
        &self,
        media_type: Option<MediaType>,
        country: Option<&str>,
        decade: Option<u16>,
        year_min: Option<u16>,
        year_max: Option<u16>,
    ) -> Vec<(u32, MediaType)> {
        self.filter_all(media_type, country, None, decade, year_min, year_max)
    }

    /// As `filter_years`, and by original language.
    #[allow(clippy::too_many_arguments)]
    pub fn filter_all(
        &self,
        media_type: Option<MediaType>,
        country: Option<&str>,
        language: Option<&str>,
        decade: Option<u16>,
        year_min: Option<u16>,
        year_max: Option<u16>,
    ) -> Vec<(u32, MediaType)> {
        let empty: &[u32] = &[];
        let mut sets: Vec<&[u32]> = Vec::new();
        if let Some(country) = country {
            let key = country_key(country);
            sets.push(key.and_then(|k| self.by_country.get(&k)).map_or(empty, Vec::as_slice));
        }
        if let Some(language) = language {
            let key = language_key(language);
            sets.push(key.and_then(|k| self.by_language.get(&k)).map_or(empty, Vec::as_slice));
        }
        if let Some(decade) = decade {
            sets.push(self.by_decade.get(&decade).map_or(empty, Vec::as_slice));
        }
        if let Some(media_type) = media_type {
            sets.push(self.by_type.get(&media_type).map_or(empty, Vec::as_slice));
        }
        let all: Vec<u32>;
        if sets.is_empty() && (year_min.is_some() || year_max.is_some()) {
            all = (0..self.rows.len() as u32).collect();
            sets.push(&all);
        }
        let Some((smallest, others)) = smallest_first(sets) else { return Vec::new() };
        let others: Vec<HashSet<u32>> = others.into_iter().map(|s| s.iter().copied().collect()).collect();
        let in_window = |p: &u32| {
            let year = self.rows[*p as usize].year;
            year == 0 || (year_min.is_none_or(|min| year >= min) && year_max.is_none_or(|max| year <= max))
        };
        let mut matched: Vec<u32> = smallest
            .iter()
            .copied()
            .filter(|p| others.iter().all(|s| s.contains(p)))
            .filter(|p| in_window(p))
            .collect();
        matched.sort_by_key(|&p| std::cmp::Reverse(self.rows[p as usize].votes));
        matched.iter().map(|&p| (self.rows[p as usize].tmdb_id, self.rows[p as usize].media_type)).collect()
    }
}

fn language_key(code: &str) -> Option<[u8; 2]> {
    let bytes = code.as_bytes();
    (bytes.len() == 2).then(|| [bytes[0].to_ascii_lowercase(), bytes[1].to_ascii_lowercase()])
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
    /// ISO 639-1. Never read from prose — a demonym is as often a country as a language, and the corpus
    /// says the guess is wrong about half the time — so this is set only by an explicit parameter.
    pub language: Option<String>,
    /// The decade's first year, e.g. 1980.
    pub decade: Option<u16>,
    /// An inclusive release-year window. A bare year ("2019") sets both ends; "recent" and "new" set only
    /// the lower one, and "classic" only the upper.
    ///
    /// Decade equality was the only thing a query could say about time, so "recent" meant nothing and a
    /// bare year was matched as PROSE — `korean thriller 2019` pulled a 2020 film above Parasite, because
    /// "2019" went to the plot vectors rather than to a filter.
    pub year_min: Option<u16>,
    pub year_max: Option<u16>,
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
        let tokens: Vec<&str> =
            lowered.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
        // Set when a two-word country name was read, so its second word is not read again.
        let mut claimed = false;
        for (at, &token) in tokens.iter().enumerate() {
            if std::mem::take(&mut claimed) {
                continue;
            }
            if query.country.is_none() {
                // The pair first: "north korean" is North Korea, not "north" and South Korea.
                let pair = tokens.get(at + 1).and_then(|next| lookup(COUNTRIES, &format!("{token} {next}")));
                if let Some(country) = pair {
                    query.country = Some(country);
                    claimed = true;
                    continue;
                }
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
            // A bare four-digit year — but NEVER as the whole query, and never as its first word.
            //
            // 67 titles in the corpus are exactly a four-digit year and 63 of them came out in a different
            // one, so reading the year unconditionally makes each of them erase itself: `q=1917` returned
            // three films from 1917 and not the 2019 one. A year that TRAILS other words ("korean thriller
            // 2019") is the case this is for, and it is the only case where the reading is safe.
            let trailing = at > 0;
            if trailing && query.year_min.is_none() && query.year_max.is_none() && token.len() == 4 {
                if let Ok(year) = token.parse::<u16>() {
                    if (1890..=2100).contains(&year) {
                        query.year_min = Some(year);
                        query.year_max = Some(year);
                        continue;
                    }
                }
            }
            // "recent" / "new" name a window with no end; "classic" / "old" one with no beginning. The
            // boundaries are deliberately generous: someone asking for something recent will accept a film
            // from a couple of years ago, and would rather see one than nothing.
            // "new", "latest", "old", "older" and "classic" are all titles in their own right — `q=old`
            // dropped Shyamalan's Old, `q=new girl` dropped New Girl. Only words no film is called survive.
            if query.year_min.is_none() && matches!(token, "recent" | "newest") {
                query.year_min = Some(RECENT_SINCE);
                continue;
            }
            if query.year_max.is_none() && matches!(token, "classics") {
                query.year_max = Some(CLASSIC_UNTIL);
                continue;
            }
            leftover.push(token);
        }
        query.leftover = leftover.join(" ");
        query
    }

    pub fn has_facet(&self) -> bool {
        self.media_type.is_some()
            || self.country.is_some()
            || self.decade.is_some()
            || self.year_min.is_some()
            || self.year_max.is_some()
    }

    /// A facet ordinary search can't express — a country or a decade. A bare type ("batman movies") is not
    /// one: title and theme search already cover it.
    pub fn has_strong_facet(&self) -> bool {
        self.country.is_some()
            || self.language.is_some()
            || self.decade.is_some()
            || self.year_min.is_some()
            || self.year_max.is_some()
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

/// What "recent" means. A fixed year rather than "now minus N" on purpose: the corpus is a published
/// snapshot, so a moving boundary would quietly empty this facet as the dataset aged, and a wrong constant
/// is easier to notice than a window that shrinks on its own.
const RECENT_SINCE: u16 = 2021;
/// And "classic". Before home video, roughly.
const CLASSIC_UNTIL: u16 = 1979;

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

/// Demonym or place → ISO 3166-1 alpha-2, for every country the corpus records a title as made in
/// (`CORPUS_COUNTRIES` in the tests holds this to that list). An entry of two words is matched as a pair
/// before either word alone.
///
/// Left out on purpose, because their usual reading in a query is something else and a country read from
/// the words discounts every title not made there: "english" (a language — most English-language titles are
/// American), "georgia" and "georgian" (a US state; a period), "jordan", "chad" and "cuba" (people's names),
/// "guinea", and "kong" (King Kong). Their demonyms or full names are here instead.
const COUNTRIES: &[(&str, &str)] = &[
    ("american", "US"),
    ("america", "US"),
    ("usa", "US"),
    ("united states", "US"),
    ("british", "GB"),
    ("britain", "GB"),
    ("great britain", "GB"),
    ("uk", "GB"),
    ("united kingdom", "GB"),
    ("england", "GB"),
    ("scottish", "GB"),
    ("scotland", "GB"),
    ("welsh", "GB"),
    ("wales", "GB"),
    ("french", "FR"),
    ("france", "FR"),
    ("east german", "DD"),
    ("east germany", "DD"),
    ("german", "DE"),
    ("germany", "DE"),
    ("italian", "IT"),
    ("italy", "IT"),
    ("canadian", "CA"),
    ("canada", "CA"),
    ("spanish", "ES"),
    ("spain", "ES"),
    ("australian", "AU"),
    ("australia", "AU"),
    ("north korean", "KP"),
    ("north korea", "KP"),
    ("south korean", "KR"),
    ("south korea", "KR"),
    ("korean", "KR"),
    ("korea", "KR"),
    ("indian", "IN"),
    ("india", "IN"),
    ("bollywood", "IN"),
    ("japanese", "JP"),
    ("japan", "JP"),
    ("belgian", "BE"),
    ("belgium", "BE"),
    ("mexican", "MX"),
    ("mexico", "MX"),
    ("chinese", "CN"),
    ("china", "CN"),
    ("brazilian", "BR"),
    ("brazil", "BR"),
    ("argentine", "AR"),
    ("argentinian", "AR"),
    ("argentina", "AR"),
    ("hong kong", "HK"),
    ("hongkong", "HK"),
    ("swedish", "SE"),
    ("sweden", "SE"),
    ("danish", "DK"),
    ("denmark", "DK"),
    ("russian", "RU"),
    ("russia", "RU"),
    ("polish", "PL"),
    ("poland", "PL"),
    ("swiss", "CH"),
    ("switzerland", "CH"),
    ("austrian", "AT"),
    ("austria", "AT"),
    ("dutch", "NL"),
    ("netherlands", "NL"),
    ("holland", "NL"),
    ("turkish", "TR"),
    ("turkey", "TR"),
    ("czech", "CZ"),
    ("czechia", "CZ"),
    ("finnish", "FI"),
    ("finland", "FI"),
    ("norwegian", "NO"),
    ("norway", "NO"),
    ("irish", "IE"),
    ("ireland", "IE"),
    ("colombian", "CO"),
    ("colombia", "CO"),
    ("new zealand", "NZ"),
    ("new zealander", "NZ"),
    ("chilean", "CL"),
    ("chile", "CL"),
    ("greek", "GR"),
    ("greece", "GR"),
    ("thai", "TH"),
    ("thailand", "TH"),
    ("hungarian", "HU"),
    ("hungary", "HU"),
    ("taiwanese", "TW"),
    ("taiwan", "TW"),
    ("romanian", "RO"),
    ("romania", "RO"),
    ("indonesian", "ID"),
    ("indonesia", "ID"),
    ("icelandic", "IS"),
    ("iceland", "IS"),
    ("iranian", "IR"),
    ("iran", "IR"),
    ("peruvian", "PE"),
    ("peru", "PE"),
    ("portuguese", "PT"),
    ("portugal", "PT"),
    ("israeli", "IL"),
    ("israel", "IL"),
    ("bulgarian", "BG"),
    ("bulgaria", "BG"),
    ("south african", "ZA"),
    ("south africa", "ZA"),
    ("venezuelan", "VE"),
    ("venezuela", "VE"),
    ("emirati", "AE"),
    ("uae", "AE"),
    ("filipino", "PH"),
    ("philippines", "PH"),
    ("egyptian", "EG"),
    ("egypt", "EG"),
    ("cuban", "CU"),
    ("ukrainian", "UA"),
    ("ukraine", "UA"),
    ("nigerian", "NG"),
    ("nigeria", "NG"),
    ("nollywood", "NG"),
    ("serbian", "RS"),
    ("serbia", "RS"),
    ("estonian", "EE"),
    ("estonia", "EE"),
    ("cypriot", "CY"),
    ("cyprus", "CY"),
    ("algerian", "DZ"),
    ("algeria", "DZ"),
    ("uruguayan", "UY"),
    ("uruguay", "UY"),
    ("luxembourgish", "LU"),
    ("luxembourg", "LU"),
    ("dominican", "DO"),
    ("dominican republic", "DO"),
    ("afghan", "AF"),
    ("afghanistan", "AF"),
    ("belarusian", "BY"),
    ("belarus", "BY"),
    ("bolivian", "BO"),
    ("bolivia", "BO"),
    ("croatian", "HR"),
    ("croatia", "HR"),
    ("lithuanian", "LT"),
    ("lithuania", "LT"),
    ("bosnian", "BA"),
    ("bosnia", "BA"),
    ("yugoslav", "YU"),
    ("yugoslavian", "YU"),
    ("yugoslavia", "YU"),
    ("pakistani", "PK"),
    ("pakistan", "PK"),
    ("albanian", "AL"),
    ("albania", "AL"),
    ("vietnamese", "VN"),
    ("vietnam", "VN"),
    ("jamaican", "JM"),
    ("jamaica", "JM"),
    ("burkinabe", "BF"),
    ("burkina faso", "BF"),
    ("malaysian", "MY"),
    ("malaysia", "MY"),
    ("palestinian", "PS"),
    ("palestine", "PS"),
    ("lebanese", "LB"),
    ("lebanon", "LB"),
    ("jordanian", "JO"),
    ("azerbaijani", "AZ"),
    ("azerbaijan", "AZ"),
    ("botswana", "BW"),
    ("ecuadorian", "EC"),
    ("ecuador", "EC"),
    ("armenian", "AM"),
    ("armenia", "AM"),
    ("congolese", "CD"),
    ("congo", "CD"),
    ("kazakh", "KZ"),
    ("kazakhstan", "KZ"),
    ("sri lankan", "LK"),
    ("sri lanka", "LK"),
    ("ivorian", "CI"),
    ("ivory coast", "CI"),
    ("senegalese", "SN"),
    ("senegal", "SN"),
    ("paraguayan", "PY"),
    ("paraguay", "PY"),
    ("singaporean", "SG"),
    ("singapore", "SG"),
    ("bangladeshi", "BD"),
    ("bangladesh", "BD"),
    ("cambodian", "KH"),
    ("cambodia", "KH"),
    ("maltese", "MT"),
    ("malta", "MT"),
    ("latvian", "LV"),
    ("latvia", "LV"),
    ("iraqi", "IQ"),
    ("iraq", "IQ"),
    ("bhutanese", "BT"),
    ("bhutan", "BT"),
    ("moroccan", "MA"),
    ("morocco", "MA"),
    ("libyan", "LY"),
    ("libya", "LY"),
    ("bahamian", "BS"),
    ("bahamas", "BS"),
    ("burmese", "MM"),
    ("burma", "MM"),
    ("myanmar", "MM"),
    ("guatemalan", "GT"),
    ("guatemala", "GT"),
    ("ethiopian", "ET"),
    ("ethiopia", "ET"),
    ("montenegrin", "ME"),
    ("montenegro", "ME"),
    ("ugandan", "UG"),
    ("uganda", "UG"),
    ("panamanian", "PA"),
    ("panama", "PA"),
    ("kenyan", "KE"),
    ("kenya", "KE"),
    ("kyrgyz", "KG"),
    ("kyrgyzstan", "KG"),
    ("macedonian", "MK"),
    ("macedonia", "MK"),
    ("mozambican", "MZ"),
    ("mozambique", "MZ"),
    ("mauritian", "MU"),
    ("mauritius", "MU"),
    ("rwandan", "RW"),
    ("rwanda", "RW"),
    ("andorran", "AD"),
    ("andorra", "AD"),
    ("sudanese", "SD"),
    ("sudan", "SD"),
    ("ghanaian", "GH"),
    ("ghana", "GH"),
    ("cameroonian", "CM"),
    ("cameroon", "CM"),
    ("costa rican", "CR"),
    ("costa rica", "CR"),
    ("gambian", "GM"),
    ("gambia", "GM"),
    ("guinean", "GN"),
    ("haitian", "HT"),
    ("haiti", "HT"),
    ("liechtenstein", "LI"),
    ("monegasque", "MC"),
    ("monaco", "MC"),
    ("malian", "ML"),
    ("mali", "ML"),
    ("mongolian", "MN"),
    ("mongolia", "MN"),
    ("mauritanian", "MR"),
    ("mauritania", "MR"),
    ("nepali", "NP"),
    ("nepalese", "NP"),
    ("nepal", "NP"),
    ("papua", "PG"),
    ("papuan", "PG"),
    ("puerto rican", "PR"),
    ("puerto rico", "PR"),
    ("qatari", "QA"),
    ("qatar", "QA"),
    ("saudi", "SA"),
    ("saudi arabia", "SA"),
    ("slovenian", "SI"),
    ("slovene", "SI"),
    ("slovenia", "SI"),
    ("slovak", "SK"),
    ("slovakia", "SK"),
    ("syrian", "SY"),
    ("syria", "SY"),
    ("chadian", "TD"),
    ("tajik", "TJ"),
    ("tajikistan", "TJ"),
    ("tunisian", "TN"),
    ("tunisia", "TN"),
    ("vatican", "VA"),
    ("vanuatu", "VU"),
    ("kosovar", "XK"),
    ("kosovo", "XK"),
    ("zambian", "ZM"),
    ("zambia", "ZM"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// (id, type, country, year, votes) per title, inserted in the order given — which is the order the
    /// index ranks equals in.
    fn built(rows: &[(u32, MediaType, &str, u16, u32)]) -> FacetIndex {
        let mut index = FacetIndex::empty();
        for &(id, media_type, country, year, votes) in rows {
            let c = country.as_bytes();
            index.insert(id, media_type, [c[0], c[1]], *b"xx", year, votes);
        }
        index
    }

    fn sample() -> FacetIndex {
        built(&[
            (1, MediaType::Movie, "KR", 1985, 100),
            (2, MediaType::Movie, "KR", 1995, 500),
            (3, MediaType::Movie, "ES", 1985, 50),
            (4, MediaType::Tv, "KR", 2010, 300),
        ])
    }

    /// A country is cased on the way in and an absent one is no bucket at all, whatever the caller writes:
    /// the store hands these over as it holds them, so the index must not care which case it gets.
    #[test]
    fn a_country_is_cased_on_insert_and_an_absent_one_is_not_a_bucket() {
        let index = built(&[
            (1, MediaType::Movie, "KR", 1985, 100),
            (2, MediaType::Movie, "kr", 1995, 500),
            (5, MediaType::Movie, "\0\0", 0, 7),
        ]);
        assert_eq!(index.len(), 3);
        assert_eq!(
            index.filter(Some(MediaType::Movie), Some("kr"), None),
            vec![(2, MediaType::Movie), (1, MediaType::Movie)]
        );
        assert_eq!(
            index.title(5, MediaType::Movie),
            Some(TitleFacets { country: None, language: None, year: None, votes: 7 })
        );
    }

    /// Time could only be said as a decade, so "recent" meant nothing and a bare year was matched as prose —
    /// `korean thriller 2019` pulled a 2020 film above Parasite.
    #[test]
    fn a_query_can_bound_its_release_years() {
        let year = |text: &str| {
            let q = FacetQuery::parse(text);
            (q.year_min, q.year_max, q.leftover)
        };
        assert_eq!(year("korean thriller 2019").0, Some(2019), "a TRAILING year is a date");
        assert_eq!(year("recent horror"), (Some(RECENT_SINCE), None, "horror".to_owned()));
        assert_eq!(year("classics"), (None, Some(CLASSIC_UNTIL), String::new()));

        // 67 titles ARE a four-digit year, 63 of them released in a different one. Reading the year when it
        // is the whole query made each of them erase itself: `1917` answered with three films from 1917 and
        // not the 2019 one.
        assert_eq!(year("1917"), (None, None, "1917".to_owned()), "a year alone is a title");
        // ("movie" is claimed by MEDIA_TYPES, so only "2012" is left over — the point is that it stayed a
        // word rather than becoming a date.)
        assert_eq!(year("2012 movie"), (None, None, "2012".to_owned()), "…and so is a leading one");

        // Same rule for words a film can be called. `old` dropped Shyamalan's Old; `new girl` dropped New
        // Girl. Only words nothing is titled survive.
        assert_eq!(year("old"), (None, None, "old".to_owned()));
        assert_eq!(year("new girl"), (None, None, "new girl".to_owned()));
        assert_eq!(year("classic"), (None, None, "classic".to_owned()));
        assert_eq!(year("latest"), (None, None, "latest".to_owned()));

        // Bounded on both sides, so a runtime or a resolution is not a date.
        assert_eq!(year("1080p"), (None, None, "1080p".to_owned()));
        assert_eq!(year("4000"), (None, None, "4000".to_owned()));
        assert_eq!(year("90"), (None, None, "90".to_owned()), "two digits is not a year");

        // A decade still wins where both could read: "1990s" is a decade, not the year 1990.
        let decade = FacetQuery::parse("1990s");
        assert_eq!((decade.decade, decade.year_min), (Some(1990), None));

        // A year window alone is worth a lane of its own: without this "recent" proposes nothing.
        assert!(FacetQuery::parse("recent").has_strong_facet());
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

    /// Every country code the corpus records a title as made in: the `countries` of the 38,669 records in
    /// den-dataset's published facts file (`facts-c85c707b0b18.json`), first-listed and co-producers alike.
    /// `DD` and `YU` are East Germany and Yugoslavia, which Wikidata keeps as their own countries.
    const CORPUS_COUNTRIES: &[&str] = &[
        "AD", "AE", "AF", "AL", "AM", "AR", "AT", "AU", "AZ", "BA", "BD", "BE", "BF", "BG", "BO", "BR", "BS",
        "BT", "BW", "BY", "CA", "CD", "CH", "CI", "CL", "CM", "CN", "CO", "CR", "CU", "CY", "CZ", "DD", "DE",
        "DK", "DO", "DZ", "EC", "EE", "EG", "ES", "ET", "FI", "FR", "GB", "GE", "GH", "GM", "GN", "GR", "GT",
        "HK", "HR", "HT", "HU", "ID", "IE", "IL", "IN", "IQ", "IR", "IS", "IT", "JM", "JO", "JP", "KE", "KG",
        "KH", "KP", "KR", "KZ", "LB", "LI", "LK", "LT", "LU", "LV", "LY", "MA", "MC", "ME", "MK", "ML", "MM",
        "MN", "MR", "MT", "MU", "MX", "MY", "MZ", "NG", "NL", "NO", "NP", "NZ", "PA", "PE", "PG", "PH", "PK",
        "PL", "PR", "PS", "PT", "PY", "QA", "RO", "RS", "RU", "RW", "SA", "SD", "SE", "SG", "SI", "SK", "SN",
        "SY", "TD", "TH", "TJ", "TN", "TR", "TW", "UA", "UG", "US", "UY", "VA", "VE", "VN", "VU", "XK", "YU",
        "ZA", "ZM",
    ];

    /// "bleak finnish 1980s films" dropped Finland: the table held 28 countries, so `finnish` went to the
    /// plot vectors as prose. Every country a title can be recorded under must be sayable.
    #[test]
    fn every_corpus_country_can_be_named() {
        // Georgia alone has no word: both of its names read as something else first.
        let unnamed: Vec<&str> = CORPUS_COUNTRIES
            .iter()
            .copied()
            .filter(|code| !COUNTRIES.iter().any(|(_, c)| c == code))
            .collect();
        assert_eq!(unnamed, vec!["GE"]);
        for (word, code) in COUNTRIES {
            assert!(CORPUS_COUNTRIES.contains(code), "{word} names {code}, which no title is made in");
            assert!(COUNTRIES.iter().filter(|(w, _)| w == word).count() == 1, "{word} is listed twice");
            assert_eq!(FacetQuery::parse(word).country, Some(*code), "{word}");
            assert_eq!(FacetQuery::parse(word).leftover, "", "{word} is claimed whole");
        }
    }

    #[test]
    fn reads_demonyms_and_two_word_names() {
        let q = FacetQuery::parse("bleak finnish 1980s films");
        assert_eq!(
            (q.country, q.decade, q.media_type, q.leftover.as_str()),
            (Some("FI"), Some(1980), Some(MediaType::Movie), "bleak")
        );
        let country = |text: &str| {
            let q = FacetQuery::parse(text);
            (q.country, q.leftover)
        };
        // The pair wins over its second word, which alone names another country.
        assert_eq!(country("north korean propaganda"), (Some("KP"), "propaganda".to_owned()));
        assert_eq!(country("east german spy"), (Some("DD"), "spy".to_owned()));
        assert_eq!(country("south korean thriller"), (Some("KR"), "thriller".to_owned()));
        assert_eq!(country("hong kong action"), (Some("HK"), "action".to_owned()));
        assert_eq!(country("films from new zealand"), (Some("NZ"), "from".to_owned()));
        // A first word with no pair is still read on its own.
        assert_eq!(country("south park"), (None, "south park".to_owned()));
        assert_eq!(country("korean south"), (Some("KR"), "south".to_owned()));
        // Words whose usual reading is not a country stay prose.
        for word in ["english", "georgia", "georgian", "jordan", "chad", "cuba", "guinea", "kong"] {
            assert_eq!(country(word), (None, word.to_owned()), "{word}");
        }
        assert_eq!(country("king kong"), (None, "king kong".to_owned()));
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
}
