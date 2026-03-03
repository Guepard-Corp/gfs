//===----------------------------------------------------------------------===//
//                         GFS
//
// graph/engine.rs - Graph-Aware Semantic Engine
//
// Combines Word2Vec text embeddings with DeepWalk graph-structural embeddings
// into a single unified representation (TextDeepWalk / mixed-corpus strategy).
//
// Training pipeline:
//   1. Text corpus  — one document per graph node (node.text × text_repeat)
//   2. Walk corpus  — DeepWalk random-walk sentences (__node_ID tokens)
//   3. Single Word2Vec trained on the merged corpus
//   4. Each node embedded via embed_text(node.text)
//   5. HNSW index built over node embeddings
//===----------------------------------------------------------------------===//

use std::io;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use crate::core::{Word2Vec, Word2VecConfig};
use crate::core::stop_words::is_stop_word;
use crate::core::vocab::Vocabulary;
use crate::hnsw::{HNSWIndex, HNSWConfig};
use crate::semantic_search_engine::ModelInfo;
use super::data_graph::{DataGraph, DataNode};
use super::walker::{RandomWalker, WalkerConfig};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum GraphError {
    /// Engine has not been trained yet.
    NotTrained,
    /// Input is invalid (e.g. empty graph, k = 0).
    InvalidInput(String),
    /// Graph has no nodes.
    EmptyGraph,
    /// I/O error during save / load.
    Io(io::Error),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTrained => write!(f, "graph engine not trained"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::EmptyGraph => write!(f, "graph has no nodes"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for GraphError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, GraphError>;

// ── Config ────────────────────────────────────────────────────────────────────

/// Full configuration for [`GraphSemanticEngine`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GraphEngineConfig {
    /// Word2Vec training configuration.
    pub w2v: Word2VecConfig,
    /// HNSW index configuration.
    pub hnsw: HNSWConfig,
    /// Random-walk configuration (DeepWalk).
    pub walker: WalkerConfig,
    /// How many times each text document is repeated in the mixed corpus to
    /// balance it against the walk corpus (which is `num_walks` × larger).
    /// Default: 3.
    pub text_repeat: usize,
}

impl Default for GraphEngineConfig {
    fn default() -> Self {
        let mut w2v = Word2VecConfig::default();
        w2v.min_count = 1; // small graphs need all tokens
        Self {
            w2v,
            hnsw: HNSWConfig::default(),
            walker: WalkerConfig::default(),
            text_repeat: 3,
        }
    }
}

// ── Search result ─────────────────────────────────────────────────────────────

/// A single result returned by [`GraphSemanticEngine::search`].
#[derive(Debug, Clone)]
pub struct GraphSearchResult {
    /// Sequential node index (as assigned during training).
    pub node_id: usize,
    /// Source table / collection (if provenance was recorded).
    pub source_table: Option<String>,
    /// Source row primary key (if provenance was recorded).
    pub source_row_id: Option<String>,
    /// Original node text.
    pub text: String,
    /// Cosine distance to the query vector (lower = more similar, range [0, 2]).
    pub distance: f32,
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Graph-aware semantic search engine (TextDeepWalk).
///
/// Trains a **single** Word2Vec model on a mixed corpus of:
/// - text documents from graph nodes
/// - DeepWalk random-walk sequences
///
/// The resulting embeddings encode both semantic content (text similarity) and
/// structural proximity (graph neighbourhood) in the same vector space.
///
/// # Usage
///
/// ```no_run
/// use gfs_ai_models::graph::{DataGraph, GraphSemanticEngine};
///
/// // Build an in-memory graph
/// let graph = DataGraph::builder()
///     .add_node_with_meta(0, "machine learning product", "products", "0")
///     .add_node_with_meta(1, "neural network component", "components", "1")
///     .add_node_with_meta(2, "cooking ingredient", "items", "2")
///     .add_edge(0, 1)
///     .build();
///
/// let mut engine = GraphSemanticEngine::new();
/// engine.train(&graph).unwrap();
///
/// let results = engine.search("deep learning", 2).unwrap();
/// for r in &results {
///     println!("[{:.3}] {:?}/{:?} — {}", r.distance, r.source_table, r.source_row_id, r.text);
/// }
/// ```
pub struct GraphSemanticEngine {
    w2v: Word2Vec,
    hnsw: Option<HNSWIndex>,
    /// Node provenance retained for [`search`](Self::search) result construction.
    nodes: Vec<DataNode>,
    cfg: GraphEngineConfig,
}

impl GraphSemanticEngine {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Create an engine with default configuration.
    pub fn new() -> Self {
        Self::with_config(GraphEngineConfig::default())
    }

    /// Create an engine with explicit configuration.
    pub fn with_config(cfg: GraphEngineConfig) -> Self {
        Self {
            w2v: Word2Vec::with_config(cfg.w2v.clone()),
            hnsw: None,
            nodes: Vec::new(),
            cfg,
        }
    }

    // ── Core API ──────────────────────────────────────────────────────────────

    /// Train on a [`DataGraph`].
    ///
    /// Steps:
    /// 1. Build a mixed corpus: text documents (repeated `text_repeat` times) +
    ///    DeepWalk random-walk sentences.
    /// 2. Train a single `Word2Vec` on the mixed corpus.
    /// 3. Embed every node via `embed_text(node.text)`.
    /// 4. Insert all non-degenerate embeddings into an HNSW index.
    ///
    /// Nodes whose text produces a degenerate (all-zero / NaN) embedding —
    /// because they contain only stop-words or OOV tokens — are silently skipped.
    ///
    /// # Errors
    /// Returns [`GraphError::EmptyGraph`] when the graph has no nodes.
    pub fn train(&mut self, graph: &DataGraph) -> Result<ModelInfo> {
        if graph.node_count() == 0 {
            return Err(GraphError::EmptyGraph);
        }

        // ── 1. Build mixed corpus ─────────────────────────────────────────────
        let walker = RandomWalker::new(self.cfg.walker.clone());
        let walk_sentences = walker.generate_walks(graph);

        let mut corpus: Vec<String> =
            Vec::with_capacity(graph.node_count() * self.cfg.text_repeat + walk_sentences.len());

        // Repeat text documents to balance against the larger walk corpus
        for _ in 0..self.cfg.text_repeat {
            for node in &graph.nodes {
                if !node.text.trim().is_empty() {
                    corpus.push(node.text.clone());
                }
            }
        }
        corpus.extend(walk_sentences);

        // ── 2. Train Word2Vec ─────────────────────────────────────────────────
        self.w2v = Word2Vec::with_config(self.cfg.w2v.clone());
        self.w2v.train(&corpus);
        let dim = self.w2v.dimension();

        // ── 3+4. Embed nodes → HNSW ──────────────────────────────────────────
        let mut hnsw = HNSWIndex::new(dim, self.cfg.hnsw.clone());
        for (idx, node) in graph.nodes.iter().enumerate() {
            let emb = self.w2v.embed_text(&node.text);
            if emb.iter().all(|v| *v == 0.0) || emb.iter().any(|v| v.is_nan()) {
                continue; // skip degenerate embeddings
            }
            hnsw.insert(&emb, Some(idx as i32));
        }

        // Retain node list for result lookup
        self.nodes = graph.nodes.clone();
        self.hnsw = Some(hnsw);

        Ok(ModelInfo {
            dimension: dim,
            vocab_size: self.w2v.vocab().size(),
            corpus_size: graph.node_count(),
        })
    }

    /// Embed a text string using the trained Word2Vec model.
    ///
    /// # Errors
    /// [`GraphError::NotTrained`] if called before [`train`](Self::train).
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if self.w2v.vocab().size() == 0 {
            return Err(GraphError::NotTrained);
        }
        Ok(self.w2v.embed_text(text))
    }

    /// Return the `k` most similar nodes to `query`, ordered by cosine distance
    /// ascending (closest first).
    ///
    /// # Errors
    /// [`GraphError::NotTrained`] if called before [`train`](Self::train).
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<GraphSearchResult>> {
        let hnsw = self.hnsw.as_ref().ok_or(GraphError::NotTrained)?;

        if k == 0 {
            return Ok(Vec::new());
        }

        let query_emb = self.w2v.embed_text(query);
        let hits = hnsw.search(&query_emb, k, None);

        let results = hits
            .into_iter()
            .filter_map(|(dist, ext_id)| {
                let idx = ext_id as usize;
                self.nodes.get(idx).map(|n| GraphSearchResult {
                    node_id: n.id,
                    source_table: n.source_table.clone(),
                    source_row_id: n.source_row_id.clone(),
                    text: n.text.clone(),
                    distance: dist,
                })
            })
            .collect();

        Ok(results)
    }

    /// Return `true` if any non-stop-word token in `text` is out-of-vocabulary.
    ///
    /// Returns `true` when the engine has not been trained yet.
    pub fn has_oov(&self, text: &str) -> bool {
        let vocab = self.w2v.vocab();
        if vocab.size() == 0 {
            return true;
        }
        Vocabulary::tokenize(text)
            .iter()
            .filter(|t| !is_stop_word(t))
            .any(|t| vocab.lookup(t).is_none())
    }

    /// Return metadata for the current model, or `None` if not trained.
    pub fn model_info(&self) -> Option<ModelInfo> {
        if self.w2v.vocab().size() == 0 {
            return None;
        }
        Some(ModelInfo {
            dimension: self.w2v.dimension(),
            vocab_size: self.w2v.vocab().size(),
            corpus_size: self.nodes.len(),
        })
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    /// Save the trained engine to disk.
    ///
    /// Writes three files:
    /// - `{path}.w2v`    — Word2Vec model
    /// - `{path}.hnsw`   — HNSW index
    /// - `{path}.gnodes` — node provenance (binary, length-prefixed UTF-8)
    ///
    /// # Errors
    /// [`GraphError::NotTrained`] if called before training.
    pub fn save(&self, path: &str) -> Result<()> {
        if self.hnsw.is_none() {
            return Err(GraphError::NotTrained);
        }

        // Create parent directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        self.w2v.save(&format!("{path}.w2v"))?;
        self.hnsw.as_ref().unwrap().save(&format!("{path}.hnsw"))?;
        Self::write_nodes(&self.nodes, &format!("{path}.gnodes"))?;
        Ok(())
    }

    /// Load a previously saved engine from disk.
    ///
    /// Reads `{path}.w2v`, `{path}.hnsw`, `{path}.gnodes`.
    pub fn load(&mut self, path: &str) -> Result<ModelInfo> {
        self.w2v.load(&format!("{path}.w2v"))?;
        let dim = self.w2v.dimension();
        let mut hnsw = HNSWIndex::new(dim, self.cfg.hnsw.clone());
        hnsw.load(&format!("{path}.hnsw"))?;
        self.hnsw = Some(hnsw);
        self.nodes = Self::read_nodes(&format!("{path}.gnodes"))?;
        Ok(ModelInfo {
            dimension: dim,
            vocab_size: self.w2v.vocab().size(),
            corpus_size: self.nodes.len(),
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Binary node format:
    /// u64 count, then per node:
    ///   u64 id, u64 text_len + bytes,
    ///   u8 has_table, [u64 len + bytes],
    ///   u8 has_row_id, [u64 len + bytes]
    fn write_nodes(nodes: &[DataNode], path: &str) -> io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        f.write_u64::<LittleEndian>(nodes.len() as u64)?;
        for n in nodes {
            f.write_u64::<LittleEndian>(n.id as u64)?;
            write_str(&mut f, &n.text)?;
            write_opt_str(&mut f, n.source_table.as_deref())?;
            write_opt_str(&mut f, n.source_row_id.as_deref())?;
        }
        Ok(())
    }

    fn read_nodes(path: &str) -> io::Result<Vec<DataNode>> {
        let mut f = std::fs::File::open(path)?;
        let count = f.read_u64::<LittleEndian>()? as usize;
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            let id = f.read_u64::<LittleEndian>()? as usize;
            let text = read_str(&mut f)?;
            let source_table = read_opt_str(&mut f)?;
            let source_row_id = read_opt_str(&mut f)?;
            nodes.push(DataNode { id, text, source_table, source_row_id });
        }
        Ok(nodes)
    }
}

impl Default for GraphSemanticEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Serialisation helpers ─────────────────────────────────────────────────────

fn write_str(f: &mut std::fs::File, s: &str) -> io::Result<()> {
    use std::io::Write as _;
    let b = s.as_bytes();
    f.write_u64::<LittleEndian>(b.len() as u64)?;
    f.write_all(b)
}

fn write_opt_str(f: &mut std::fs::File, s: Option<&str>) -> io::Result<()> {
    match s {
        None => f.write_u8(0),
        Some(val) => {
            f.write_u8(1)?;
            write_str(f, val)
        }
    }
}

fn read_str(f: &mut std::fs::File) -> io::Result<String> {
    use std::io::Read as _;
    let len = f.read_u64::<LittleEndian>()? as usize;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_opt_str(f: &mut std::fs::File) -> io::Result<Option<String>> {
    let flag = f.read_u8()?;
    if flag == 0 {
        Ok(None)
    } else {
        Ok(Some(read_str(f)?))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::DataGraph;

    fn small_graph() -> DataGraph {
        DataGraph::builder()
            .add_node_with_meta(0, "machine learning algorithms are powerful", "products", "0")
            .add_node_with_meta(1, "deep learning uses neural networks", "products", "1")
            .add_node_with_meta(2, "natural language processing transformers", "features", "2")
            .add_node_with_meta(3, "cooking pasta with tomato sauce", "items", "3")
            .add_node_with_meta(4, "baking bread at home", "items", "4")
            .add_edge(0, 1)
            .add_edge(1, 2)
            .add_edge(3, 4)
            .build()
    }

    fn small_engine() -> GraphSemanticEngine {
        let mut cfg = GraphEngineConfig::default();
        cfg.w2v.dimension = 20;
        cfg.w2v.epochs = 3;
        cfg.w2v.num_threads = 1;
        cfg.w2v.min_count = 1;
        cfg.hnsw.m = 4;
        cfg.hnsw.ef_construction = 20;
        cfg.walker.num_walks = 5;
        cfg.walker.walk_length = 10;
        cfg.text_repeat = 2;
        GraphSemanticEngine::with_config(cfg)
    }

    // ── train ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_train_empty_graph_returns_error() {
        let mut engine = small_engine();
        let err = engine.train(&DataGraph::builder().build()).unwrap_err();
        assert!(matches!(err, GraphError::EmptyGraph));
    }

    #[test]
    fn test_train_returns_model_info() {
        let mut engine = small_engine();
        let info = engine.train(&small_graph()).unwrap();
        assert!(info.vocab_size > 0);
        assert_eq!(info.dimension, 20);
        assert_eq!(info.corpus_size, 5);
    }

    #[test]
    fn test_model_info_none_before_training() {
        let engine = small_engine();
        assert!(engine.model_info().is_none());
    }

    #[test]
    fn test_model_info_some_after_training() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        assert!(engine.model_info().is_some());
    }

    // ── embed ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_embed_not_trained_returns_error() {
        let engine = small_engine();
        assert!(matches!(engine.embed("text"), Err(GraphError::NotTrained)));
    }

    #[test]
    fn test_embed_returns_correct_dimension() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        let emb = engine.embed("machine learning").unwrap();
        assert_eq!(emb.len(), 20);
    }

    #[test]
    fn test_embed_produces_finite_values() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        let emb = engine.embed("neural network").unwrap();
        assert!(emb.iter().all(|v| v.is_finite()));
    }

    // ── search ────────────────────────────────────────────────────────────────

    #[test]
    fn test_search_not_trained_returns_error() {
        let engine = small_engine();
        assert!(matches!(engine.search("query", 2), Err(GraphError::NotTrained)));
    }

    #[test]
    fn test_search_k_zero_returns_empty() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        let results = engine.search("machine learning", 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_returns_at_most_k_results() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        let results = engine.search("learning", 3).unwrap();
        assert!(results.len() <= 3);
    }

    #[test]
    fn test_search_results_ordered_by_distance() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        let results = engine.search("neural network", 4).unwrap();
        for w in results.windows(2) {
            assert!(w[0].distance <= w[1].distance);
        }
    }

    #[test]
    fn test_search_provenance_populated() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        let results = engine.search("machine learning", 3).unwrap();
        for r in &results {
            assert!(r.source_table.is_some());
            assert!(r.source_row_id.is_some());
        }
    }

    // ── has_oov ───────────────────────────────────────────────────────────────

    #[test]
    fn test_has_oov_untrained_returns_true() {
        let engine = small_engine();
        assert!(engine.has_oov("anything"));
    }

    #[test]
    fn test_has_oov_known_word() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        assert!(!engine.has_oov("machine learning"));
    }

    #[test]
    fn test_has_oov_unknown_word() {
        let mut engine = small_engine();
        engine.train(&small_graph()).unwrap();
        assert!(engine.has_oov("xyzzy_completely_unknown_qqqq"));
    }

    // ── save / load ───────────────────────────────────────────────────────────

    #[test]
    fn test_save_not_trained_returns_error() {
        let engine = small_engine();
        assert!(matches!(engine.save("/tmp/gfs_graph_test"), Err(GraphError::NotTrained)));
    }

    #[test]
    fn test_save_load_roundtrip() {
        let graph = small_graph();
        let mut engine = small_engine();
        let info_before = engine.train(&graph).unwrap();

        let path = "/tmp/gfs_graph_engine_test";
        engine.save(path).unwrap();

        let mut engine2 = small_engine();
        let info_after = engine2.load(path).unwrap();

        assert_eq!(info_before.dimension, info_after.dimension);
        assert_eq!(info_before.vocab_size, info_after.vocab_size);
        assert_eq!(info_before.corpus_size, info_after.corpus_size);

        let results = engine2.search("neural network", 2).unwrap();
        assert!(!results.is_empty());

        // Clean up
        for ext in &["w2v", "hnsw", "gnodes"] {
            std::fs::remove_file(format!("{path}.{ext}")).ok();
        }
    }

    #[test]
    fn test_load_nonexistent_returns_io_error() {
        let mut engine = small_engine();
        assert!(matches!(
            engine.load("/tmp/this_file_does_not_exist_gfs_graph"),
            Err(GraphError::Io(_))
        ));
    }
}
