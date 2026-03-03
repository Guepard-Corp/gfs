//===----------------------------------------------------------------------===//
//                         GFS
//
// lib.rs - AI Models Library
//
// Contains AI models and utilities for semantic search
//===----------------------------------------------------------------------===//

pub mod core;
pub mod features;
pub mod hnsw;
pub mod similarity;
pub mod semantic_search_engine;
pub mod graph;
pub mod training;

// Re-export commonly used items from core
pub use core::{
    is_stop_word, get_stop_words,
    Vocabulary, WordEntry,
    Word2Vec, Word2VecConfig, W2VArchitecture,
};

// Re-export HNSW items
pub use hnsw::{HNSWIndex, HNSWConfig, DistanceMetric};

// Re-export similarity functions
pub use similarity::{
    cosine_distance, cosine_similarity,
    l2_distance, l2_squared,
    inner_product,
    is_similar,
};

// Re-export semantic search engine
pub use semantic_search_engine::{
    SemanticSearchEngine,
    SemanticSearchError,
    SearchResult,
    ModelInfo,
};

// Re-export graph engine
pub use graph::{
    DataGraph, DataGraphBuilder, DataNode,
    RandomWalker, WalkerConfig,
    GraphSemanticEngine, GraphEngineConfig, GraphSearchResult, GraphError,
};
