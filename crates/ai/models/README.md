# GFS AI Models

This crate contains AI models and utilities for semantic search functionality in GFS.

## Project Structure

```
crates/ai/models/src/
├── core/                          # Core components
│   ├── mod.rs                     # Module exports
│   ├── stop_words.rs              # Stop words filtering (✅ 2.8KB)
│   ├── stop_words.txt             # Stop words data (733 words)
│   ├── vocab.rs                   # Vocabulary builder (✅ 15KB)
│   └── word2vec.rs                # Word2Vec training (✅ 24KB)
├── hnsw/                          # HNSW index module
│   ├── mod.rs                     # Module exports
│   └── index.rs                   # HNSW implementation (✅ 20KB)
├── semantic_search_engine.rs      # High-level API (to be implemented)
└── lib.rs                         # Public API
```

## Stop Words Module

Migrated from DuckDB's `extension/semantic_search/core/stop_words.cpp` to Rust.

**Location**: `src/core/stop_words.rs`

### Key Differences from C++ Implementation

**C++ Version:**
- Uses `std::unordered_set<std::string>` with static initialization
- Includes stop words from a separate `.inc` file
- Two functions: `IsStopWord()` and `GetStopWords()`

**Rust Version:**
- Uses `std::collections::HashSet<&'static str>` with `OnceLock` for lazy initialization
- Embeds stop words directly from `stop_words.txt` using `include_str!` macro
- Two functions: `is_stop_word()` and `get_stop_words()` (following Rust naming conventions)
- Thread-safe initialization using `OnceLock` (no need for mutexes)
- Zero-cost abstraction: initialized once on first use

### Usage Example

```rust
use gfs_ai_models::{is_stop_word, get_stop_words, Vocabulary};

// Check if a word is a stop word
if is_stop_word("the") {
    println!("This is a stop word");
}

// Get all stop words for filtering
let stop_words = get_stop_words();
let filtered: Vec<_> = words
    .into_iter()
    .filter(|w| !stop_words.contains(w.as_str()))
    .collect();

// Build a vocabulary from a corpus
let corpus = vec![
    "Hello world from Rust".to_string(),
    "Machine learning with Word2Vec".to_string(),
];

let mut vocab = Vocabulary::new();
vocab.build(&corpus, 1); // min_count = 1

// Look up words
if let Some(idx) = vocab.lookup("rust") {
    println!("Word 'rust' has index {} and count {}", idx, vocab.count_at(idx));
}

// Convert document to indices
let indices = vocab.doc_to_indices("Hello Rust world");
println!("Document as indices: {:?}", indices);
```

## Vocabulary Module

Migrated from DuckDB's `extension/semantic_search/core/vocab.cpp` to Rust.

**Location**: `src/core/vocab.rs`

### Key Differences from C++ Implementation

**C++ Version:**
- Uses `std::vector` and `std::unordered_map` for storage
- Manual memory management
- Stateful class with mutable methods

**Rust Version:**
- Uses `Vec` and `HashMap` with owned `String` keys
- Automatic memory management with ownership rules
- Idiomatic Rust API with clear ownership semantics
- Comprehensive unit tests (10+ test cases)

### Vocabulary Features

1. **Building**: Create vocabulary from corpus with min_count threshold
2. **Tokenization**: Split text into lowercase words
3. **Filtering**: Automatically filters stop words during building
4. **Frequency Ordering**: Words sorted by frequency (most common first)
5. **Negative Sampling**: Unigram table with P(w)^0.75 distribution
6. **Subsampling**: Discard probabilities for frequent words
7. **Serialization**: Save/restore vocabulary state

### API Comparison

| C++ Function | Rust Function | Notes |
|-------------|---------------|-------|
| `build(corpus, min_count)` | `build(&[String], u64)` | Build from corpus |
| `restore(words, counts)` | `restore(&[String], &[u64])` | Restore from saved state |
| `tokenize(text)` | `tokenize(&str)` | Static method in Rust |
| `doc_to_indices(text)` | `doc_to_indices(&str)` | Convert doc to indices |
| `lookup(word)` | `lookup(&str)` | Returns `Option<usize>` |
| `word_at(idx)` | `word_at(usize)` | Returns `&str` |
| `count_at(idx)` | `count_at(usize)` | Returns `u64` |
| `size()` | `size()` | Vocabulary size |
| `sample_negative(rng)` | `sample_negative(u64)` | Negative sampling |
| `discard_probability(idx)` | `discard_probability(usize)` | Subsampling prob |

## Word2Vec Module

Migrated from DuckDB's `extension/semantic_search/core/word2vec.cpp` to Rust.

**Location**: `src/core/word2vec.rs`

### Key Differences from C++ Implementation

**C++ Version:**
- Uses `std::thread` for multi-threading
- Raw pointers for embedding matrices
- Manual memory management
- Nullable pointers for error handling

**Rust Version:**
- Uses scoped threads with SendPtr wrapper for safety
- Owned `Vec<f32>` for embeddings
- Automatic memory management
- `Option<&[f32]>` and `io::Result` for error handling
- Comprehensive unit tests (8 test cases)

### Word2Vec Features

1. **Training Algorithms**: Skip-gram and CBOW architectures
2. **Multi-threading**: Parallel training with scoped threads
3. **Negative Sampling**: Unigram distribution sampling
4. **Subsampling**: Frequent word subsampling
5. **Learning Rate Decay**: Progress-based decay
6. **Serialization**: Binary save/load with byteorder
7. **Fast RNG**: Xorshift128 for performance
8. **Text Embedding**: Average + normalize word vectors

### API Comparison

| C++ Function | Rust Function | Notes |
|-------------|---------------|-------|
| `train(corpus)` | `train(&[String])` | Builds vocab + trains |
| `embed_text(text)` | `embed_text(&str) -> Vec<f32>` | Returns owned vector |
| `embedding(idx)` | `embedding(usize) -> Option<&[f32]>` | Safe access |
| `save(path)` | `save(&str) -> io::Result<()>` | Error handling |
| `load(path)` | `load(&str) -> io::Result<()>` | Error handling |

### Usage Example

```rust
use gfs_ai_models::{Word2Vec, Word2VecConfig, W2VArchitecture};

// Configure model
let mut cfg = Word2VecConfig::default();
cfg.dimension = 50;
cfg.epochs = 5;
cfg.arch = W2VArchitecture::SkipGram;

// Train model
let corpus = vec![
    "machine learning is amazing".to_string(),
    "deep learning neural networks".to_string(),
];

let mut model = Word2Vec::with_config(cfg);
model.train(&corpus);

// Embed text
let embedding = model.embed_text("machine learning");
println!("Embedding dimension: {}", embedding.len());

// Save model
model.save("/tmp/model.bin").unwrap();

// Load model
let mut loaded = Word2Vec::new();
loaded.load("/tmp/model.bin").unwrap();
```

## Features

- **Stop Words**: ✅ Embedded stopword list for vocabulary building and OOV detection
- **Vocabulary**: ✅ Word frequency counting, tokenization, and sampling for Word2Vec training
- **Word2Vec**: ✅ Skip-gram & CBOW training with multi-threading and serialization
- **HNSW Index**: ✅ Fast approximate nearest neighbor search with multiple distance metrics
- **Semantic Search Engine**: 🔲 High-level API (to be implemented)

## Testing

Run tests with:
```bash
cargo test -p gfs-ai-models
```
