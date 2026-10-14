//! Numeric maturity policy shared by every discovery surface.
//!
//! A title's age is public-authority metadata from the store. The request supplies policy: an optional
//! maximum age and what to do when the title has no defensible age. Unknown is never silently converted to
//! zero; a Kid request that omits `unknownRatings` therefore fails closed.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UnknownRatings {
    #[default]
    Allow,
    Block,
}

/// Which intended-audience evidence a Kid catalogue accepts.
///
/// This is deliberately explicit instead of inferred from a numeric ceiling: age safety and catalogue
/// relevance are separate product decisions, and a restricted adult/Visitor request must not silently become
/// a Kid catalogue.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
pub enum KidAudience {
    #[serde(rename = "child")]
    Child,
    #[serde(rename = "child-or-teen")]
    ChildOrTeen,
}

impl KidAudience {
    fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "child" => Ok(Self::Child),
            "child-or-teen" => Ok(Self::ChildOrTeen),
            _ => Err("kidAudience must be child or child-or-teen"),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Child => "child",
            Self::ChildOrTeen => "child-or-teen",
        }
    }
}

/// An explicitly supplied ISO 3166-1 alpha-2 viewer region.
///
/// The public store currently carries one conservative global age, so region does not change eligibility yet.
/// It is nevertheless part of request identity: a later regional authority mapping must not inherit an answer
/// cached for another region. Atlas never fills this from its operator default or an install's catalogue country.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Region([u8; 2]);

impl Region {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let bytes = value.as_bytes();
        if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_alphabetic) {
            return Err("region must be a two-letter ISO 3166-1 code");
        }
        Ok(Self([bytes[0].to_ascii_uppercase(), bytes[1].to_ascii_uppercase()]))
    }

    pub fn as_str(&self) -> &str {
        // `parse` admits ASCII letters only.
        std::str::from_utf8(&self.0).expect("Region is ASCII")
    }
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RegionVisitor;

        impl Visitor<'_> for RegionVisitor {
            type Value = Region;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a two-letter ISO 3166-1 region code")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Region::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(RegionVisitor)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Policy {
    pub maximum_age: Option<u8>,
    pub unknown_ratings: UnknownRatings,
    pub region: Option<Region>,
    pub kid_audience: Option<KidAudience>,
}

impl Policy {
    pub fn from_query(query: &str) -> Result<Self, String> {
        let maximum_age = crate::handler::query_param(query, "maximumAge")
            .map(|value| value.parse::<u8>().map_err(|_| "maximumAge must be an integer from 0 to 18"))
            .transpose()?;
        let unknown_ratings = match crate::handler::query_param(query, "unknownRatings").as_deref() {
            None if maximum_age.is_some() => UnknownRatings::Block,
            None | Some("allow") => UnknownRatings::Allow,
            Some("block") => UnknownRatings::Block,
            Some(_) => return Err("unknownRatings must be allow or block".to_owned()),
        };
        let region = crate::handler::query_param(query, "region")
            .map(|value| Region::parse(&value).map_err(str::to_owned))
            .transpose()?;
        let kid_audience = crate::handler::query_param(query, "kidAudience")
            .map(|value| KidAudience::parse(&value).map_err(str::to_owned))
            .transpose()?;
        let policy = Self { maximum_age, unknown_ratings, region, kid_audience };
        policy.check()?;
        Ok(policy)
    }

    pub fn check(self) -> Result<(), String> {
        if self.maximum_age.is_some_and(|age| age > 18) {
            return Err("maximumAge must be an integer from 0 to 18".to_owned());
        }
        Ok(())
    }

    pub fn allows(self, minimum_age: Option<u8>) -> bool {
        match minimum_age {
            Some(minimum_age) => self.maximum_age.is_none_or(|maximum_age| minimum_age <= maximum_age),
            None => self.unknown_ratings == UnknownRatings::Allow,
        }
    }

    /// Test a title lazily, so the unrestricted policy preserves the historical path without even
    /// consulting the maturity view. Besides avoiding needless work, this keeps instrumentation able to
    /// prove that adding policy support has no data-access cost for ordinary requests.
    pub fn allows_with(self, minimum_age: impl FnOnce() -> Option<u8>) -> bool {
        !self.age_restricted() || self.allows(minimum_age())
    }

    /// Apply age safety and Kid catalogue relevance lazily. For a Kid the title must be made for its band; an unknown
    /// age is admitted only for a title made for children (den-spec profiles-v1 §5). Made for teens is 13 to 17, too
    /// wide to stand in for a missing age (an unrated TV-MA teen drama), so such a title needs a known age.
    pub fn allows_title_with(
        self,
        minimum_age: impl FnOnce() -> Option<u8>,
        made_for: impl Fn(KidAudience) -> bool,
    ) -> bool {
        let Some(band) = self.kid_audience else { return self.allows_with(minimum_age) };
        made_for(band) && self.kid_age_allows(minimum_age(), &made_for)
    }

    /// Whether a title may be opened directly — detail, play, sources, download — under this policy. There is no
    /// catalogue gate here: a Kid may open an age-eligible title not made for it. Made for children admits an
    /// unknown age; the store's Kid veto (a serious warning on a title not made for children) refuses whatever the
    /// age, so a single lenient public rating of an adult work cannot open it (profiles-v1 §5).
    pub fn allows_direct(
        self,
        minimum_age: Option<u8>,
        made_for: impl Fn(KidAudience) -> bool,
        vetoed: impl FnOnce() -> bool,
    ) -> bool {
        if self.kid_audience.is_none() {
            return self.allows_with(|| minimum_age);
        }
        !vetoed() && self.kid_age_allows(minimum_age, &made_for)
    }

    /// A Kid's age rule: a known age is held to the ceiling, an unknown one admitted when the title is made for
    /// children or the policy allows unknown ratings.
    fn kid_age_allows(self, minimum_age: Option<u8>, made_for: &impl Fn(KidAudience) -> bool) -> bool {
        !self.age_restricted()
            || match minimum_age {
                Some(age) => self.allows(Some(age)),
                None => self.allows(None) || made_for(KidAudience::Child),
            }
    }

    pub fn age_restricted(self) -> bool {
        self.maximum_age.is_some() || self.unknown_ratings == UnknownRatings::Block
    }

    pub fn restricted(self) -> bool {
        self.age_restricted() || self.kid_audience.is_some()
    }

    /// Canonical cache/ETag identity for every explicitly meaningful maturity input.
    ///
    /// `maximumAge=12` and `maximumAge=12&unknownRatings=block` deliberately converge. A request with no
    /// policy and no explicit region returns `None`, preserving all pre-policy memo keys and ETags byte-for-byte.
    pub fn cache_identity(self) -> Option<String> {
        if !self.restricted() && self.region.is_none() {
            return None;
        }
        Some(format!(
            "maturity:v2;maximumAge={};unknownRatings={};region={};kidAudience={}",
            self.maximum_age.map_or_else(|| "-".to_owned(), |age| age.to_string()),
            match self.unknown_ratings {
                UnknownRatings::Allow => "allow",
                UnknownRatings::Block => "block",
            },
            self.region.map_or_else(|| "-".to_owned(), |region| region.as_str().to_owned()),
            self.kid_audience.map_or("-", KidAudience::as_str),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_fail_closed_numeric_policy() {
        let policy = Policy::from_query("maximumAge=12").unwrap();
        assert!(policy.allows(Some(12)));
        assert!(!policy.allows(Some(13)));
        assert!(!policy.allows(None));
        assert!(Policy::from_query("maximumAge=12&unknownRatings=allow").unwrap().allows(None));
        assert!(Policy::from_query("").unwrap().allows(None));
        let unrated_only = Policy::from_query("unknownRatings=block").unwrap();
        assert!(unrated_only.allows(Some(18)));
        assert!(!unrated_only.allows(None));
        assert_eq!(Policy::from_query("region=fi").unwrap().region.unwrap().as_str(), "FI");
        assert_eq!(serde_json::from_str::<Region>(r#""se""#).unwrap().as_str(), "SE");
        assert_eq!(
            Policy::from_query("kidAudience=child-or-teen").unwrap().kid_audience,
            Some(KidAudience::ChildOrTeen)
        );
    }

    #[test]
    fn a_kid_admits_an_unrated_title_made_for_its_band_and_nothing_else() {
        let kid = Policy::from_query("maximumAge=7&unknownRatings=block&kidAudience=child").unwrap();
        assert!(kid.allows_title_with(|| None, |_| true), "unrated, made for children");
        assert!(!kid.allows_title_with(|| None, |_| false), "unrated, not made for children");
        assert!(!kid.allows_title_with(|| Some(12), |_| true), "a known age above the ceiling still blocks");
        assert!(kid.allows_title_with(|| Some(7), |_| true));
        // Not a Kid: an unknown age is decided by unknownRatings alone.
        let ceiling = Policy::from_query("maximumAge=7&unknownRatings=block").unwrap();
        assert!(!ceiling.allows_title_with(|| None, |_| true));
    }

    #[test]
    fn made_for_teens_needs_a_known_age() {
        let kid = Policy::from_query("maximumAge=12&unknownRatings=block&kidAudience=child-or-teen").unwrap();
        let teens_only = |band: KidAudience| band == KidAudience::ChildOrTeen;
        // Wayne: unrated, made for teens. In the band, but no age to hold it to.
        assert!(!kid.allows_title_with(|| None, teens_only));
        assert!(!kid.allows_direct(None, teens_only, || false));
        assert!(kid.allows_title_with(|| Some(12), teens_only), "a rated teen title within the ceiling");
    }

    #[test]
    fn a_kid_opens_directly_what_its_age_admits_unless_a_serious_warning_vetoes_it() {
        let kid = Policy::from_query("maximumAge=12&unknownRatings=block&kidAudience=child-or-teen").unwrap();
        let (vetoed, calm) = (|| true, || false);
        // The Blue Lagoon: one lenient rating of 8, made for teens, vetoed for sexual content.
        assert!(!kid.allows_direct(Some(8), |_| true, vetoed));
        assert!(kid.allows_direct(Some(8), |_| false, calm), "no catalogue gate on a direct read");
        assert!(kid.allows_direct(None, |_| true, calm), "unrated, made for its band");
        assert!(!kid.allows_direct(None, |_| false, calm), "unrated, not made for it");
        assert!(!kid.allows_direct(Some(13), |_| true, calm), "a known age still decides");
        // Not a Kid: the age policy alone, no veto.
        let ceiling = Policy::from_query("maximumAge=12&unknownRatings=block").unwrap();
        assert!(ceiling.allows_direct(Some(8), |_| false, vetoed));
        assert!(!ceiling.allows_direct(None, |_| true, calm));
        assert!(Policy::default().allows_direct(None, |_| false, vetoed));
    }

    #[test]
    fn unrestricted_policy_does_not_evaluate_the_age_lookup() {
        let mut looked_up = false;
        assert!(Policy::default().allows_with(|| {
            looked_up = true;
            None
        }));
        assert!(!looked_up);
    }

    #[test]
    fn refuses_ambiguous_or_out_of_range_policy() {
        assert!(Policy::from_query("maximumAge=19").is_err());
        assert!(Policy::from_query("maximumAge=pg13").is_err());
        assert!(Policy::from_query("maximumAge=7&unknownRatings=maybe").is_err());
        assert!(Policy::from_query("region=FIN").is_err());
        assert!(Policy::from_query("region=1x").is_err());
        assert!(Policy::from_query("kidAudience=family").is_err());
        assert!(serde_json::from_str::<Region>(r#""north""#).is_err());
    }

    #[test]
    fn cache_identity_is_canonical_and_leaves_the_legacy_request_untouched() {
        assert_eq!(Policy::from_query("").unwrap().cache_identity(), None);
        assert_eq!(Policy::from_query("unknownRatings=allow").unwrap().cache_identity(), None);
        assert_eq!(
            Policy::from_query("maximumAge=12&region=fi").unwrap().cache_identity(),
            Policy::from_query("region=FI&unknownRatings=block&maximumAge=12").unwrap().cache_identity()
        );
        assert_ne!(
            Policy::from_query("maximumAge=12&region=FI").unwrap().cache_identity(),
            Policy::from_query("maximumAge=12&region=SE").unwrap().cache_identity()
        );
        assert_ne!(
            Policy::from_query("maximumAge=12").unwrap().cache_identity(),
            Policy::from_query("maximumAge=12&kidAudience=child-or-teen").unwrap().cache_identity()
        );
    }
}
