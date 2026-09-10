//! Text folding shared by indexing and querying, so "Pokémon" and "pokemon" meet. Mirrors the tvOS app's
//! `TitleIndex.fold`: decomposed, diacritics dropped, lowercased.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// The lowercased, diacritic-free form of `s`.
pub fn fold(s: &str) -> String {
    s.nfd().filter(|c| !is_combining_mark(*c)).collect::<String>().to_lowercase()
}

/// Character trigrams of already-folded text, whitespace removed so multi-word titles still overlap
/// ("the matrix" → "the", "hem", "ema", …). Each is packed into one integer — three code points of 21
/// bits — which keeps the index a sorted `Vec<u64>` instead of a map of strings.
pub(crate) fn trigram_keys(folded: &str) -> Vec<u64> {
    let chars: Vec<char> = folded.chars().filter(|c| !c.is_whitespace()).collect();
    chars.windows(3).map(|w| ((w[0] as u64) << 42) | ((w[1] as u64) << 21) | w[2] as u64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_and_diacritics() {
        assert_eq!(fold("Pokémon"), "pokemon");
        assert_eq!(fold("AMÉLIE"), "amelie");
        assert_eq!(fold("Crème Brûlée"), "creme brulee");
    }

    #[test]
    fn trigrams_ignore_whitespace() {
        assert_eq!(trigram_keys("ab c"), trigram_keys("abc"));
        assert_eq!(trigram_keys("abcd").len(), 2);
        assert!(trigram_keys("ab").is_empty());
    }
}
