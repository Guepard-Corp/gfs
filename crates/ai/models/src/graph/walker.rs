//===----------------------------------------------------------------------===//
//                         GFS
//
// graph/walker.rs - DeepWalk Random Walker
//
// Generates random-walk sequences over a DataGraph for use as a Word2Vec
// training corpus.  Each walk is serialised as a whitespace-separated string
// of `__node_{id}` tokens so that the node IDs occupy a distinct token space
// from natural-language words.
//
// Reference: Perozzi et al., "DeepWalk: Online Learning of Social
//            Representations", KDD 2014.
//===----------------------------------------------------------------------===//

use super::data_graph::DataGraph;

/// Configuration for the random walker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WalkerConfig {
    /// Number of random walks to start from each node. Default: 10.
    pub num_walks: usize,
    /// Number of steps (hops) per walk. Default: 20.
    pub walk_length: usize,
    /// Seed for the PRNG. Default: 42.
    pub seed: u64,
}

impl Default for WalkerConfig {
    fn default() -> Self {
        Self {
            num_walks: 10,
            walk_length: 20,
            seed: 42,
        }
    }
}

/// DeepWalk random walker.
///
/// For each node in the graph, generates `num_walks` random walks of
/// `walk_length` steps.  Each walk is emitted as a single `String` of
/// `__node_{id}` tokens separated by spaces, ready to be appended to a
/// Word2Vec training corpus.
///
/// Nodes with no outgoing edges produce walks that stay at the starting node
/// for all remaining steps (self-loop fallback), ensuring every node appears
/// in the walk corpus regardless of graph connectivity.
pub struct RandomWalker {
    cfg: WalkerConfig,
}

impl RandomWalker {
    pub fn new(cfg: WalkerConfig) -> Self {
        Self { cfg }
    }

    /// Generate all random-walk sentences for `graph`.
    ///
    /// Returns `graph.node_count() × num_walks` strings, one per walk.
    /// Each string contains `walk_length` whitespace-separated tokens of the
    /// form `__node_{external_id}`.
    pub fn generate_walks(&self, graph: &DataGraph) -> Vec<String> {
        if graph.node_count() == 0 {
            return Vec::new();
        }

        let total = graph.node_count() * self.cfg.num_walks;
        let mut walks = Vec::with_capacity(total);

        // Xorshift64 — fast, deterministic, no external dep
        let mut rng = Xorshift64::new(self.cfg.seed);

        for start_idx in 0..graph.node_count() {
            for _ in 0..self.cfg.num_walks {
                let walk = self.single_walk(graph, start_idx, &mut rng);
                walks.push(walk);
            }
        }

        walks
    }

    // ── Private ───────────────────────────────────────────────────────────────

    fn single_walk(&self, graph: &DataGraph, start: usize, rng: &mut Xorshift64) -> String {
        let mut tokens = Vec::with_capacity(self.cfg.walk_length);
        let mut current = start;

        for _ in 0..self.cfg.walk_length {
            tokens.push(format!("__node_{}", graph.node(current).id));

            let neighbours = graph.neighbours(current);
            if neighbours.is_empty() {
                // No outgoing edges: stay at current node (self-loop fallback)
            } else {
                let pick = rng.next_usize() % neighbours.len();
                current = neighbours[pick];
            }
        }

        tokens.join(" ")
    }
}

// ── Fast PRNG (no external dep) ───────────────────────────────────────────────

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        // Ensure non-zero state (xorshift64 requires non-zero)
        Self {
            state: if seed == 0 { 0xdeadbeef_cafebabe } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self) -> usize {
        self.next_u64() as usize
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::DataGraph;

    fn three_node_graph() -> DataGraph {
        DataGraph::builder()
            .add_node(0, "machine learning")
            .add_node(1, "neural networks")
            .add_node(2, "data science")
            .add_edge(0, 1)
            .add_edge(1, 2)
            .add_edge(2, 0)
            .build()
    }

    #[test]
    fn test_walker_total_count() {
        let cfg = WalkerConfig { num_walks: 5, walk_length: 10, seed: 1 };
        let walker = RandomWalker::new(cfg);
        let graph = three_node_graph();
        let walks = walker.generate_walks(&graph);
        // 3 nodes × 5 walks = 15
        assert_eq!(walks.len(), 15);
    }

    #[test]
    fn test_walker_walk_length() {
        let cfg = WalkerConfig { num_walks: 3, walk_length: 8, seed: 42 };
        let walker = RandomWalker::new(cfg);
        let graph = three_node_graph();
        for walk in walker.generate_walks(&graph) {
            let tokens: Vec<&str> = walk.split_whitespace().collect();
            assert_eq!(tokens.len(), 8, "each walk should have walk_length tokens");
        }
    }

    #[test]
    fn test_walker_token_format() {
        let cfg = WalkerConfig::default();
        let walker = RandomWalker::new(cfg);
        let graph = three_node_graph();
        for walk in walker.generate_walks(&graph) {
            for token in walk.split_whitespace() {
                assert!(
                    token.starts_with("__node_"),
                    "token '{token}' should start with __node_"
                );
                let suffix = &token["__node_".len()..];
                assert!(suffix.parse::<usize>().is_ok(), "suffix should be a usize");
            }
        }
    }

    #[test]
    fn test_walker_empty_graph() {
        let cfg = WalkerConfig::default();
        let walker = RandomWalker::new(cfg);
        let graph = DataGraph::builder().build();
        assert!(walker.generate_walks(&graph).is_empty());
    }

    #[test]
    fn test_walker_isolated_node_stays() {
        // A node with no edges should produce walks that repeat the same token
        let graph = DataGraph::builder()
            .add_node(7, "isolated")
            .build();
        let cfg = WalkerConfig { num_walks: 2, walk_length: 5, seed: 1 };
        let walker = RandomWalker::new(cfg);
        let walks = walker.generate_walks(&graph);
        assert_eq!(walks.len(), 2);
        for walk in &walks {
            let tokens: Vec<&str> = walk.split_whitespace().collect();
            assert!(tokens.iter().all(|&t| t == "__node_7"));
        }
    }

    #[test]
    fn test_walker_deterministic() {
        let cfg = WalkerConfig { num_walks: 4, walk_length: 6, seed: 123 };
        let graph = three_node_graph();
        let w1 = RandomWalker::new(cfg.clone()).generate_walks(&graph);
        let w2 = RandomWalker::new(cfg).generate_walks(&graph);
        assert_eq!(w1, w2, "same seed should produce identical walks");
    }
}
