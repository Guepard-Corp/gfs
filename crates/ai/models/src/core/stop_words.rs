//===----------------------------------------------------------------------===//
//                         GFS
//
// stop_words.rs
//
// Embedded stopword list: omitted from vocab build and from OOV detection.
// Generated from stop_words.txt
//===----------------------------------------------------------------------===//

use std::collections::HashSet;
use std::sync::OnceLock;

/// Lazily initialized set of stop words
static STOP_WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Initialize the stop words set from the embedded text file
fn init_stop_words() -> HashSet<&'static str> {
    const STOP_WORDS_TXT: &str = include_str!("stop_words.txt");

    STOP_WORDS_TXT
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Returns the static reference to the stop words set
fn get_stop_words_set() -> &'static HashSet<&'static str> {
    STOP_WORDS.get_or_init(init_stop_words)
}

/// Returns true if the word (lowercase) is a stopword.
/// Used by Vocabulary during build (exclude from vocab) and by smart_refresh OOV check (ignore).
///
/// # Arguments
/// * `word` - The word to check (should be lowercase)
///
/// # Returns
/// `true` if the word is a stop word, `false` otherwise
pub fn is_stop_word(word: &str) -> bool {
    get_stop_words_set().contains(word)
}

/// Returns a reference to the set of stopwords (for callers that need to filter a list).
///
/// # Returns
/// A reference to the static HashSet containing all stop words
pub fn get_stop_words() -> &'static HashSet<&'static str> {
    get_stop_words_set()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_stop_word() {
        // Test common stop words
        assert!(is_stop_word("the"));
        assert!(is_stop_word("and"));
        assert!(is_stop_word("or"));
        assert!(is_stop_word("but"));
        assert!(is_stop_word("if"));

        // Test non-stop words
        assert!(!is_stop_word("algorithm"));
        assert!(!is_stop_word("database"));
        assert!(!is_stop_word("computer"));
    }

    #[test]
    fn test_get_stop_words() {
        let stop_words = get_stop_words();

        // Verify that the set is not empty
        assert!(!stop_words.is_empty());

        // Verify some known stop words exist
        assert!(stop_words.contains("the"));
        assert!(stop_words.contains("and"));
        assert!(stop_words.contains("is"));
        assert!(stop_words.contains("are"));
    }

    #[test]
    fn test_stop_words_consistency() {
        // Verify that calling the function multiple times returns the same reference
        let set1 = get_stop_words();
        let set2 = get_stop_words();

        // They should be the same reference
        assert!(std::ptr::eq(set1, set2));
    }
}
