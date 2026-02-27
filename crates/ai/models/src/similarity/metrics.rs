//===----------------------------------------------------------------------===//
//                         GFS
//
// similarity/metrics.rs - Vector Distance Metrics
//
// Migrated from DuckDB's extension/semantic_search/similarity/similarity.cpp
//
// Core distance and similarity functions for semantic search.
// All functions work on f32 slices (matching the embedding type used throughout
// the codebase) and return f64 for precision, matching the C++ double output.
//===----------------------------------------------------------------------===//

/// Cosine distance between two vectors: `1 - cosine_similarity(a, b)`.
///
/// Returns a value in `[0.0, 2.0]` where `0.0` means identical direction.
/// Returns `1.0` if either vector is the zero vector.
///
/// Equivalent to C++ `CosineDistanceFromBuffers` in `similarity.cpp`.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "cosine_distance: dimension mismatch");
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let xd = x as f64;
        let yd = y as f64;
        dot += xd * yd;
        na += xd * xd;
        nb += yd * yd;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        return 1.0;
    }
    1.0 - dot / denom
}

/// Cosine similarity between two vectors: `dot(a, b) / (|a| * |b|)`.
///
/// Returns a value in `[-1.0, 1.0]` where `1.0` means identical direction.
/// Returns `0.0` if either vector is the zero vector.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    1.0 - cosine_distance(a, b)
}

/// L2 (Euclidean) distance between two vectors: `sqrt(sum((a_i - b_i)^2))`.
pub fn l2_distance(a: &[f32], b: &[f32]) -> f64 {
    l2_squared(a, b).sqrt()
}

/// L2 squared distance: `sum((a_i - b_i)^2)`.
///
/// Cheaper than `l2_distance` when only ranking by distance (avoids sqrt).
/// Equivalent to C++ `dist::l2_squared` in `hnsw.hpp`.
pub fn l2_squared(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "l2_squared: dimension mismatch");
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = (x - y) as f64;
            d * d
        })
        .sum()
}

/// Inner product (dot product): `sum(a_i * b_i)`.
pub fn inner_product(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "inner_product: dimension mismatch");
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| x as f64 * y as f64)
        .sum()
}

/// Returns `true` if `cosine_distance(a, b) <= max_distance`.
///
/// Equivalent to C++ `SemanticSearchFunction` in `similarity.cpp`.
/// Use in filter passes: keeps rows whose embedding is within `max_distance`
/// of the query embedding (in cosine-distance space).
pub fn is_similar(a: &[f32], b: &[f32], max_distance: f64) -> bool {
    cosine_distance(a, b) <= max_distance
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── cosine_distance / cosine_similarity ──────────────────────────────────

    #[test]
    fn test_cosine_identical_vectors() {
        let v = vec![1.0f32, 2.0, 3.0];
        let dist = cosine_distance(&v, &v);
        assert!(dist.abs() < 1e-6, "identical vectors should have distance ~0, got {dist}");
    }

    #[test]
    fn test_cosine_orthogonal_vectors() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let dist = cosine_distance(&a, &b);
        assert!((dist - 1.0).abs() < 1e-6, "orthogonal vectors should have distance ~1, got {dist}");
    }

    #[test]
    fn test_cosine_opposite_vectors() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![-1.0f32, 0.0, 0.0];
        let dist = cosine_distance(&a, &b);
        assert!((dist - 2.0).abs() < 1e-6, "opposite vectors should have distance ~2, got {dist}");
    }

    #[test]
    fn test_cosine_zero_vector() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 0.0, 0.0];
        let dist = cosine_distance(&a, &b);
        assert_eq!(dist, 1.0, "zero vector should return distance 1.0");
    }

    #[test]
    fn test_cosine_45_degrees() {
        // Two vectors 45° apart: cos(45°) ≈ 0.7071, distance ≈ 1 - 0.7071 ≈ 0.2929
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 1.0];
        let dist = cosine_distance(&a, &b);
        let expected = 1.0 - (1.0f64 / 2.0f64.sqrt());
        assert!((dist - expected).abs() < 1e-5, "45° distance expected {expected}, got {dist}");
    }

    #[test]
    fn test_cosine_similarity_is_complement() {
        let a = vec![3.0f32, 1.0, 2.0];
        let b = vec![1.0f32, 4.0, 2.0];
        let dist = cosine_distance(&a, &b);
        let sim = cosine_similarity(&a, &b);
        assert!((dist + sim - 1.0).abs() < 1e-10, "distance + similarity should equal 1");
    }

    #[test]
    fn test_cosine_scale_invariant() {
        // cosine distance is scale-invariant
        let a = vec![1.0f32, 2.0, 3.0];
        let b = vec![2.0f32, 4.0, 6.0]; // = 2 * a
        let dist = cosine_distance(&a, &b);
        assert!(dist.abs() < 1e-6, "parallel vectors (different scale) should have distance ~0, got {dist}");
    }

    // ── l2_squared / l2_distance ─────────────────────────────────────────────

    #[test]
    fn test_l2_squared_identical() {
        let v = vec![1.0f32, 2.0, 3.0];
        let d = l2_squared(&v, &v);
        assert!(d.abs() < 1e-10, "identical vectors should have l2_squared = 0, got {d}");
    }

    #[test]
    fn test_l2_squared_known_value() {
        // distance between [0,0,0] and [1,2,2] = sqrt(1+4+4) = 3, squared = 9
        let a = vec![0.0f32; 3];
        let b = vec![1.0f32, 2.0, 2.0];
        let d = l2_squared(&a, &b);
        assert!((d - 9.0).abs() < 1e-6, "expected l2_squared = 9, got {d}");
    }

    #[test]
    fn test_l2_distance_known_value() {
        let a = vec![0.0f32; 3];
        let b = vec![1.0f32, 2.0, 2.0];
        let d = l2_distance(&a, &b);
        assert!((d - 3.0).abs() < 1e-6, "expected l2 = 3, got {d}");
    }

    // ── inner_product ────────────────────────────────────────────────────────

    #[test]
    fn test_inner_product_orthogonal() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let ip = inner_product(&a, &b);
        assert!(ip.abs() < 1e-10, "orthogonal vectors should have inner_product = 0, got {ip}");
    }

    #[test]
    fn test_inner_product_known_value() {
        let a = vec![1.0f32, 2.0, 3.0];
        let b = vec![4.0f32, 5.0, 6.0];
        let ip = inner_product(&a, &b);
        // 1*4 + 2*5 + 3*6 = 4 + 10 + 18 = 32
        assert!((ip - 32.0).abs() < 1e-6, "expected inner_product = 32, got {ip}");
    }

    // ── is_similar ───────────────────────────────────────────────────────────

    #[test]
    fn test_is_similar_identical() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!(is_similar(&v, &v, 0.0), "identical vectors should be similar at threshold 0");
    }

    #[test]
    fn test_is_similar_orthogonal_excluded() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!(!is_similar(&a, &b, 0.5), "orthogonal vectors (dist=1) should not be similar at threshold 0.5");
    }

    #[test]
    fn test_is_similar_orthogonal_included() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!(is_similar(&a, &b, 1.0), "orthogonal vectors (dist=1) should be similar at threshold 1.0");
    }

    #[test]
    fn test_is_similar_threshold_boundary() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let dist = cosine_distance(&a, &b);
        assert!(is_similar(&a, &b, dist), "should be similar at exactly the distance threshold");
    }
}
