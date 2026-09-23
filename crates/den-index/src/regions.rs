//! Notable regions: named groups of countries of origin, for filtering ("Nordic") and ranking. Hand-kept;
//! countries are ISO 3166-1 alpha-2, uppercase, as the facts spell them. A country may sit in more than one
//! region (Egypt is both Middle Eastern and African).

/// One region: its slug (the id a filter names it by), its display label, the words a search finds it by
/// besides the label, and its member countries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub slug: &'static str,
    pub label: &'static str,
    pub aliases: &'static [&'static str],
    pub countries: &'static [&'static str],
}

pub const REGIONS: &[Region] = &[
    Region {
        slug: "nordic",
        label: "Nordic",
        aliases: &["nordic", "nordics"],
        countries: &["SE", "NO", "DK", "FI", "IS"],
    },
    Region {
        slug: "scandinavian",
        label: "Scandinavian",
        aliases: &["scandinavian", "scandinavia", "scandi"],
        countries: &["SE", "NO", "DK"],
    },
    Region {
        slug: "british-irish",
        label: "British & Irish",
        aliases: &["british isles", "uk and ireland"],
        countries: &["GB", "IE"],
    },
    Region {
        slug: "slavic",
        label: "Slavic",
        aliases: &["slavic", "eastern european"],
        countries: &["RU", "UA", "BY", "PL", "CZ", "SK", "SI", "HR", "RS", "BA", "ME", "MK", "BG"],
    },
    Region { slug: "north-american", label: "North American", aliases: &[], countries: &["US", "CA"] },
    Region {
        slug: "latin-american",
        label: "Latin American",
        aliases: &["latin", "latino", "latin america", "south american"],
        countries: &[
            "MX", "GT", "BZ", "HN", "SV", "NI", "CR", "PA", "CU", "DO", "PR", "CO", "VE", "EC", "PE", "BO",
            "BR", "PY", "UY", "AR", "CL",
        ],
    },
    Region {
        slug: "east-asian",
        label: "East Asian",
        aliases: &["east asian", "asian"],
        countries: &["JP", "KR", "CN", "TW", "HK"],
    },
    Region {
        slug: "southeast-asian",
        label: "Southeast Asian",
        aliases: &[],
        countries: &["TH", "VN", "PH", "ID", "MY", "SG", "KH", "LA", "MM"],
    },
    Region {
        slug: "south-asian",
        label: "South Asian",
        aliases: &["indian subcontinent", "desi"],
        countries: &["IN", "PK", "BD", "LK", "NP"],
    },
    Region {
        slug: "middle-eastern",
        label: "Middle Eastern",
        aliases: &["middle east", "arab"],
        countries: &[
            "TR", "IR", "IL", "SA", "AE", "QA", "KW", "BH", "OM", "JO", "LB", "SY", "IQ", "YE", "PS", "EG",
        ],
    },
    Region {
        slug: "african",
        label: "African",
        aliases: &["africa", "nollywood"],
        countries: &[
            "DZ", "AO", "BJ", "BW", "BF", "BI", "CM", "CV", "CF", "TD", "KM", "CG", "CD", "CI", "DJ", "EG",
            "GQ", "ER", "SZ", "ET", "GA", "GM", "GH", "GN", "GW", "KE", "LS", "LR", "LY", "MG", "MW", "ML",
            "MR", "MU", "MA", "MZ", "NA", "NE", "NG", "RW", "ST", "SN", "SC", "SL", "SO", "ZA", "SS", "SD",
            "TZ", "TG", "TN", "UG", "ZM", "ZW",
        ],
    },
    Region {
        slug: "oceanian",
        label: "Oceanian",
        aliases: &["australian", "new zealand", "oceania", "aussie"],
        countries: &["AU", "NZ"],
    },
];

/// A region by its slug.
pub fn region(slug: &str) -> Option<&'static Region> {
    REGIONS.iter().find(|r| r.slug == slug)
}

/// The continents, for More Like This's lightest regional tier (`similar.rs`, `w_region`): wider than any
/// region above and not a filter, so kept apart from `REGIONS`. Each country is on exactly one; Turkey,
/// Cyprus, Russia and the Caucasus go where their film industries face — Europe for Russia and Cyprus, Asia
/// for Turkey and the Caucasus. Central America and the Caribbean are North America.
pub const CONTINENTS: &[Region] = &[
    Region {
        slug: "europe",
        label: "Europe",
        aliases: &[],
        countries: &[
            "AD", "AL", "AT", "BA", "BE", "BG", "BY", "CH", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FO",
            "FR", "GB", "GI", "GR", "HR", "HU", "IE", "IS", "IT", "LI", "LT", "LU", "LV", "MC", "MD", "ME",
            "MK", "MT", "NL", "NO", "PL", "PT", "RO", "RS", "RU", "SE", "SI", "SK", "SM", "UA", "VA", "XK",
        ],
    },
    Region {
        slug: "asia",
        label: "Asia",
        aliases: &[],
        countries: &[
            "AE", "AF", "AM", "AZ", "BD", "BH", "BN", "BT", "CN", "GE", "HK", "ID", "IL", "IN", "IQ", "IR",
            "JO", "JP", "KG", "KH", "KP", "KR", "KW", "KZ", "LA", "LB", "LK", "MM", "MN", "MO", "MV", "MY",
            "NP", "OM", "PH", "PK", "PS", "QA", "SA", "SG", "SY", "TH", "TJ", "TL", "TM", "TR", "TW", "UZ",
            "VN", "YE",
        ],
    },
    Region {
        slug: "africa",
        label: "Africa",
        aliases: &[],
        countries: &[
            "DZ", "AO", "BJ", "BW", "BF", "BI", "CM", "CV", "CF", "TD", "KM", "CG", "CD", "CI", "DJ", "EG",
            "GQ", "ER", "SZ", "ET", "GA", "GM", "GH", "GN", "GW", "KE", "LS", "LR", "LY", "MG", "MW", "ML",
            "MR", "MU", "MA", "MZ", "NA", "NE", "NG", "RW", "ST", "SN", "SC", "SL", "SO", "ZA", "SS", "SD",
            "TZ", "TG", "TN", "UG", "ZM", "ZW",
        ],
    },
    Region {
        slug: "north-america",
        label: "North America",
        aliases: &[],
        countries: &[
            "US", "CA", "MX", "GT", "BZ", "HN", "SV", "NI", "CR", "PA", "CU", "DO", "PR", "HT", "JM", "BS",
            "BB", "TT",
        ],
    },
    Region {
        slug: "south-america",
        label: "South America",
        aliases: &[],
        countries: &["CO", "VE", "EC", "PE", "BO", "BR", "PY", "UY", "AR", "CL", "GY", "SR"],
    },
    Region {
        slug: "oceania",
        label: "Oceania",
        aliases: &[],
        countries: &["AU", "NZ", "FJ", "PG", "WS", "TO"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_well_formed() {
        for (i, r) in REGIONS.iter().enumerate() {
            assert!(REGIONS[..i].iter().all(|o| o.slug != r.slug), "{} twice", r.slug);
            assert!(r.slug.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{}", r.slug);
            assert!(!r.countries.is_empty(), "{}", r.slug);
            for c in r.countries {
                assert!(c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()), "{}: {c}", r.slug);
                assert_eq!(r.countries.iter().filter(|o| o == &c).count(), 1, "{}: {c} twice", r.slug);
            }
        }
        assert_eq!(region("nordic").map(|r| r.countries.len()), Some(5));
        assert_eq!(region("african").map(|r| r.countries.len()), Some(54));
        assert!(region("Nordic").is_none(), "slugs are lowercase");
    }

    /// Every country is on one continent, and every region's country is on one.
    #[test]
    fn every_country_is_on_exactly_one_continent() {
        let on = |c: &str| CONTINENTS.iter().filter(|k| k.countries.contains(&c)).count();
        for k in CONTINENTS {
            for c in k.countries {
                assert!(c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()), "{}: {c}", k.slug);
                assert_eq!(on(c), 1, "{c}");
            }
        }
        for r in REGIONS {
            for c in r.countries {
                assert_eq!(on(c), 1, "{}: {c}", r.slug);
            }
        }
    }
}
