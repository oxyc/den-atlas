//! Locale-neutral display headings published beside Atlas's legacy English strings.
//!
//! `id` is the stable semantic identity a client localizes, `args` are verbatim values substituted by
//! that localization, and `fallback` is the canonical English text. Keeping the old `name`/`title` field beside
//! this object lets clients adopt the contract independently.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Heading {
    pub id: String,
    pub args: BTreeMap<String, String>,
    pub fallback: String,
}

impl Heading {
    pub fn fixed(id: impl Into<String>, fallback: impl Into<String>) -> Self {
        Self { id: id.into(), args: BTreeMap::new(), fallback: fallback.into() }
    }

    pub fn with_args<const N: usize>(
        id: impl Into<String>,
        args: [(&str, &str); N],
        fallback: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            args: args.into_iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect(),
            fallback: fallback.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_names_identity_args_and_canonical_fallback() {
        let value = serde_json::to_value(Heading::with_args(
            "atlas.catalog.popular_on",
            [("service", "Netflix")],
            "Popular on Netflix",
        ))
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "id": "atlas.catalog.popular_on",
                "args": { "service": "Netflix" },
                "fallback": "Popular on Netflix",
            })
        );
    }
}
