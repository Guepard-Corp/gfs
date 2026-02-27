//===----------------------------------------------------------------------===//
//                         GFS
//
// hnsw/mod.rs - HNSW Module
//
// Hierarchical Navigable Small World graphs for ANN search
//===----------------------------------------------------------------------===//

pub mod index;

// Re-export commonly used items
pub use index::{HNSWIndex, HNSWConfig, DistanceMetric};
