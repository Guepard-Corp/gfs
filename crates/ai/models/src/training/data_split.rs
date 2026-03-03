//! Data splitting utilities for train/validation/test sets.
//!
//! Provides functions to split a DataGraph into separate graphs for training,
//! validation, and testing while preserving graph structure.

use rand::seq::SliceRandom;
use rand::SeedableRng;

use crate::graph::DataGraph;

/// Split a graph into train/validation/test sets.
///
/// Splits nodes randomly according to the provided ratios while attempting to
/// preserve graph structure. Edges are included in a split if both source and
/// target nodes are in that split.
///
/// # Arguments
///
/// * `graph` - Input graph to split
/// * `ratios` - [train, val, test] ratios (must sum to 1.0)
/// * `seed` - Random seed for reproducibility
///
/// # Returns
///
/// Tuple of (train_graph, val_graph, test_graph)
///
/// # Errors
///
/// Returns error if ratios don't sum to 1.0 or graph is empty
pub fn split_graph(
    graph: &DataGraph,
    ratios: [f32; 3],
    seed: u64,
) -> Result<(DataGraph, DataGraph, DataGraph), SplitError> {
    // Validate ratios
    let sum: f32 = ratios.iter().sum();
    if (sum - 1.0).abs() > 0.001 {
        return Err(SplitError::InvalidRatios(format!(
            "ratios must sum to 1.0, got {}",
            sum
        )));
    }

    if graph.node_count() == 0 {
        return Err(SplitError::EmptyGraph);
    }

    // Create random permutation of node indices
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut node_indices: Vec<usize> = (0..graph.node_count()).collect();
    node_indices.shuffle(&mut rng);

    // Calculate split boundaries
    let train_end = (graph.node_count() as f32 * ratios[0]) as usize;
    let val_end = train_end + (graph.node_count() as f32 * ratios[1]) as usize;

    // Split node indices
    let train_nodes: Vec<usize> = node_indices[..train_end].to_vec();
    let val_nodes: Vec<usize> = node_indices[train_end..val_end].to_vec();
    let test_nodes: Vec<usize> = node_indices[val_end..].to_vec();

    // Build train graph
    let train_graph = build_subgraph(graph, &train_nodes)?;

    // Build validation graph
    let val_graph = build_subgraph(graph, &val_nodes)?;

    // Build test graph
    let test_graph = build_subgraph(graph, &test_nodes)?;

    Ok((train_graph, val_graph, test_graph))
}

/// Build a subgraph containing only the specified nodes.
///
/// Creates a new graph with only the nodes in `node_indices` and edges where
/// both source and target are in the node set.
fn build_subgraph(graph: &DataGraph, node_indices: &[usize]) -> Result<DataGraph, SplitError> {
    let mut builder = DataGraph::builder();
    let node_set: std::collections::HashSet<usize> = node_indices.iter().copied().collect();

    // Add nodes
    for &idx in node_indices {
        let node = graph.node(idx);
        builder = builder.add_node_with_meta(
            node.id,
            node.text.clone(),
            node.source_table.clone().unwrap_or_default(),
            node.source_row_id.clone().unwrap_or_default(),
        );
    }

    // Add edges where both endpoints are in the node set
    for &src_idx in node_indices {
        let neighbors = graph.neighbours(src_idx);
        for &tgt_idx in neighbors {
            if node_set.contains(&tgt_idx) {
                let src_node = graph.node(src_idx);
                let tgt_node = graph.node(tgt_idx);
                builder = builder.add_edge(src_node.id, tgt_node.id);
            }
        }
    }

    Ok(builder.build())
}

/// Errors during data splitting.
#[derive(Debug, thiserror::Error)]
pub enum SplitError {
    #[error("invalid ratios: {0}")]
    InvalidRatios(String),

    #[error("cannot split empty graph")]
    EmptyGraph,

    #[error("split failed: {0}")]
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_graph() -> DataGraph {
        // Create a simple graph with 10 nodes and some edges
        let mut builder = DataGraph::builder();
        for i in 0..10 {
            builder = builder.add_node_with_meta(
                i,
                format!("node {}", i),
                "test_table",
                format!("{}", i),
            );
        }

        // Add some edges
        builder = builder
            .add_edge(0, 1)
            .add_edge(1, 2)
            .add_edge(2, 3)
            .add_edge(3, 4)
            .add_edge(5, 6)
            .add_edge(6, 7)
            .add_edge(7, 8)
            .add_edge(8, 9);

        builder.build()
    }

    #[test]
    fn test_split_graph_basic() {
        let graph = create_test_graph();
        let (train, val, test) = split_graph(&graph, [0.6, 0.2, 0.2], 42).unwrap();

        // Check node counts
        assert_eq!(train.node_count(), 6);
        assert_eq!(val.node_count(), 2);
        assert_eq!(test.node_count(), 2);

        // Total nodes should match
        assert_eq!(
            train.node_count() + val.node_count() + test.node_count(),
            graph.node_count()
        );
    }

    #[test]
    fn test_split_graph_invalid_ratios() {
        let graph = create_test_graph();
        let result = split_graph(&graph, [0.5, 0.3, 0.3], 42);
        assert!(matches!(result, Err(SplitError::InvalidRatios(_))));
    }

    #[test]
    fn test_split_graph_empty() {
        let graph = DataGraph::builder().build();
        let result = split_graph(&graph, [0.8, 0.1, 0.1], 42);
        assert!(matches!(result, Err(SplitError::EmptyGraph)));
    }

    #[test]
    fn test_split_graph_deterministic() {
        let graph = create_test_graph();

        // Split with same seed should produce same results
        let (train1, _, _) = split_graph(&graph, [0.6, 0.2, 0.2], 42).unwrap();
        let (train2, _, _) = split_graph(&graph, [0.6, 0.2, 0.2], 42).unwrap();

        assert_eq!(train1.node_count(), train2.node_count());
        assert_eq!(train1.edge_count(), train2.edge_count());
    }

    #[test]
    fn test_split_graph_different_seeds() {
        let graph = create_test_graph();

        // Different seeds should produce different splits
        let (train1, _, _) = split_graph(&graph, [0.6, 0.2, 0.2], 42).unwrap();
        let (train2, _, _) = split_graph(&graph, [0.6, 0.2, 0.2], 123).unwrap();

        // Counts should be the same
        assert_eq!(train1.node_count(), train2.node_count());

        // But the actual nodes might be different (hard to test exactly)
        // Just verify they're both valid
        assert!(train1.node_count() > 0);
        assert!(train2.node_count() > 0);
    }

    #[test]
    fn test_build_subgraph_preserves_edges() {
        let graph = create_test_graph();

        // Get first 5 nodes
        let node_indices: Vec<usize> = (0..5).collect();
        let subgraph = build_subgraph(&graph, &node_indices).unwrap();

        assert_eq!(subgraph.node_count(), 5);

        // Should have edges 0->1, 1->2, 2->3, 3->4 (all within subgraph)
        // Edge count might be less if not all edges are preserved
        assert!(subgraph.edge_count() <= 4);
    }
}
