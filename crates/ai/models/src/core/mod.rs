//===----------------------------------------------------------------------===//
//                         GFS
//
// core/mod.rs - Core AI Models
//
// Core components for semantic search and Word2Vec
//===----------------------------------------------------------------------===//

pub mod backend;
pub mod stop_words;
pub mod vocab;
pub mod word2vec;

// Re-export commonly used items
pub use backend::{BackendConfig, ComputeBackend, create_backend};
pub use stop_words::{get_stop_words, is_stop_word};
pub use vocab::{Vocabulary, WordEntry};
pub use word2vec::{Word2Vec, Word2VecConfig, W2VArchitecture};
