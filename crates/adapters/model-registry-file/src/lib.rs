//! Filesystem-based model registry implementation.
//!
//! Stores model artifacts in `.gfs/models/` directory with JSON metadata.

use std::path::PathBuf;
use std::fs;
use std::io;

use async_trait::async_trait;
use gfs_domain::model::errors::DomainError;
use gfs_domain::model::model_artifact::ModelArtifact;
use gfs_domain::ports::model_registry::{ModelQuery, ModelRegistry};

/// Filesystem-based model registry.
///
/// Stores models in the following structure:
/// ```text
/// .gfs/models/
///   ├── {model_id}/
///   │   ├── artifact.json  (ModelArtifact metadata)
///   │   └── files/         (Optional: model files referenced by model_path)
///   └── ...
/// ```
pub struct FileModelRegistry {
    /// Base directory for models (typically `.gfs/models`)
    base_dir: PathBuf,
}

impl FileModelRegistry {
    /// Create a new filesystem model registry.
    ///
    /// # Arguments
    /// * `base_dir` - Base directory for storing models (will be created if it doesn't exist)
    pub fn new(base_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let base_dir = base_dir.into();

        // Create base directory if it doesn't exist
        if !base_dir.exists() {
            fs::create_dir_all(&base_dir)?;
        }

        Ok(Self { base_dir })
    }

    /// Get the directory path for a model ID.
    fn model_dir(&self, id: &str) -> PathBuf {
        self.base_dir.join(id)
    }

    /// Get the artifact.json path for a model ID.
    fn artifact_path(&self, id: &str) -> PathBuf {
        self.model_dir(id).join("artifact.json")
    }

    /// Load a model artifact from disk.
    fn load_artifact(&self, id: &str) -> io::Result<Option<ModelArtifact>> {
        let path = self.artifact_path(id);

        if !path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&path)?;
        let artifact: ModelArtifact = serde_json::from_str(&contents)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(Some(artifact))
    }

    /// Save a model artifact to disk.
    fn save_artifact(&self, artifact: &ModelArtifact) -> io::Result<()> {
        let model_dir = self.model_dir(&artifact.id);

        // Create model directory if it doesn't exist
        if !model_dir.exists() {
            fs::create_dir_all(&model_dir)?;
        }

        // Write artifact.json
        let artifact_path = self.artifact_path(&artifact.id);
        let json = serde_json::to_string_pretty(artifact)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        fs::write(&artifact_path, json)?;

        Ok(())
    }

    /// List all model IDs.
    fn list_model_ids(&self) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();

        if !self.base_dir.exists() {
            return Ok(ids);
        }

        for entry in fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Check if artifact.json exists
                    if path.join("artifact.json").exists() {
                        ids.push(name.to_string());
                    }
                }
            }
        }

        Ok(ids)
    }
}

#[async_trait]
impl ModelRegistry for FileModelRegistry {
    async fn register_model(&self, artifact: ModelArtifact) -> Result<(), DomainError> {
        // Check if model already exists
        if self.artifact_path(&artifact.id).exists() {
            return Err(DomainError::AlreadyExists(format!(
                "Model with ID '{}' already exists",
                artifact.id
            )));
        }

        self.save_artifact(&artifact)
            .map_err(|e| DomainError::Internal(format!("Failed to save model artifact: {}", e)))?;

        Ok(())
    }

    async fn get_model(&self, id: &str) -> Result<Option<ModelArtifact>, DomainError> {
        self.load_artifact(id)
            .map_err(|e| DomainError::Internal(format!("Failed to load model artifact: {}", e)))
    }

    async fn list_models(&self) -> Result<Vec<ModelArtifact>, DomainError> {
        let ids = self.list_model_ids()
            .map_err(|e| DomainError::Internal(format!("Failed to list model IDs: {}", e)))?;

        let mut models = Vec::new();
        for id in ids {
            if let Some(artifact) = self.load_artifact(&id)
                .map_err(|e| DomainError::Internal(format!("Failed to load model '{}': {}", id, e)))? {
                models.push(artifact);
            }
        }

        // Sort by trained_at (newest first)
        models.sort_by(|a, b| b.trained_at.cmp(&a.trained_at));

        Ok(models)
    }

    async fn delete_model(&self, id: &str) -> Result<(), DomainError> {
        let model_dir = self.model_dir(id);

        if !model_dir.exists() {
            return Err(DomainError::NotFound(format!(
                "Model with ID '{}' not found",
                id
            )));
        }

        fs::remove_dir_all(&model_dir)
            .map_err(|e| DomainError::Internal(format!("Failed to delete model directory: {}", e)))?;

        Ok(())
    }

    async fn search_models(&self, query: &ModelQuery) -> Result<Vec<ModelArtifact>, DomainError> {
        // Get all models
        let all_models = self.list_models().await?;

        // Filter using query.matches()
        let filtered: Vec<ModelArtifact> = all_models
            .into_iter()
            .filter(|artifact| query.matches(artifact))
            .collect();

        Ok(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;
    use gfs_domain::model::model_artifact::{ModelConfig, TrainingDataInfo};
    use tempfile::TempDir;

    fn create_test_artifact(id: &str) -> ModelArtifact {
        ModelArtifact {
            id: id.to_string(),
            version: "1.0.0".to_string(),
            name: Some(format!("test_model_{}", id)),
            description: Some("A test model".to_string()),
            trained_at: Utc::now(),
            trained_by: "testuser@testhost".to_string(),
            config: ModelConfig {
                dimension: 128,
                epochs: 10,
                learning_rate: 0.025,
                hyperparameters: HashMap::new(),
            },
            training_data: TrainingDataInfo {
                num_nodes: 1000,
                num_edges: 5000,
                tables: vec!["products".to_string(), "categories".to_string()],
                row_count: 1000,
                split_ratios: Some([0.8, 0.1, 0.1]),
            },
            metrics: HashMap::new(),
            model_path: PathBuf::from(format!("models/{}", id)),
            commit_hash: Some("abc123".to_string()),
            tags: vec!["test".to_string()],
        }
    }

    #[tokio::test]
    async fn test_register_and_get_model() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let artifact = create_test_artifact("model1");

        // Register model
        registry.register_model(artifact.clone()).await.unwrap();

        // Get model
        let retrieved = registry.get_model("model1").await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, artifact.id);
        assert_eq!(retrieved.version, artifact.version);
    }

    #[tokio::test]
    async fn test_register_duplicate_fails() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let artifact = create_test_artifact("model1");

        // Register first time
        registry.register_model(artifact.clone()).await.unwrap();

        // Register again should fail
        let result = registry.register_model(artifact).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_models() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        // Register multiple models
        registry.register_model(create_test_artifact("model1")).await.unwrap();
        registry.register_model(create_test_artifact("model2")).await.unwrap();
        registry.register_model(create_test_artifact("model3")).await.unwrap();

        // List all models
        let models = registry.list_models().await.unwrap();
        assert_eq!(models.len(), 3);

        let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
        assert!(ids.contains(&"model1".to_string()));
        assert!(ids.contains(&"model2".to_string()));
        assert!(ids.contains(&"model3".to_string()));
    }

    #[tokio::test]
    async fn test_delete_model() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let artifact = create_test_artifact("model1");
        registry.register_model(artifact).await.unwrap();

        // Delete model
        registry.delete_model("model1").await.unwrap();

        // Verify it's gone
        let retrieved = registry.get_model("model1").await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_model_fails() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let result = registry.delete_model("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_models_by_dimension() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        // Create models with different dimensions
        let mut model1 = create_test_artifact("model1");
        model1.config.dimension = 64;

        let mut model2 = create_test_artifact("model2");
        model2.config.dimension = 128;

        let mut model3 = create_test_artifact("model3");
        model3.config.dimension = 256;

        registry.register_model(model1).await.unwrap();
        registry.register_model(model2).await.unwrap();
        registry.register_model(model3).await.unwrap();

        // Search for models with dimension >= 128
        let query = ModelQuery::new().dimension_range(128, usize::MAX);
        let results = registry.search_models(&query).await.unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|m| m.config.dimension >= 128));
    }

    #[tokio::test]
    async fn test_search_models_by_tables() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let mut model1 = create_test_artifact("model1");
        model1.training_data.tables = vec!["products".to_string(), "categories".to_string()];

        let mut model2 = create_test_artifact("model2");
        model2.training_data.tables = vec!["products".to_string()];

        registry.register_model(model1).await.unwrap();
        registry.register_model(model2).await.unwrap();

        // Search for models trained on "products" AND "categories"
        let query = ModelQuery::new().with_tables(vec!["products".to_string(), "categories".to_string()]);
        let results = registry.search_models(&query).await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "model1");
    }

    #[tokio::test]
    async fn test_get_nonexistent_model_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let result = registry.get_model("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_exists() {
        let temp_dir = TempDir::new().unwrap();
        let registry = FileModelRegistry::new(temp_dir.path()).unwrap();

        let artifact = create_test_artifact("model1");
        registry.register_model(artifact).await.unwrap();

        assert!(registry.exists("model1").await.unwrap());
        assert!(!registry.exists("nonexistent").await.unwrap());
    }
}
