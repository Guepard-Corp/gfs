//===----------------------------------------------------------------------===//
//                         GFS
//
// vocab.rs
//
// Vocabulary builder for Word2Vec training.
// Migrated from DuckDB's extension/semantic_search/core/vocab.cpp
//===----------------------------------------------------------------------===//

use std::collections::HashMap;
use super::stop_words::is_stop_word;

/// A single word entry in the vocabulary
#[derive(Debug, Clone)]
pub struct WordEntry {
    /// The word string
    pub word: String,
    /// Number of times this word appears in the corpus
    pub count: u64,
    /// Index in the vocabulary (position after sorting by frequency)
    pub index: usize,
}

/// Vocabulary for Word2Vec training
///
/// The vocabulary:
/// - Filters stop words during building
/// - Applies minimum count threshold
/// - Sorts words by frequency (most frequent first)
/// - Builds a unigram table for negative sampling (using P(w)^0.75)
/// - Computes discard probabilities for subsampling frequent words
#[derive(Debug, Clone)]
pub struct Vocabulary {
    /// Ordered list of word entries (sorted by frequency descending)
    entries: Vec<WordEntry>,
    /// Fast lookup from word to index
    word_to_index: HashMap<String, usize>,
    /// Unigram table for negative sampling (10M entries)
    unigram_table: Vec<usize>,
    /// Discard probability for each word (for subsampling)
    discard_prob: Vec<f64>,
    /// Total words encountered in corpus (including filtered)
    total_words: u64,
    /// Total words kept in vocabulary (after filtering)
    train_words: u64,
}

impl Vocabulary {
    /// Size of the unigram table for negative sampling
    pub const UNIGRAM_TABLE_SIZE: usize = 10_000_000; // 1e7

    /// Power to raise word frequencies for negative sampling (Mikolov uses 0.75)
    pub const UNIGRAM_POWER: f64 = 0.75;

    /// Threshold for computing discard probabilities (subsampling)
    const SUBSAMPLE_THRESHOLD: f64 = 1e-5;

    /// Creates a new empty vocabulary
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            word_to_index: HashMap::new(),
            unigram_table: Vec::new(),
            discard_prob: Vec::new(),
            total_words: 0,
            train_words: 0,
        }
    }

    /// Build vocabulary from a corpus of documents
    ///
    /// # Arguments
    /// * `corpus` - Vector of text documents
    /// * `min_count` - Minimum word frequency to include in vocabulary
    ///
    /// # Process
    /// 1. Tokenize all documents
    /// 2. Count word frequencies (excluding stop words)
    /// 3. Filter words below min_count threshold
    /// 4. Sort by frequency (descending)
    /// 5. Build unigram table for negative sampling
    /// 6. Compute discard probabilities for subsampling
    pub fn build(&mut self, corpus: &[String], min_count: u64) {
        // Count word frequencies
        let mut raw_counts: HashMap<String, u64> = HashMap::new();
        self.total_words = 0;

        for doc in corpus {
            let tokens = Self::tokenize(doc);
            for token in tokens {
                if is_stop_word(&token) {
                    continue;
                }
                *raw_counts.entry(token).or_insert(0) += 1;
                self.total_words += 1;
            }
        }

        // Filter by min_count and create entries
        self.entries.clear();
        self.word_to_index.clear();

        for (word, count) in raw_counts {
            if count >= min_count {
                let idx = self.entries.len();
                self.entries.push(WordEntry {
                    word,
                    count,
                    index: idx,
                });
            }
        }

        // Sort by frequency (descending)
        self.entries.sort_by(|a, b| b.count.cmp(&a.count));

        // Update indices and rebuild lookup map
        for (i, entry) in self.entries.iter_mut().enumerate() {
            entry.index = i;
            self.word_to_index.insert(entry.word.clone(), i);
        }

        // Calculate total training words
        self.train_words = self.entries.iter().map(|e| e.count).sum();

        // Build auxiliary structures
        self.build_unigram_table();
        self.compute_discard_probabilities();
    }

    /// Restore vocabulary from saved words and counts
    ///
    /// Used when loading a pre-trained model
    ///
    /// # Arguments
    /// * `words` - Vector of words in the vocabulary
    /// * `counts` - Corresponding frequency counts
    pub fn restore(&mut self, words: &[String], counts: &[u64]) {
        assert_eq!(words.len(), counts.len(), "Words and counts must have same length");

        self.entries.clear();
        self.word_to_index.clear();
        self.total_words = 0;
        self.train_words = 0;

        for (i, (word, &count)) in words.iter().zip(counts.iter()).enumerate() {
            self.entries.push(WordEntry {
                word: word.clone(),
                count,
                index: i,
            });
            self.word_to_index.insert(word.clone(), i);
            self.train_words += count;
        }

        self.total_words = self.train_words;
        self.build_unigram_table();
        self.compute_discard_probabilities();
    }

    /// Tokenize text into lowercase words
    ///
    /// Splits on whitespace and converts to lowercase
    ///
    /// # Arguments
    /// * `text` - Input text to tokenize
    ///
    /// # Returns
    /// Vector of lowercase tokens
    pub fn tokenize(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();

        for ch in text.chars() {
            if ch.is_whitespace() {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            } else {
                current.push(ch.to_lowercase().next().unwrap_or(ch));
            }
        }

        if !current.is_empty() {
            tokens.push(current);
        }

        tokens
    }

    /// Convert document text to word indices
    ///
    /// # Arguments
    /// * `text` - Input document text
    ///
    /// # Returns
    /// Vector of word indices (stops words and OOV words are excluded)
    pub fn doc_to_indices(&self, text: &str) -> Vec<usize> {
        let mut indices = Vec::new();
        let tokens = Self::tokenize(text);

        for token in tokens {
            if is_stop_word(&token) {
                continue;
            }
            if let Some(&idx) = self.word_to_index.get(&token) {
                indices.push(idx);
            }
        }

        indices
    }

    /// Look up the index of a word
    ///
    /// # Arguments
    /// * `word` - Word to look up
    ///
    /// # Returns
    /// Some(index) if word is in vocabulary, None otherwise
    pub fn lookup(&self, word: &str) -> Option<usize> {
        self.word_to_index.get(word).copied()
    }

    /// Get the word at a specific index
    ///
    /// # Arguments
    /// * `idx` - Word index
    ///
    /// # Returns
    /// Word string
    ///
    /// # Panics
    /// If index is out of bounds
    pub fn word_at(&self, idx: usize) -> &str {
        &self.entries[idx].word
    }

    /// Get the count (frequency) of the word at a specific index
    ///
    /// # Arguments
    /// * `idx` - Word index
    ///
    /// # Returns
    /// Word frequency count
    ///
    /// # Panics
    /// If index is out of bounds
    pub fn count_at(&self, idx: usize) -> u64 {
        self.entries[idx].count
    }

    /// Get the vocabulary size (number of unique words)
    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// Get total words encountered in corpus (including filtered words)
    pub fn total_words(&self) -> u64 {
        self.total_words
    }

    /// Get total words used in training (after filtering)
    pub fn train_words(&self) -> u64 {
        self.train_words
    }

    /// Get discard probability for a word (for subsampling frequent words)
    ///
    /// # Arguments
    /// * `idx` - Word index
    ///
    /// # Returns
    /// Probability of discarding this word during training (0.0 to 1.0)
    ///
    /// # Panics
    /// If index is out of bounds
    pub fn discard_probability(&self, idx: usize) -> f64 {
        self.discard_prob[idx]
    }

    /// Sample a word index for negative sampling
    ///
    /// Uses the unigram distribution raised to power 0.75
    ///
    /// # Arguments
    /// * `rng_val` - Random number (any u64 value)
    ///
    /// # Returns
    /// Word index sampled from the unigram distribution
    pub fn sample_negative(&self, rng_val: u64) -> usize {
        if self.unigram_table.is_empty() {
            return 0;
        }
        let idx = (rng_val as usize) % self.unigram_table.len();
        self.unigram_table[idx]
    }

    /// Build unigram table for negative sampling
    ///
    /// Creates a table where each word appears proportionally to P(w)^0.75
    /// This "smooths" the distribution to give rare words more samples
    fn build_unigram_table(&mut self) {
        if self.entries.is_empty() {
            return;
        }

        self.unigram_table = vec![0; Self::UNIGRAM_TABLE_SIZE];

        // Calculate normalization factor
        let norm: f64 = self
            .entries
            .iter()
            .map(|e| (e.count as f64).powf(Self::UNIGRAM_POWER))
            .sum();

        if norm <= 0.0 {
            return;
        }

        // Fill table using cumulative distribution
        let mut idx = 0;
        let mut cumulative = (self.entries[0].count as f64).powf(Self::UNIGRAM_POWER) / norm;

        for i in 0..Self::UNIGRAM_TABLE_SIZE {
            self.unigram_table[i] = idx;
            let progress = (i as f64) / (Self::UNIGRAM_TABLE_SIZE as f64);

            while progress > cumulative && idx + 1 < self.entries.len() {
                idx += 1;
                cumulative += (self.entries[idx].count as f64).powf(Self::UNIGRAM_POWER) / norm;
            }
        }
    }

    /// Compute discard probabilities for subsampling
    ///
    /// Frequent words are randomly discarded during training to balance learning
    /// Formula: P(discard) = 1 - sqrt(threshold / frequency)
    fn compute_discard_probabilities(&mut self) {
        self.discard_prob = Vec::with_capacity(self.entries.len());

        for entry in &self.entries {
            let freq = (entry.count as f64) / (self.train_words as f64);
            let prob = 1.0 - (Self::SUBSAMPLE_THRESHOLD / freq).sqrt();
            self.discard_prob.push(prob.max(0.0));
        }
    }

    /// Get all words in the vocabulary
    pub fn words(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.word.clone()).collect()
    }

    /// Get all counts in the vocabulary
    pub fn counts(&self) -> Vec<u64> {
        self.entries.iter().map(|e| e.count).collect()
    }
}

impl Default for Vocabulary {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let text = "Hello World! This is a TEST.";
        let tokens = Vocabulary::tokenize(text);
        assert_eq!(tokens, vec!["hello", "world!", "this", "is", "a", "test."]);
    }

    #[test]
    fn test_tokenize_whitespace() {
        let text = "  spaces\t\ttabs\n\nnewlines  ";
        let tokens = Vocabulary::tokenize(text);
        assert_eq!(tokens, vec!["spaces", "tabs", "newlines"]);
    }

    #[test]
    fn test_build_simple() {
        let corpus = vec![
            "the cat sat on the mat".to_string(),
            "the dog sat on the log".to_string(),
        ];

        let mut vocab = Vocabulary::new();
        vocab.build(&corpus, 1);

        // "the" is a stop word, so should be filtered out
        // Remaining words: cat, sat, on, mat, dog, log
        assert!(vocab.size() > 0);

        // Check that stop words are not in vocab
        assert_eq!(vocab.lookup("the"), None);
    }

    #[test]
    fn test_build_min_count() {
        let corpus = vec![
            "apple banana cherry".to_string(),
            "apple banana date".to_string(),
            "apple elderberry fig".to_string(),
        ];

        let mut vocab = Vocabulary::new();
        vocab.build(&corpus, 2); // min_count = 2

        // apple appears 3 times, banana appears 2 times
        // Others appear only once, so should be filtered
        assert!(vocab.lookup("apple").is_some());
        assert!(vocab.lookup("banana").is_some());
        assert_eq!(vocab.lookup("cherry"), None);
        assert_eq!(vocab.lookup("date"), None);
    }

    #[test]
    fn test_frequency_ordering() {
        let corpus = vec![
            "alpha alpha alpha beta beta gamma".to_string(),
        ];

        let mut vocab = Vocabulary::new();
        vocab.build(&corpus, 1);

        // Should be ordered by frequency: alpha (3), beta (2), gamma (1)
        if let Some(idx_alpha) = vocab.lookup("alpha") {
            assert_eq!(vocab.count_at(idx_alpha), 3);
        }
    }

    #[test]
    fn test_doc_to_indices() {
        let corpus = vec![
            "machine learning deep learning".to_string(),
        ];

        let mut vocab = Vocabulary::new();
        vocab.build(&corpus, 1);

        let indices = vocab.doc_to_indices("deep learning");
        assert_eq!(indices.len(), 2);
    }

    #[test]
    fn test_restore() {
        let words = vec!["apple".to_string(), "banana".to_string(), "cherry".to_string()];
        let counts = vec![10, 5, 3];

        let mut vocab = Vocabulary::new();
        vocab.restore(&words, &counts);

        assert_eq!(vocab.size(), 3);
        assert_eq!(vocab.train_words(), 18);
        assert_eq!(vocab.lookup("apple"), Some(0));
        assert_eq!(vocab.count_at(0), 10);
    }

    #[test]
    fn test_discard_probability() {
        let words = vec!["rare".to_string(), "common".to_string()];
        let counts = vec![1, 1000];

        let mut vocab = Vocabulary::new();
        vocab.restore(&words, &counts);

        let rare_idx = vocab.lookup("rare").unwrap();
        let common_idx = vocab.lookup("common").unwrap();

        // Common words should have higher discard probability
        assert!(vocab.discard_probability(common_idx) > vocab.discard_probability(rare_idx));
    }

    #[test]
    fn test_sample_negative() {
        let words = vec!["word1".to_string(), "word2".to_string(), "word3".to_string()];
        let counts = vec![100, 50, 25];

        let mut vocab = Vocabulary::new();
        vocab.restore(&words, &counts);

        // Just test that it doesn't panic and returns valid indices
        for i in 0..10 {
            let idx = vocab.sample_negative(i);
            assert!(idx < vocab.size());
        }
    }

    #[test]
    fn test_empty_vocab() {
        let vocab = Vocabulary::new();
        assert_eq!(vocab.size(), 0);
        assert_eq!(vocab.total_words(), 0);
        assert_eq!(vocab.train_words(), 0);
    }
}
