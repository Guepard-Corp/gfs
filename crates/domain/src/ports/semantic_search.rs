use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SemanticSearchError {
    #[error("model not trained")]
    NotTrained,

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("training failed: {0}")]
    TrainingFailed(String),

    #[error("embedding failed: {0}")]
    EmbeddingFailed(String),

    #[error("search failed: {0}")]
    SearchFailed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, SemanticSearchError>;

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// Represents a trained semantic search model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    /// Model identifier/version
    pub id: String,
    /// Dimensionality of the embedding vectors
    pub dimension: usize,
    /// Vocabulary size
    pub vocab_size: usize,
    /// Model metadata
    pub metadata: ModelMetadata,
}

/// Metadata about the model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Training corpus size
    pub corpus_size: usize,
    /// Training timestamp
    pub trained_at: chrono::DateTime<chrono::Utc>,
    /// Model parameters
    pub parameters: std::collections::HashMap<String, String>,
}

/// A single search result with similarity score
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Document/text ID or content
    pub content: String,
    /// Similarity score (higher is better, typically 0.0 to 1.0)
    pub score: f32,
    /// Embedding vector (optional)
    pub embedding: Option<Vec<f32>>,
    /// Additional metadata
    pub metadata: std::collections::HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Port for semantic search functionality using Word2Vec and HNSW
#[async_trait]
pub trait SemanticSearchEngine: Send + Sync {
    /// Train the model on a corpus of text documents
    ///
    /// # Arguments
    /// * `corpus` - Collection of text documents to train on
    ///
    /// # Returns
    /// The trained model information
    async fn train(&mut self, corpus: &[String]) -> Result<Model>;

    /// Generate an embedding vector for the given text
    ///
    /// # Arguments
    /// * `text` - Text to embed
    ///
    /// # Returns
    /// Vector of floats representing the text embedding
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Search for similar documents/texts
    ///
    /// # Arguments
    /// * `query` - Query text to search for
    /// * `k` - Number of results to return
    ///
    /// # Returns
    /// Vector of search results ordered by similarity (descending)
    async fn search(&self, query: &str, k: usize) -> Result<Vec<SearchResult>>;

    /// Load a pre-trained model
    async fn load_model(&mut self, model_id: &str) -> Result<Model>;

    /// Save the current model
    async fn save_model(&self, model_id: &str) -> Result<()>;
}
