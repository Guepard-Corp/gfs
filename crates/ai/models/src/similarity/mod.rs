//===----------------------------------------------------------------------===//
//                         GFS
//
// similarity/mod.rs - Similarity Module
//
// Vector distance and similarity metrics for semantic search
//===----------------------------------------------------------------------===//

pub mod metrics;

// Re-export commonly used items
pub use metrics::{
    cosine_distance, cosine_similarity,
    l2_distance, l2_squared,
    inner_product,
    is_similar,
};
