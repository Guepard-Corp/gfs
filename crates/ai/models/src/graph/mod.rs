//===----------------------------------------------------------------------===//
//                         GFS
//
// graph/mod.rs - Graph Module
//
// Graph-aware semantic search combining text embeddings (Word2Vec) with
// graph-structural embeddings (DeepWalk random walks) in a single model.
//===----------------------------------------------------------------------===//

pub mod data_graph;
pub mod walker;
pub mod engine;

// Re-export commonly used items
pub use data_graph::{DataGraph, DataGraphBuilder, DataNode};
pub use walker::{RandomWalker, WalkerConfig};
pub use engine::{
    GraphSemanticEngine,
    GraphEngineConfig,
    GraphSearchResult,
    GraphError,
};
