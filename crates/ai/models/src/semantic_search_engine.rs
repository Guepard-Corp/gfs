//===----------------------------------------------------------------------===//
//                         GFS
//
// semantic_search_engine.rs - Semantic Search Engine
//
// High-level API combining Word2Vec and HNSW for end-to-end semantic search.
//
// Orchestrates the same pipeline as DuckDB's:
//   train_model.cpp     — corpus → Word2Vec training
//   embed_text.cpp      — text  → embedding vector
//   vector_search.cpp   — query → nearest-neighbour results
//   smart_maintenance.cpp::HasOOV() — OOV detection
//===----------------------------------------------------------------------===//

use std::io;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use crate::core::{Word2Vec, Word2VecConfig};
use crate::core::stop_words::is_stop_word;
use crate::core::vocab::Vocabulary;
use crate::hnsw::{HNSWIndex, HNSWConfig};

// ── Error ────────────────────────────────────────────────────────────────────

/// Errors that can occur during semantic search operations.
#[derive(Debug)]
pub enum SemanticSearchError {
    /// [`SemanticSearchEngine::train`] has not been called yet.
    NotTrained,
    /// The supplied input is invalid (e.g. empty corpus, k = 0).
    InvalidInput(String),
    /// An I/O error occurred during save or load.
    Io(io::Error),
}

impl std::fmt::Display for SemanticSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTrained => write!(f, "model not trained"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for SemanticSearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SemanticSearchError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, SemanticSearchError>;

// ── Supporting types ─────────────────────────────────────────────────────────

/// Metadata about a trained model returned by [`SemanticSearchEngine::train`]
/// and [`SemanticSearchEngine::model_info`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Embedding vector dimensionality.
    pub dimension: usize,
    /// Number of unique tokens in the vocabulary.
    pub vocab_size: usize,
    /// Number of documents indexed (= corpus length passed to `train`).
    pub corpus_size: usize,
}

/// A single result returned by [`SemanticSearchEngine::search`].
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Zero-based index of this document in the original training corpus.
    pub id: usize,
    /// The original document text.
    pub content: String,
    /// Cosine distance to the query vector (lower = more similar, range [0, 2]).
    pub distance: f32,
}

// ── Engine ───────────────────────────────────────────────────────────────────

/// End-to-end semantic search engine built on Word2Vec + HNSW.
///
/// # Typical workflow
///
/// ```no_run
/// use gfs_ai_models::semantic_search_engine::SemanticSearchEngine;
///
/// let corpus = vec![
///     "machine learning is fascinating".to_string(),
///     "deep learning with neural networks".to_string(),
///     "cooking pasta at home".to_string(),
/// ];
///
/// let mut engine = SemanticSearchEngine::new();
/// engine.train(&corpus).unwrap();
///
/// let results = engine.search("neural network learning", 2).unwrap();
/// for r in &results {
///     println!("id={} dist={:.4}  {}", r.id, r.distance, r.content);
/// }
///
/// engine.save("/tmp/my_model").unwrap();
/// ```
pub struct SemanticSearchEngine {
    /// Trained Word2Vec model.
    w2v: Word2Vec,
    /// HNSW index over per-document embedding vectors.
    hnsw: Option<HNSWIndex>,
    /// Original corpus retained for result content retrieval.
    corpus: Vec<String>,
    /// HNSW configuration applied on construction (and re-applied on load).
    hnsw_cfg: HNSWConfig,
}

impl SemanticSearchEngine {
    // ── Constructors ─────────────────────────────────────────────────────────

    /// Create an engine with default configuration.
    ///
    /// Uses Word2Vec defaults except `min_count = 1` (matching the behaviour of
    /// DuckDB's `train_model.cpp`) so that every corpus token is included even
    /// in small corpora.
    pub fn new() -> Self {
        let mut w2v_cfg = Word2VecConfig::default();
        w2v_cfg.min_count = 1;
        Self {
            w2v: Word2Vec::with_config(w2v_cfg),
            hnsw: None,
            corpus: Vec::new(),
            hnsw_cfg: HNSWConfig::default(),
        }
    }

    /// Create an engine with explicit Word2Vec and HNSW configuration.
    pub fn with_config(w2v_cfg: Word2VecConfig, hnsw_cfg: HNSWConfig) -> Self {
        Self {
            w2v: Word2Vec::with_config(w2v_cfg),
            hnsw: None,
            corpus: Vec::new(),
            hnsw_cfg,
        }
    }

    // ── Core API ─────────────────────────────────────────────────────────────

    /// Train the engine on a corpus of text documents.
    ///
    /// Steps:
    /// 1. Train a Word2Vec model on `corpus`.
    /// 2. Embed every document and insert it into a fresh HNSW index (the
    ///    external ID stored in the index equals the document's 0-based index).
    /// 3. Retain the corpus in memory so that [`search`](Self::search) can
    ///    return the original document text.
    ///
    /// Documents that produce a degenerate (all-zero / NaN) embedding — because
    /// they contain only stop-words or OOV tokens — are skipped silently; they
    /// will never appear in search results.
    ///
    /// # Errors
    /// Returns [`SemanticSearchError::InvalidInput`] when `corpus` is empty.
    pub fn train(&mut self, corpus: &[String]) -> Result<ModelInfo> {
        if corpus.is_empty() {
            return Err(SemanticSearchError::InvalidInput(
                "corpus is empty".into(),
            ));
        }

        // Train Word2Vec.
        self.w2v.train(corpus);
        let dim = self.w2v.dimension();

        // Build HNSW index over per-document embeddings.
        let mut hnsw = HNSWIndex::new(dim, self.hnsw_cfg.clone());
        for (i, doc) in corpus.iter().enumerate() {
            let emb = self.w2v.embed_text(doc);
            // Skip degenerate embeddings (all-OOV / all-stop-word documents).
            if emb.iter().all(|v| *v == 0.0) || emb.iter().any(|v| v.is_nan()) {
                continue;
            }
            hnsw.insert(&emb, Some(i as i32));
        }

        self.corpus = corpus.to_vec();
        self.hnsw = Some(hnsw);

        Ok(ModelInfo {
            dimension: dim,
            vocab_size: self.w2v.vocab().size(),
            corpus_size: self.corpus.len(),
        })
    }

    /// Embed a text string using the trained Word2Vec model.
    ///
    /// Returns the average of word vectors for known, non-stop-word tokens,
    /// normalised to unit length.
    ///
    /// # Errors
    /// Returns [`SemanticSearchError::NotTrained`] if called before
    /// [`train`](Self::train).
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if self.w2v.vocab().size() == 0 {
            return Err(SemanticSearchError::NotTrained);
        }
        Ok(self.w2v.embed_text(text))
    }

    /// Return the `k` most similar documents to `query`, ordered by cosine
    /// distance ascending (closest first).
    ///
    /// # Errors
    /// Returns [`SemanticSearchError::NotTrained`] if called before
    /// [`train`](Self::train).
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<SearchResult>> {
        let hnsw = self.hnsw.as_ref().ok_or(SemanticSearchError::NotTrained)?;

        if k == 0 {
            return Ok(Vec::new());
        }

        let query_emb = self.w2v.embed_text(query);
        let hits = hnsw.search(&query_emb, k, None);

        let results = hits
            .into_iter()
            .filter_map(|(dist, ext_id)| {
                let id = ext_id as usize;
                self.corpus.get(id).map(|content| SearchResult {
                    id,
                    content: content.clone(),
                    distance: dist,
                })
            })
            .collect();

        Ok(results)
    }

    /// Return `true` if any non-stop-word token in `text` is out-of-vocabulary.
    ///
    /// Mirrors `HasOOV()` from DuckDB's `smart_maintenance.cpp`. Useful for
    /// deciding whether a retrain is needed before embedding new documents:
    ///
    /// ```no_run
    /// # use gfs_ai_models::semantic_search_engine::SemanticSearchEngine;
    /// # let mut engine = SemanticSearchEngine::new();
    /// # engine.train(&["hello world".to_string()]).unwrap();
    /// if engine.has_oov("a brand new unseen term") {
    ///     // retrain with expanded corpus
    /// } else {
    ///     // incremental embed is sufficient
    /// }
    /// ```
    ///
    /// Returns `true` (treat as OOV) when the engine has not been trained yet.
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

    /// Return metadata for the currently loaded model, or `None` if the engine
    /// has not been trained or loaded yet.
    pub fn model_info(&self) -> Option<ModelInfo> {
        if self.w2v.vocab().size() == 0 {
            return None;
        }
        Some(ModelInfo {
            dimension: self.w2v.dimension(),
            vocab_size: self.w2v.vocab().size(),
            corpus_size: self.corpus.len(),
        })
    }

    // ── Persistence ──────────────────────────────────────────────────────────

    /// Persist the trained engine to disk.
    ///
    /// Writes three files (all required for a full restore via
    /// [`load`](Self::load)):
    ///
    /// | File | Contents |
    /// |---|---|
    /// | `{path}.w2v` | Word2Vec binary model |
    /// | `{path}.hnsw` | HNSW index binary |
    /// | `{path}.corpus` | Corpus documents (length-prefixed UTF-8) |
    ///
    /// # Errors
    /// - [`SemanticSearchError::NotTrained`] when called before training.
    /// - [`SemanticSearchError::Io`] on filesystem errors.
    pub fn save(&self, path: &str) -> Result<()> {
        if self.hnsw.is_none() {
            return Err(SemanticSearchError::NotTrained);
        }
        self.w2v.save(&format!("{path}.w2v"))?;
        self.hnsw.as_ref().unwrap().save(&format!("{path}.hnsw"))?;
        Self::write_corpus(&self.corpus, &format!("{path}.corpus"))?;
        Ok(())
    }

    /// Restore a previously saved engine from disk.
    ///
    /// Reads the three files written by [`save`](Self::save).
    ///
    /// # Errors
    /// Returns [`SemanticSearchError::Io`] when any file cannot be read or
    /// contains invalid data.
    pub fn load(&mut self, path: &str) -> Result<ModelInfo> {
        self.w2v.load(&format!("{path}.w2v"))?;
        let dim = self.w2v.dimension();

        // Create a placeholder index with the loaded dimension; `load()` will
        // overwrite dim, M, M_max0, metric, nodes from the file.
        let mut hnsw = HNSWIndex::new(dim, self.hnsw_cfg.clone());
        hnsw.load(&format!("{path}.hnsw"))?;
        self.hnsw = Some(hnsw);

        self.corpus = Self::read_corpus(&format!("{path}.corpus"))?;

        Ok(ModelInfo {
            dimension: dim,
            vocab_size: self.w2v.vocab().size(),
            corpus_size: self.corpus.len(),
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Binary corpus format: `u64` count, then per document: `u64` byte-length
    /// followed by raw UTF-8 bytes.  Uses little-endian throughout to match the
    /// HNSW and Word2Vec serialisation formats.
    fn write_corpus(corpus: &[String], path: &str) -> io::Result<()> {
        use std::io::Write as _;
        let mut f = std::fs::File::create(path)?;
        f.write_u64::<LittleEndian>(corpus.len() as u64)?;
        for doc in corpus {
            let bytes = doc.as_bytes();
            f.write_u64::<LittleEndian>(bytes.len() as u64)?;
            f.write_all(bytes)?;
        }
        Ok(())
    }

    fn read_corpus(path: &str) -> io::Result<Vec<String>> {
        use std::io::Read as _;
        let mut f = std::fs::File::open(path)?;
        let count = f.read_u64::<LittleEndian>()? as usize;
        let mut corpus = Vec::with_capacity(count);
        for _ in 0..count {
            let len = f.read_u64::<LittleEndian>()? as usize;
            let mut buf = vec![0u8; len];
            f.read_exact(&mut buf)?;
            let s = String::from_utf8(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            corpus.push(s);
        }
        Ok(corpus)
    }
}

impl Default for SemanticSearchEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn small_corpus() -> Vec<String> {
        vec![
            "machine learning algorithms are powerful".to_string(),
            "deep learning uses neural networks".to_string(),
            "natural language processing with transformers".to_string(),
            "cooking pasta with tomato sauce".to_string(),
            "baking bread at home is easy".to_string(),
            "data science and statistics".to_string(),
        ]
    }

    fn small_engine() -> SemanticSearchEngine {
        let mut w2v_cfg = Word2VecConfig::default();
        w2v_cfg.min_count = 1;
        w2v_cfg.dimension = 20;
        w2v_cfg.epochs = 3;
        w2v_cfg.num_threads = 1;
        let hnsw_cfg = HNSWConfig {
            m: 4,
            ef_construction: 20,
            ..Default::default()
        };
        SemanticSearchEngine::with_config(w2v_cfg, hnsw_cfg)
    }

    // ── train ────────────────────────────────────────────────────────────────

    #[test]
    fn test_train_empty_corpus() {
        let mut engine = small_engine();
        let err = engine.train(&[]).unwrap_err();
        assert!(matches!(err, SemanticSearchError::InvalidInput(_)));
    }

    #[test]
    fn test_train_returns_model_info() {
        let mut engine = small_engine();
        let info = engine.train(&small_corpus()).unwrap();
        assert!(info.vocab_size > 0, "vocabulary should be non-empty after training");
        assert_eq!(info.dimension, 20);
        assert_eq!(info.corpus_size, small_corpus().len());
    }

    #[test]
    fn test_train_populates_model_info() {
        let mut engine = small_engine();
        assert!(engine.model_info().is_none(), "before training model_info should be None");
        engine.train(&small_corpus()).unwrap();
        let info = engine.model_info().unwrap();
        assert!(info.vocab_size > 0);
        assert_eq!(info.corpus_size, small_corpus().len());
    }

    #[test]
    fn test_train_single_document() {
        let mut engine = small_engine();
        let corpus = vec!["hello world".to_string()];
        let info = engine.train(&corpus).unwrap();
        assert_eq!(info.corpus_size, 1);
    }

    // ── embed ────────────────────────────────────────────────────────────────

    #[test]
    fn test_embed_not_trained_returns_error() {
        let engine = small_engine();
        let err = engine.embed("test text").unwrap_err();
        assert!(matches!(err, SemanticSearchError::NotTrained));
    }

    #[test]
    fn test_embed_returns_correct_dimension() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        let emb = engine.embed("machine learning").unwrap();
        assert_eq!(emb.len(), 20);
    }

    #[test]
    fn test_embed_produces_finite_values() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        let emb = engine.embed("neural network").unwrap();
        assert!(emb.iter().all(|v| v.is_finite()), "embedding should contain only finite values");
    }

    #[test]
    fn test_embed_unknown_text_returns_zero_vector() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        // All tokens unknown → embed_text returns a zero vector
        let emb = engine.embed("xyzzy quux frobnicator").unwrap();
        assert_eq!(emb.len(), 20);
        assert!(emb.iter().all(|v| *v == 0.0));
    }

    // ── search ───────────────────────────────────────────────────────────────

    #[test]
    fn test_search_not_trained_returns_error() {
        let engine = small_engine();
        let err = engine.search("machine learning", 2).unwrap_err();
        assert!(matches!(err, SemanticSearchError::NotTrained));
    }

    #[test]
    fn test_search_k_zero_returns_empty() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        let results = engine.search("machine learning", 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_returns_at_most_k_results() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        let results = engine.search("machine learning", 3).unwrap();
        assert!(results.len() <= 3);
    }

    #[test]
    fn test_search_results_ordered_by_distance() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        let results = engine.search("neural network", 4).unwrap();
        for w in results.windows(2) {
            assert!(
                w[0].distance <= w[1].distance,
                "results should be ordered nearest-first: {} > {}",
                w[0].distance,
                w[1].distance
            );
        }
    }

    #[test]
    fn test_search_ids_are_valid_corpus_indices() {
        let corpus = small_corpus();
        let mut engine = small_engine();
        engine.train(&corpus).unwrap();
        let results = engine.search("learning", 4).unwrap();
        for r in &results {
            assert!(r.id < corpus.len(), "result id {} out of range", r.id);
            assert_eq!(r.content, corpus[r.id]);
        }
    }

    #[test]
    fn test_search_content_matches_corpus() {
        let corpus = small_corpus();
        let mut engine = small_engine();
        engine.train(&corpus).unwrap();
        let results = engine.search("deep learning", 2).unwrap();
        for r in &results {
            assert_eq!(r.content, corpus[r.id]);
        }
    }

    // ── has_oov ──────────────────────────────────────────────────────────────

    #[test]
    fn test_has_oov_untrained_returns_true() {
        let engine = small_engine();
        assert!(engine.has_oov("anything"), "untrained engine should report OOV");
    }

    #[test]
    fn test_has_oov_known_word_returns_false() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        // "machine" and "learning" are in the corpus
        assert!(!engine.has_oov("machine learning"));
    }

    #[test]
    fn test_has_oov_unknown_word_returns_true() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        assert!(engine.has_oov("xyzzy_completely_unknown_token"));
    }

    #[test]
    fn test_has_oov_only_stop_words_returns_false() {
        let mut engine = small_engine();
        engine.train(&small_corpus()).unwrap();
        // stop-words are not considered OOV
        assert!(!engine.has_oov("the and is a with"));
    }

    // ── save / load ──────────────────────────────────────────────────────────

    #[test]
    fn test_save_not_trained_returns_error() {
        let engine = small_engine();
        let err = engine.save("/tmp/gfs_test_untrained").unwrap_err();
        assert!(matches!(err, SemanticSearchError::NotTrained));
    }

    #[test]
    fn test_save_load_roundtrip() {
        let corpus = small_corpus();
        let mut engine = small_engine();
        let info_before = engine.train(&corpus).unwrap();

        let path = "/tmp/gfs_sse_test";
        engine.save(path).unwrap();

        let mut engine2 = small_engine();
        let info_after = engine2.load(path).unwrap();

        assert_eq!(info_before.dimension, info_after.dimension);
        assert_eq!(info_before.vocab_size, info_after.vocab_size);
        assert_eq!(info_before.corpus_size, info_after.corpus_size);

        // Search should work after reload
        let results = engine2.search("machine learning", 2).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert_eq!(r.content, corpus[r.id]);
        }

        // Clean up
        for ext in &["w2v", "hnsw", "corpus"] {
            std::fs::remove_file(format!("{path}.{ext}")).ok();
        }
    }

    #[test]
    fn test_load_nonexistent_file_returns_io_error() {
        let mut engine = small_engine();
        let err = engine.load("/tmp/this_file_does_not_exist_gfs_sse").unwrap_err();
        assert!(matches!(err, SemanticSearchError::Io(_)));
    }

    #[test]
    fn test_corpus_serialization_roundtrip() {
        let corpus = vec![
            "hello world".to_string(),
            "café résumé naïve".to_string(), // non-ASCII UTF-8
            "".to_string(),                   // empty document
            "a b c d e f".to_string(),
        ];
        let path = "/tmp/gfs_corpus_test.corpus";
        SemanticSearchEngine::write_corpus(&corpus, path).unwrap();
        let loaded = SemanticSearchEngine::read_corpus(path).unwrap();
        assert_eq!(corpus, loaded);
        std::fs::remove_file(path).ok();
    }
}
