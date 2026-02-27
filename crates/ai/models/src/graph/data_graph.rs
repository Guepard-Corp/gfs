//===----------------------------------------------------------------------===//
//                         GFS
//
// graph/data_graph.rs - Abstract Data Graph
//
// Database-agnostic graph representation for the GraphSemanticEngine.
// The caller (adapter / application layer) builds a DataGraph from whatever
// data source they have (DatasourceMetadata + DatasourceResultSet, CSV, etc.)
// and passes it to GraphSemanticEngine::train().
//===----------------------------------------------------------------------===//

use std::collections::HashMap;

// ── Node ────────────────────────────────────────────────────────────────────

/// A single node in the data graph.
///
/// Each node carries the text that will be embedded (e.g. the concatenated
/// values of the text columns for a given database row) and optional provenance
/// metadata so that search results can be traced back to the original source.
#[derive(Debug, Clone)]
pub struct DataNode {
    /// Sequential 0-based index assigned by [`DataGraphBuilder`].
    pub id: usize,
    /// Text content to embed (e.g. concatenated text columns of a DB row).
    pub text: String,
    /// Optional: source table / collection name.
    pub source_table: Option<String>,
    /// Optional: primary-key value of the originating row (as a string).
    pub source_row_id: Option<String>,
}

// ── Graph ────────────────────────────────────────────────────────────────────

/// Abstract relational graph: nodes with text content and directed edges.
///
/// Built via [`DataGraphBuilder`]; consumed by [`GraphSemanticEngine::train`].
///
/// [`GraphSemanticEngine::train`]: crate::graph::GraphSemanticEngine::train
pub struct DataGraph {
    pub(crate) nodes: Vec<DataNode>,
    /// `adjacency[idx]` = list of neighbour indices reachable from node `idx`.
    pub(crate) adjacency: Vec<Vec<usize>>,
    edge_count: usize,
}

impl DataGraph {
    /// Start building a new graph.
    pub fn builder() -> DataGraphBuilder {
        DataGraphBuilder::new()
    }

    /// Total number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Total number of directed edges.
    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    /// Access a node by its internal index (same as `node.id` for nodes added
    /// via the builder in order).
    ///
    /// # Panics
    /// If `idx >= node_count()`.
    pub fn node(&self, idx: usize) -> &DataNode {
        &self.nodes[idx]
    }

    /// Return the neighbour indices of node `idx`.
    ///
    /// # Panics
    /// If `idx >= node_count()`.
    pub fn neighbours(&self, idx: usize) -> &[usize] {
        &self.adjacency[idx]
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Fluent builder for [`DataGraph`].
///
/// # Example
/// ```rust
/// use gfs_ai_models::graph::DataGraph;
///
/// let graph = DataGraph::builder()
///     .add_node(0, "machine learning product")
///     .add_node(1, "neural network component")
///     .add_node(2, "cooking ingredient")
///     .add_edge(0, 1)   // product → component (FK relationship)
///     .build();
///
/// assert_eq!(graph.node_count(), 3);
/// assert_eq!(graph.edge_count(), 1);
/// ```
pub struct DataGraphBuilder {
    nodes: Vec<DataNode>,
    adjacency: Vec<Vec<usize>>,
    /// Maps caller-supplied external id → internal index in `nodes`.
    id_map: HashMap<usize, usize>,
    edge_count: usize,
}

impl DataGraphBuilder {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            adjacency: Vec::new(),
            id_map: HashMap::new(),
            edge_count: 0,
        }
    }

    /// Add a node with text content. `id` is a caller-supplied external
    /// identifier (e.g. a database row primary key cast to `usize`).
    ///
    /// If `id` is already registered the call is a no-op (first registration
    /// wins).
    pub fn add_node(mut self, id: usize, text: impl Into<String>) -> Self {
        if self.id_map.contains_key(&id) {
            return self;
        }
        let idx = self.nodes.len();
        self.id_map.insert(id, idx);
        self.nodes.push(DataNode {
            id,
            text: text.into(),
            source_table: None,
            source_row_id: None,
        });
        self.adjacency.push(Vec::new());
        self
    }

    /// Add a node with provenance metadata (source table and row identifier).
    ///
    /// Duplicate `id` registrations are ignored (first wins).
    pub fn add_node_with_meta(
        mut self,
        id: usize,
        text: impl Into<String>,
        source_table: impl Into<String>,
        source_row_id: impl Into<String>,
    ) -> Self {
        if self.id_map.contains_key(&id) {
            return self;
        }
        let idx = self.nodes.len();
        self.id_map.insert(id, idx);
        self.nodes.push(DataNode {
            id,
            text: text.into(),
            source_table: Some(source_table.into()),
            source_row_id: Some(source_row_id.into()),
        });
        self.adjacency.push(Vec::new());
        self
    }

    /// Add a directed edge `from_id → to_id`.
    ///
    /// If either id has not been registered with [`add_node`] /
    /// [`add_node_with_meta`], the edge is silently ignored.
    ///
    /// [`add_node`]: DataGraphBuilder::add_node
    /// [`add_node_with_meta`]: DataGraphBuilder::add_node_with_meta
    pub fn add_edge(mut self, from_id: usize, to_id: usize) -> Self {
        match (self.id_map.get(&from_id), self.id_map.get(&to_id)) {
            (Some(&from_idx), Some(&to_idx)) => {
                self.adjacency[from_idx].push(to_idx);
                self.edge_count += 1;
            }
            _ => {} // silently ignore edges to unregistered nodes
        }
        self
    }

    /// Consume the builder and produce the [`DataGraph`].
    pub fn build(self) -> DataGraph {
        DataGraph {
            nodes: self.nodes,
            adjacency: self.adjacency,
            edge_count: self.edge_count,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_empty() {
        let g = DataGraph::builder().build();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn test_builder_add_node() {
        let g = DataGraph::builder()
            .add_node(0, "hello world")
            .add_node(10, "machine learning")
            .build();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.node(0).text, "hello world");
        assert_eq!(g.node(1).text, "machine learning");
        // external ids preserved
        assert_eq!(g.node(0).id, 0);
        assert_eq!(g.node(1).id, 10);
    }

    #[test]
    fn test_builder_duplicate_node_ignored() {
        let g = DataGraph::builder()
            .add_node(5, "first")
            .add_node(5, "second") // duplicate — ignored
            .build();
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.node(0).text, "first");
    }

    #[test]
    fn test_builder_add_node_with_meta() {
        let g = DataGraph::builder()
            .add_node_with_meta(42, "product text", "products", "42")
            .build();
        assert_eq!(g.node(0).source_table.as_deref(), Some("products"));
        assert_eq!(g.node(0).source_row_id.as_deref(), Some("42"));
    }

    #[test]
    fn test_builder_add_edge() {
        let g = DataGraph::builder()
            .add_node(0, "a")
            .add_node(1, "b")
            .add_node(2, "c")
            .add_edge(0, 1)
            .add_edge(0, 2)
            .build();
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.neighbours(0), &[1usize, 2]);
        assert_eq!(g.neighbours(1), &[] as &[usize]);
    }

    #[test]
    fn test_builder_edge_unknown_ids_ignored() {
        let g = DataGraph::builder()
            .add_node(0, "a")
            .add_edge(0, 99)  // 99 not registered
            .add_edge(99, 0)  // 99 not registered
            .build();
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.neighbours(0), &[] as &[usize]);
    }

    #[test]
    fn test_builder_edge_directionality() {
        // Edge 0→1 does NOT create reverse edge 1→0
        let g = DataGraph::builder()
            .add_node(0, "a")
            .add_node(1, "b")
            .add_edge(0, 1)
            .build();
        assert_eq!(g.neighbours(0), &[1usize]);
        assert_eq!(g.neighbours(1), &[] as &[usize]);
    }
}
