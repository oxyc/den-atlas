//! Text folding shared by indexing and querying, so "Pokémon" and "pokemon" meet. Mirrors the tvOS app's
//! `TitleIndex.fold`: decomposed, diacritics dropped, lowercased.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// The lowercased, diacritic-free form of `s`, with `&` read as the word it stands for.
///
/// `&` used to fall to `words`, which splits on anything non-alphanumeric — so "Hustle & Flow" folded to
/// "hustle flow" while the spelling anyone types folded to "hustle and flow". The two never met: `q=hustle
/// and flow` returned Nathalie Granger, A River Runs Through It and Partly Cloudy, every hit `t=0.0`, the
/// film itself nowhere. 322 records carry a `&` name with no "and" spelling indexed beside it.
///
/// Both sides of every comparison come through here — the index is built with it (`TitleIndex::build`), the
/// title lane queries with it, and `search::words` folds with it — so the two spellings meet everywhere at
/// once. It is the only fold safe to widen this way: no two distinct films differ solely by `&` versus the
/// word "and", so unlike stripping articles or part suffixes it cannot merge one title into another.
pub fn fold(s: &str) -> String {
    s.nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
        // Spaces around it, so "Tom&Jerry" folds the same as "Tom & Jerry" rather than to "tomandjerry".
        .replace('&', " and ")
}

/// Character trigrams of already-folded text, whitespace removed so multi-word titles still overlap
/// ("the matrix" → "the", "hem", "ema", …). Each is packed into one integer — three code points of 21
/// bits — which keeps the index a sorted `Vec<u64>` instead of a map of strings.
pub fn trigram_keys(folded: &str) -> Vec<u64> {
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

    /// `q=hustle and flow` found nothing at all — every hit scored t=0.0 and Hustle & Flow was not among
    /// them — because `&` was dropped as a separator instead of read as the word.
    #[test]
    fn an_ampersand_meets_the_word_it_stands_for() {
        // Whitespace is stripped from trigrams, so this is the comparison that actually decides retrieval.
        assert_eq!(trigram_keys(&fold("Hustle & Flow")), trigram_keys(&fold("hustle and flow")));
        assert_eq!(trigram_keys(&fold("Tom&Jerry")), trigram_keys(&fold("Tom and Jerry")));
        assert_eq!(trigram_keys(&fold("Cloak & Dagger")), trigram_keys(&fold("Cloak and Dagger")));
    }

    #[test]
    fn trigrams_ignore_whitespace() {
        assert_eq!(trigram_keys("ab c"), trigram_keys("abc"));
        assert_eq!(trigram_keys("abcd").len(), 2);
        assert!(trigram_keys("ab").is_empty());
    }
}
