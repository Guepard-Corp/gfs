# Vocabulary Module - Technical Details

## Overview

The Vocabulary module is a core component for Word2Vec training, responsible for:
- Building word frequency dictionaries from text corpora
- Tokenizing and normalizing text
- Filtering stop words and low-frequency words
- Creating sampling distributions for training

## Architecture

### Data Structures

```rust
pub struct WordEntry {
    pub word: String,      // The word itself
    pub count: u64,        // Frequency in corpus
    pub index: usize,      // Position in vocabulary
}

pub struct Vocabulary {
    entries: Vec<WordEntry>,              // Sorted by frequency
    word_to_index: HashMap<String, usize>, // Fast lookup
    unigram_table: Vec<usize>,            // For negative sampling
    discard_prob: Vec<f64>,               // For subsampling
    total_words: u64,                     // Total words in corpus
    train_words: u64,                     // Words kept for training
}
```

## Key Algorithms

### 1. Building the Vocabulary

**Process**:
1. Tokenize all documents in the corpus
2. Count word frequencies (excluding stop words)
3. Filter words below `min_count` threshold
4. Sort by frequency (descending)
5. Re-index and build lookup table
6. Build unigram table for negative sampling
7. Compute discard probabilities for subsampling

**Time Complexity**: O(n log n) where n is vocabulary size
**Space Complexity**: O(n + m) where m is total words in corpus

### 2. Negative Sampling Distribution

Word2Vec uses negative sampling to make training efficient. Instead of updating all words in the vocabulary, we sample a few "negative" words.

**Distribution**: P(w)^0.75 / Z

This "smooths" the distribution:
- Rare words get sampled more than their frequency suggests
- Common words get sampled less than their frequency suggests

**Implementation**:
- Pre-compute a table of 10M entries
- Each entry contains a word index
- Words appear proportionally to their smoothed probability
- Sampling is O(1) using `rng_val % table_size`

**Example**:
```
Word      Count   P(w)      P(w)^0.75   Entries in table
----      -----   ----      ---------   ----------------
"the"     1000    0.50      0.42        ~4,200,000
"cat"     500     0.25      0.25        ~2,500,000
"rare"    10      0.005     0.013       ~130,000
```

### 3. Subsampling Frequent Words

Very common words (like "the", "is") provide less information. We randomly discard them during training.

**Formula**: P(discard) = 1 - sqrt(threshold / frequency)

Where:
- `threshold` = 1e-5 (default)
- `frequency` = word_count / total_words

**Example**:
```
Word      Count   Frequency    P(discard)
----      -----   ---------    ----------
"the"     1000    0.50         0.996  (discard ~99.6%)
"cat"     50      0.025        0.98   (discard ~98%)
"rare"    1       0.0005       0.0    (never discard)
```

This balances the training data and improves quality.

### 4. Tokenization

**Simple whitespace tokenization**:
- Split on: space, tab, newline, carriage return
- Convert to lowercase
- No punctuation removal (words include punctuation)

**Example**:
```
Input:  "Hello, World! How are you?"
Output: ["hello,", "world!", "how", "are", "you?"]
```

For more advanced tokenization (BPE, WordPiece), extend the `tokenize()` method.

## Performance Characteristics

### Memory Usage

| Component | Size | Notes |
|-----------|------|-------|
| Vocabulary entries | ~40 bytes/word | String + u64 + usize |
| Word to index map | ~50 bytes/word | HashMap overhead |
| Unigram table | 40 MB | 10M × 4 bytes |
| Discard probabilities | 8 bytes/word | f64 per word |
| **Total** | **~100 bytes/word + 40 MB** | For 100K words: ~50 MB |

### Time Complexity

| Operation | Complexity | Notes |
|-----------|-----------|-------|
| `build()` | O(D·T + V log V) | D=docs, T=tokens/doc, V=vocab size |
| `tokenize()` | O(n) | n=text length |
| `doc_to_indices()` | O(t) | t=tokens in doc |
| `lookup()` | O(1) average | HashMap lookup |
| `sample_negative()` | O(1) | Array index |
| `discard_probability()` | O(1) | Array lookup |

## Constants

```rust
// Unigram table size (10 million entries)
const UNIGRAM_TABLE_SIZE: usize = 10_000_000;

// Power for negative sampling distribution
const UNIGRAM_POWER: f64 = 0.75;

// Threshold for subsampling computation
const SUBSAMPLE_THRESHOLD: f64 = 1e-5;
```

These are based on the original Word2Vec paper by Mikolov et al.

## Usage Patterns

### Building from Corpus

```rust
let corpus = vec![
    "natural language processing".to_string(),
    "machine learning algorithms".to_string(),
    "deep learning neural networks".to_string(),
];

let mut vocab = Vocabulary::new();
vocab.build(&corpus, 1); // min_count = 1

println!("Vocabulary size: {}", vocab.size());
println!("Total words: {}", vocab.total_words());
```

### Saving and Restoring

```rust
// Save
let words = vocab.words();
let counts = vocab.counts();
// Serialize to file...

// Restore
let mut vocab = Vocabulary::new();
vocab.restore(&words, &counts);
```

### Training Loop Integration

```rust
let indices = vocab.doc_to_indices("machine learning is amazing");

for &word_idx in &indices {
    // Check if we should subsample this word
    let rand_val: f64 = random();
    if rand_val < vocab.discard_probability(word_idx) {
        continue; // Skip this word
    }

    // Sample negative examples
    let mut negatives = Vec::new();
    for _ in 0..5 {
        let neg_idx = vocab.sample_negative(random());
        negatives.push(neg_idx);
    }

    // Train with word_idx and negatives...
}
```

## Testing

The module includes comprehensive tests:

1. **Tokenization Tests**: Verify text splitting and lowercasing
2. **Building Tests**: Verify corpus processing and filtering
3. **Frequency Tests**: Verify proper sorting by count
4. **Min Count Tests**: Verify threshold filtering
5. **Lookup Tests**: Verify word-to-index mapping
6. **Sampling Tests**: Verify negative sampling works
7. **Subsampling Tests**: Verify discard probabilities
8. **Restore Tests**: Verify save/restore functionality
9. **Edge Cases**: Empty vocabulary, single word, etc.

Run tests with:
```bash
cargo test -p gfs-ai-models vocab
```

## References

1. **Word2Vec Paper**: Mikolov et al. "Efficient Estimation of Word Representations in Vector Space" (2013)
2. **Negative Sampling**: Mikolov et al. "Distributed Representations of Words and Phrases and their Compositionality" (2013)
3. **Original Implementation**: Google's word2vec C code
4. **DuckDB Implementation**: Extension/semantic_search/core/vocab.cpp
