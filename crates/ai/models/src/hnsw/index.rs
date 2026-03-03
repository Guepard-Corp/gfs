//===----------------------------------------------------------------------===//
//                         GFS
//
// hnsw.rs
//
// HNSW — Hierarchical Navigable Small World Graph
// Reference: Malkov, Yashunin. "Efficient and Robust Approximate Nearest
//            Neighbor using Hierarchical Navigable Small World Graphs."
//            IEEE TPAMI, 2018.
//
// Migrated from DuckDB's extension/semantic_search/hnsw/hnsw.hpp
//===----------------------------------------------------------------------===//

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io;
use std::sync::Mutex;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use rand::distributions::Distribution;

/// Distance metric for similarity search
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DistanceMetric {
    /// Cosine distance (1 - cosine similarity)
    Cosine,
    /// L2 (Euclidean) distance squared
    L2,
    /// Negative inner product (for maximum inner product search)
    InnerProduct,
}

/// HNSW index configuration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HNSWConfig {
    /// Number of bidirectional links for each node (except layer 0)
    pub m: usize,
    /// Max neighbors for layer 0
    pub m_max0: usize,
    /// Size of dynamic candidate list during construction
    pub ef_construction: usize,
    /// Size of dynamic candidate list during search
    pub ef_search: usize,
    /// Level multiplier (calculated if 0.0)
    pub m_l: f64,
    /// Distance metric
    pub metric: DistanceMetric,
    /// Random seed
    pub seed: u64,
}

impl Default for HNSWConfig {
    fn default() -> Self {
        Self {
            m: 16,
            m_max0: 32,
            ef_construction: 200,
            ef_search: 50,
            m_l: 0.0, // Will be calculated
            metric: DistanceMetric::Cosine,
            seed: 42,
        }
    }
}

/// A node in the HNSW graph
#[derive(Debug, Clone)]
struct Node {
    /// Vector data
    vec: Vec<f32>,
    /// External ID (user-provided)
    external_id: i32,
    /// Node's level in the hierarchy
    level: usize,
    /// Neighbors at each level (neighbors[layer] = list of neighbor IDs)
    neighbors: Vec<Vec<usize>>,
}

/// Distance-ID pair for priority queues
#[derive(Debug, Clone, Copy)]
struct DistId {
    dist: f32,
    id: usize,
}

impl PartialEq for DistId {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist
    }
}

impl Eq for DistId {}

impl PartialOrd for DistId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.dist.partial_cmp(&other.dist)
    }
}

impl Ord for DistId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

/// Max-heap (farther first) - used for results
struct FartherFirstHeap(BinaryHeap<DistId>);

impl FartherFirstHeap {
    fn new() -> Self {
        Self(BinaryHeap::new())
    }

    fn push(&mut self, item: DistId) {
        self.0.push(item);
    }

    fn pop(&mut self) -> Option<DistId> {
        self.0.pop()
    }

    fn peek(&self) -> Option<&DistId> {
        self.0.peek()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Min-heap (closer first) - used for candidates
struct CloserFirstHeap(BinaryHeap<std::cmp::Reverse<DistId>>);

impl CloserFirstHeap {
    fn new() -> Self {
        Self(BinaryHeap::new())
    }

    fn push(&mut self, item: DistId) {
        self.0.push(std::cmp::Reverse(item));
    }

    fn pop(&mut self) -> Option<DistId> {
        self.0.pop().map(|r| r.0)
    }

    #[allow(dead_code)]
    fn peek(&self) -> Option<&DistId> {
        self.0.peek().map(|r| &r.0)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// HNSW Index for approximate nearest neighbor search
pub struct HNSWIndex {
    /// Vector dimension
    dim: usize,
    /// Configuration
    cfg: HNSWConfig,
    /// Random number generator
    rng: Mutex<rand::rngs::StdRng>,
    /// Graph nodes
    nodes: Vec<Node>,
    /// Maximum level in the graph
    max_level: i32,
    /// Entry point node ID
    entry_point: i32,
    /// Read-write lock for thread safety
    rw_lock: Mutex<()>,
}

impl HNSWIndex {
    /// Create a new HNSW index
    ///
    /// # Arguments
    /// * `dimension` - Vector dimension
    /// * `cfg` - Configuration (optional)
    ///
    /// # Returns
    /// New HNSW index
    ///
    /// # Panics
    /// If dimension <= 0 or M < 2
    pub fn new(dimension: usize, mut cfg: HNSWConfig) -> Self {
        if dimension == 0 {
            panic!("dimension must be > 0");
        }
        if cfg.m < 2 {
            panic!("M must be >= 2");
        }

        // Set defaults
        if cfg.m_max0 == 0 {
            cfg.m_max0 = 2 * cfg.m;
        }
        if cfg.m_l <= 0.0 {
            cfg.m_l = 1.0 / (cfg.m as f64).ln();
        }

        use rand::SeedableRng;
        let rng = Mutex::new(rand::rngs::StdRng::seed_from_u64(cfg.seed));

        Self {
            dim: dimension,
            cfg,
            rng,
            nodes: Vec::new(),
            max_level: -1,
            entry_point: -1,
            rw_lock: Mutex::new(()),
        }
    }

    /// Get the dimension
    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Get the number of nodes
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// Insert a vector into the index
    ///
    /// # Arguments
    /// * `vec` - Vector to insert (must match dimension)
    /// * `external_id` - Optional external ID (defaults to internal ID)
    ///
    /// # Returns
    /// Internal node ID
    pub fn insert(&mut self, vec: &[f32], external_id: Option<i32>) -> usize {
        if vec.len() != self.dim {
            panic!("insert: dimension mismatch");
        }

        let _lock = self.rw_lock.lock().unwrap();

        let id = self.nodes.len();
        let level = self.random_level();

        let node = Node {
            vec: vec.to_vec(),
            external_id: external_id.unwrap_or(id as i32),
            level,
            neighbors: vec![Vec::new(); level + 1],
        };

        self.nodes.push(node);

        // First node becomes entry point
        if id == 0 {
            self.entry_point = 0;
            self.max_level = level as i32;
            return id;
        }

        let q = &self.nodes[id].vec.clone();
        let mut ep = self.entry_point as usize;

        // Greedy search from top to target level
        for layer in ((level + 1)..=(self.max_level as usize)).rev() {
            ep = self.greedy_descend(q, ep, layer);
        }

        // Collect nodes to prune first
        let mut nodes_to_prune = Vec::new();

        // Insert at each level from target down to 0
        for layer in (0..=level.min(self.max_level as usize)).rev() {
            let w = self.search_layer(q, ep, self.cfg.ef_construction, layer);
            let m_cur = self.max_neighbors_for(layer);
            let selected = self.select_neighbors(q, &w, m_cur);

            self.nodes[id].neighbors[layer] = selected.clone();

            // Add bidirectional links and mark for pruning
            for &nbr in &selected {
                self.nodes[nbr].neighbors[layer].push(id);
                if self.nodes[nbr].neighbors[layer].len() > m_cur {
                    nodes_to_prune.push((nbr, layer));
                }
            }

            if !w.is_empty() {
                ep = w[0].id;
            }
        }

        // Drop the lock before pruning
        drop(_lock);

        // Now prune the marked nodes
        for (node_id, layer) in nodes_to_prune {
            self.prune(node_id, layer);
        }

        // Update entry point if necessary
        if level > self.max_level as usize {
            self.max_level = level as i32;
            self.entry_point = id as i32;
        }

        id
    }

    /// Search for k nearest neighbors
    ///
    /// # Arguments
    /// * `query` - Query vector
    /// * `k` - Number of neighbors to return
    /// * `ef` - Search quality (optional, uses ef_search if None)
    ///
    /// # Returns
    /// Vector of (distance, external_id) pairs sorted by distance
    pub fn search(&self, query: &[f32], k: usize, ef: Option<usize>) -> Vec<(f32, i32)> {
        if query.len() != self.dim {
            panic!("search: dimension mismatch");
        }

        let _lock = self.rw_lock.lock().unwrap();

        if self.nodes.is_empty() {
            return Vec::new();
        }

        let ef = ef.unwrap_or(self.cfg.ef_search).max(k);
        let mut ep = self.entry_point as usize;

        // Greedy search from top to layer 0
        for layer in ((1)..=(self.max_level as usize)).rev() {
            ep = self.greedy_descend(query, ep, layer);
        }

        // Search at layer 0
        let mut w = self.search_layer(query, ep, ef, 0);

        // Truncate to k results
        if w.len() > k {
            w.truncate(k);
        }

        // Convert to (dist, external_id) pairs
        w.into_iter()
            .map(|p| (p.dist, self.nodes[p.id].external_id))
            .collect()
    }

    /// Save index to file
    pub fn save(&self, path: &str) -> io::Result<()> {
        let mut file = File::create(path)?;

        // Write header
        file.write_i32::<LittleEndian>(self.dim as i32)?;
        file.write_i32::<LittleEndian>(self.max_level)?;
        file.write_i32::<LittleEndian>(self.entry_point)?;
        file.write_i32::<LittleEndian>(self.cfg.m as i32)?;
        file.write_i32::<LittleEndian>(self.cfg.m_max0 as i32)?;
        file.write_i32::<LittleEndian>(self.cfg.metric as i32)?;

        // Write nodes
        file.write_i32::<LittleEndian>(self.nodes.len() as i32)?;

        for node in &self.nodes {
            file.write_i32::<LittleEndian>(node.external_id)?;
            file.write_i32::<LittleEndian>(node.level as i32)?;

            // Write vector
            for &val in &node.vec {
                file.write_f32::<LittleEndian>(val)?;
            }

            // Write neighbors at each level
            for level in 0..=node.level {
                let neighbors = &node.neighbors[level];
                file.write_i32::<LittleEndian>(neighbors.len() as i32)?;
                for &n in neighbors {
                    file.write_i32::<LittleEndian>(n as i32)?;
                }
            }
        }

        Ok(())
    }

    /// Load index from file
    pub fn load(&mut self, path: &str) -> io::Result<()> {
        let mut file = File::open(path)?;

        // Read header
        self.dim = file.read_i32::<LittleEndian>()? as usize;
        self.max_level = file.read_i32::<LittleEndian>()?;
        self.entry_point = file.read_i32::<LittleEndian>()?;
        self.cfg.m = file.read_i32::<LittleEndian>()? as usize;
        self.cfg.m_max0 = file.read_i32::<LittleEndian>()? as usize;
        let metric_int = file.read_i32::<LittleEndian>()?;
        self.cfg.metric = match metric_int {
            0 => DistanceMetric::Cosine,
            1 => DistanceMetric::L2,
            2 => DistanceMetric::InnerProduct,
            _ => DistanceMetric::Cosine,
        };

        // Read nodes
        let n = file.read_i32::<LittleEndian>()? as usize;
        self.nodes.clear();
        self.nodes.reserve(n);

        for _ in 0..n {
            let external_id = file.read_i32::<LittleEndian>()?;
            let level = file.read_i32::<LittleEndian>()? as usize;

            // Read vector
            let mut vec = vec![0.0f32; self.dim];
            for val in &mut vec {
                *val = file.read_f32::<LittleEndian>()?;
            }

            // Read neighbors at each level
            let mut neighbors = vec![Vec::new(); level + 1];
            for level_idx in 0..=level {
                let nc = file.read_i32::<LittleEndian>()? as usize;
                neighbors[level_idx].reserve(nc);
                for _ in 0..nc {
                    neighbors[level_idx].push(file.read_i32::<LittleEndian>()? as usize);
                }
            }

            self.nodes.push(Node {
                vec,
                external_id,
                level,
                neighbors,
            });
        }

        Ok(())
    }

    // Private helper methods

    /// Compute distance between two vectors
    fn dist(&self, a: &[f32], b: &[f32]) -> f32 {
        match self.cfg.metric {
            DistanceMetric::L2 => Self::l2_squared(a, b),
            DistanceMetric::Cosine => Self::cosine_distance(a, b),
            DistanceMetric::InnerProduct => Self::neg_inner_product(a, b),
        }
    }

    /// L2 distance squared
    fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = x - y;
                d * d
            })
            .sum()
    }

    /// Cosine distance (1 - cosine similarity)
    fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        let denom = na * nb;
        if denom == 0.0 {
            return 1.0;
        }
        1.0 - dot / denom
    }

    /// Negative inner product
    fn neg_inner_product(a: &[f32], b: &[f32]) -> f32 {
        -a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>()
    }

    /// Generate random level for new node
    fn random_level(&self) -> usize {
        let mut rng = self.rng.lock().unwrap();
        let uniform = rand::distributions::Uniform::new(0.0, 1.0);
        let u: f64 = uniform.sample(&mut *rng);
        ((-u.ln()) * self.cfg.m_l) as usize
    }

    /// Get max neighbors for a layer
    fn max_neighbors_for(&self, layer: usize) -> usize {
        if layer == 0 {
            self.cfg.m_max0
        } else {
            self.cfg.m
        }
    }

    /// Greedy descent to find closest node at a layer
    fn greedy_descend(&self, q: &[f32], mut ep: usize, layer: usize) -> usize {
        let mut best = self.dist(q, &self.nodes[ep].vec);
        let mut improved = true;

        while improved {
            improved = false;
            for &nbr in &self.nodes[ep].neighbors[layer] {
                let d = self.dist(q, &self.nodes[nbr].vec);
                if d < best {
                    best = d;
                    ep = nbr;
                    improved = true;
                }
            }
        }

        ep
    }

    /// Search a single layer
    fn search_layer(&self, q: &[f32], ep: usize, ef: usize, layer: usize) -> Vec<DistId> {
        let mut visited = vec![false; self.nodes.len()];
        visited[ep] = true;

        let d0 = self.dist(q, &self.nodes[ep].vec);
        let mut candidates = CloserFirstHeap::new();
        let mut results = FartherFirstHeap::new();

        candidates.push(DistId { dist: d0, id: ep });
        results.push(DistId { dist: d0, id: ep });

        while !candidates.is_empty() {
            let current = candidates.pop().unwrap();

            if current.dist > results.peek().unwrap().dist {
                break;
            }

            if layer > self.nodes[current.id].level {
                continue;
            }

            for &nbr in &self.nodes[current.id].neighbors[layer] {
                if visited[nbr] {
                    continue;
                }
                visited[nbr] = true;

                let d = self.dist(q, &self.nodes[nbr].vec);

                if d < results.peek().unwrap().dist || results.len() < ef {
                    candidates.push(DistId { dist: d, id: nbr });
                    results.push(DistId { dist: d, id: nbr });

                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        // Convert heap to sorted vec
        let mut out = Vec::with_capacity(results.len());
        while !results.is_empty() {
            out.push(results.pop().unwrap());
        }
        out.reverse(); // Now in ascending order
        out
    }

    /// Select neighbors using heuristic
    fn select_neighbors(&self, _q: &[f32], cands: &[DistId], m: usize) -> Vec<usize> {
        let mut selected: Vec<usize> = Vec::with_capacity(m);
        let mut rejected: Vec<usize> = Vec::new();

        for &cand in cands {
            if selected.len() >= m {
                break;
            }

            let mut accept = true;
            let cand_vec: &Vec<f32> = &self.nodes[cand.id].vec;
            for &sid in &selected {
                let sid_vec: &Vec<f32> = &self.nodes[sid].vec;
                let dist_cs = self.dist(cand_vec, sid_vec);
                if dist_cs < cand.dist {
                    accept = false;
                    break;
                }
            }

            if accept {
                selected.push(cand.id);
            } else {
                rejected.push(cand.id);
            }
        }

        // Fill remaining slots with rejected if needed
        for rid in rejected {
            if selected.len() >= m {
                break;
            }
            selected.push(rid);
        }

        selected
    }

    /// Prune neighbors of a node
    fn prune(&mut self, node_id: usize, layer: usize) {
        let m_cur = self.max_neighbors_for(layer);
        let nlist = &self.nodes[node_id].neighbors[layer];

        if nlist.len() <= m_cur {
            return;
        }

        let q = &self.nodes[node_id].vec.clone();

        let mut cands: Vec<DistId> = nlist
            .iter()
            .map(|&nid| DistId {
                dist: self.dist(q, &self.nodes[nid].vec),
                id: nid,
            })
            .collect();

        cands.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

        self.nodes[node_id].neighbors[layer] = self.select_neighbors(q, &cands, m_cur);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance_metrics() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];

        let l2 = HNSWIndex::l2_squared(&a, &b);
        assert!((l2 - 2.0).abs() < 0.001);

        let cos = HNSWIndex::cosine_distance(&a, &b);
        assert!((cos - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_hnsw_create() {
        let cfg = HNSWConfig::default();
        let index = HNSWIndex::new(128, cfg);
        assert_eq!(index.dimension(), 128);
        assert_eq!(index.size(), 0);
    }

    #[test]
    fn test_hnsw_insert() {
        let cfg = HNSWConfig {
            m: 8,
            ef_construction: 100,
            ..Default::default()
        };
        let mut index = HNSWIndex::new(10, cfg);

        let vec1 = vec![1.0; 10];
        let id1 = index.insert(&vec1, None);
        assert_eq!(id1, 0);
        assert_eq!(index.size(), 1);

        let vec2 = vec![2.0; 10];
        let id2 = index.insert(&vec2, Some(100));
        assert_eq!(id2, 1);
        assert_eq!(index.size(), 2);
    }

    #[test]
    fn test_hnsw_search() {
        let mut cfg = HNSWConfig::default();
        cfg.m = 8;
        cfg.ef_construction = 50;
        cfg.ef_search = 50;

        let mut index = HNSWIndex::new(3, cfg);

        // Insert some vectors
        index.insert(&[1.0, 0.0, 0.0], Some(1));
        index.insert(&[0.0, 1.0, 0.0], Some(2));
        index.insert(&[0.0, 0.0, 1.0], Some(3));
        index.insert(&[0.9, 0.1, 0.0], Some(4));

        // Search for nearest to [1, 0, 0]
        let results = index.search(&[1.0, 0.0, 0.0], 2, None);

        assert_eq!(results.len(), 2);
        // Should find exact match first
        assert_eq!(results[0].1, 1); // external_id = 1
    }

    #[test]
    fn test_hnsw_save_load() {
        let cfg = HNSWConfig {
            m: 8,
            ef_construction: 50,
            ..Default::default()
        };
        let mut index1 = HNSWIndex::new(5, cfg.clone());

        // Insert vectors
        index1.insert(&[1.0, 2.0, 3.0, 4.0, 5.0], Some(10));
        index1.insert(&[5.0, 4.0, 3.0, 2.0, 1.0], Some(20));

        // Save
        let path = "/tmp/test_hnsw.bin";
        index1.save(path).unwrap();

        // Load
        let mut index2 = HNSWIndex::new(5, cfg);
        index2.load(path).unwrap();

        assert_eq!(index2.size(), 2);
        assert_eq!(index2.dimension(), 5);

        // Clean up
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_cosine_metric() {
        let cfg = HNSWConfig {
            metric: DistanceMetric::Cosine,
            m: 4,
            ..Default::default()
        };
        let mut index = HNSWIndex::new(3, cfg);

        // Normalized vectors
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.0, 1.0, 0.0];
        let v3 = vec![0.7071, 0.7071, 0.0]; // 45 degrees from both

        index.insert(&v1, Some(1));
        index.insert(&v2, Some(2));
        index.insert(&v3, Some(3));

        // Query with v1 should find v1 closest, then v3
        let results = index.search(&v1, 2, None);
        assert_eq!(results[0].1, 1);
    }
}
