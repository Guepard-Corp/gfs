//! Model registry port for managing trained GNN models.
//!
//! Provides an abstraction for storing, retrieving, and querying trained model artifacts.

use async_trait::async_trait;
use std::result::Result as StdResult;

use crate::model::model_artifact::ModelArtifact;
use crate::model::errors::DomainError;

/// Query parameters for searching models.
#[derive(Debug, Clone, Default)]
pub struct ModelQuery {
    /// Filter by trained date (after this date)
    pub trained_after: Option<chrono::DateTime<chrono::Utc>>,

    /// Filter by trained date (before this date)
    pub trained_before: Option<chrono::DateTime<chrono::Utc>>,

    /// Filter by user who trained the model
    pub trained_by: Option<String>,

    /// Filter by minimum embedding dimension
    pub min_dimension: Option<usize>,

    /// Filter by maximum embedding dimension
    pub max_dimension: Option<usize>,

    /// Filter by tables used in training (must include all)
    pub tables: Option<Vec<String>>,

    /// Filter by tags (must include all)
    pub tags: Option<Vec<String>>,

    /// Filter by model version
    pub version: Option<String>,

    /// Filter by commit hash
    pub commit_hash: Option<String>,
}

impl ModelQuery {
    /// Create a new empty query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter by trained date range.
    pub fn trained_after(mut self, date: chrono::DateTime<chrono::Utc>) -> Self {
        self.trained_after = Some(date);
        self
    }

    /// Filter by user.
    pub fn trained_by(mut self, user: impl Into<String>) -> Self {
        self.trained_by = Some(user.into());
        self
    }

    /// Filter by dimension range.
    pub fn dimension_range(mut self, min: usize, max: usize) -> Self {
        self.min_dimension = Some(min);
        self.max_dimension = Some(max);
        self
    }

    /// Filter by tables.
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = Some(tables);
        self
    }

    /// Filter by tags.
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = Some(tags);
        self
    }

    /// Check if a model matches this query.
    pub fn matches(&self, artifact: &ModelArtifact) -> bool {
        // Check trained_after
        if let Some(after) = self.trained_after {
            if artifact.trained_at < after {
                return false;
            }
        }

        // Check trained_before
        if let Some(before) = self.trained_before {
            if artifact.trained_at > before {
                return false;
            }
        }

        // Check trained_by
        if let Some(ref user) = self.trained_by {
            if &artifact.trained_by != user {
                return false;
            }
        }

        // Check min_dimension
        if let Some(min) = self.min_dimension {
            if artifact.config.dimension < min {
                return false;
            }
        }

        // Check max_dimension
        if let Some(max) = self.max_dimension {
            if artifact.config.dimension > max {
                return false;
            }
        }

        // Check tables (must include all)
        if let Some(ref tables) = self.tables {
            for table in tables {
                if !artifact.training_data.tables.contains(table) {
                    return false;
                }
            }
        }

        // Check tags (must include all)
        if let Some(ref tags) = self.tags {
            for tag in tags {
                if !artifact.tags.contains(tag) {
                    return false;
                }
            }
        }

        // Check version
        if let Some(ref version) = self.version {
            if &artifact.version != version {
                return false;
            }
        }

        // Check commit_hash
        if let Some(ref hash) = self.commit_hash {
            match &artifact.commit_hash {
                Some(artifact_hash) if artifact_hash == hash => {},
                _ => return false,
            }
        }

        true
    }
}

/// Port for managing trained model artifacts.
#[async_trait]
pub trait ModelRegistry: Send + Sync {
    /// Register a new trained model.
    ///
    /// # Arguments
    /// * `artifact` - The model artifact to register
    ///
    /// # Returns
    /// * `Ok(())` if the model was registered successfully
    /// * `Err(DomainError)` if registration failed
    async fn register_model(&self, artifact: ModelArtifact) -> StdResult<(), DomainError>;

    /// Get a model by its ID.
    ///
    /// # Arguments
    /// * `id` - The unique model ID
    ///
    /// # Returns
    /// * `Ok(Some(artifact))` if the model was found
    /// * `Ok(None)` if the model does not exist
    /// * `Err(DomainError)` if retrieval failed
    async fn get_model(&self, id: &str) -> StdResult<Option<ModelArtifact>, DomainError>;

    /// List all registered models.
    ///
    /// # Returns
    /// * `Ok(Vec<artifact>)` with all models
    /// * `Err(DomainError)` if listing failed
    async fn list_models(&self) -> StdResult<Vec<ModelArtifact>, DomainError>;

    /// Delete a model by its ID.
    ///
    /// # Arguments
    /// * `id` - The unique model ID
    ///
    /// # Returns
    /// * `Ok(())` if the model was deleted successfully
    /// * `Err(DomainError)` if deletion failed (including if model doesn't exist)
    async fn delete_model(&self, id: &str) -> StdResult<(), DomainError>;

    /// Search for models matching a query.
    ///
    /// # Arguments
    /// * `query` - The query parameters
    ///
    /// # Returns
    /// * `Ok(Vec<artifact>)` with matching models
    /// * `Err(DomainError)` if search failed
    async fn search_models(&self, query: &ModelQuery) -> StdResult<Vec<ModelArtifact>, DomainError>;

    /// Check if a model exists by ID.
    ///
    /// # Arguments
    /// * `id` - The unique model ID
    ///
    /// # Returns
    /// * `Ok(true)` if the model exists
    /// * `Ok(false)` if the model does not exist
    /// * `Err(DomainError)` if check failed
    async fn exists(&self, id: &str) -> StdResult<bool, DomainError> {
        self.get_model(id).await.map(|opt| opt.is_some())
    }
}
