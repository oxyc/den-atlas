//! Numeric maturity policy shared by every discovery surface.
//!
//! A title's age is public-authority metadata from the store. The request supplies policy: an optional
//! maximum age and what to do when the title has no defensible age. Unknown is never silently converted to
//! zero; a Kid request that omits `unknownRatings` therefore fails closed.

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UnknownRatings {
    Allow,
    #[default]
    Block,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Policy {
    pub maximum_age: Option<u8>,
    pub unknown_ratings: UnknownRatings,
}

impl Policy {
    pub fn from_query(query: &str) -> Result<Self, String> {
        let maximum_age = crate::handler::query_param(query, "maximumAge")
            .map(|value| value.parse::<u8>().map_err(|_| "maximumAge must be an integer from 0 to 18"))
            .transpose()?;
        let unknown_ratings = match crate::handler::query_param(query, "unknownRatings").as_deref() {
            None | Some("block") => UnknownRatings::Block,
            Some("allow") => UnknownRatings::Allow,
            Some(_) => return Err("unknownRatings must be allow or block".to_owned()),
        };
        let policy = Self { maximum_age, unknown_ratings };
        policy.check()?;
        Ok(policy)
    }

    pub fn check(self) -> Result<(), String> {
        if self.maximum_age.is_some_and(|age| age > 18) {
            return Err("maximumAge must be an integer from 0 to 18".to_owned());
        }
        if self.maximum_age.is_none() && self.unknown_ratings == UnknownRatings::Allow {
            return Err("unknownRatings requires maximumAge".to_owned());
        }
        Ok(())
    }

    pub fn allows(self, minimum_age: Option<u8>) -> bool {
        let Some(maximum_age) = self.maximum_age else { return true };
        match minimum_age {
            Some(minimum_age) => minimum_age <= maximum_age,
            None => self.unknown_ratings == UnknownRatings::Allow,
        }
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
    }

    #[test]
    fn refuses_ambiguous_or_out_of_range_policy() {
        assert!(Policy::from_query("maximumAge=19").is_err());
        assert!(Policy::from_query("maximumAge=pg13").is_err());
        assert!(Policy::from_query("unknownRatings=allow").is_err());
        assert!(Policy::from_query("maximumAge=7&unknownRatings=maybe").is_err());
    }
}
