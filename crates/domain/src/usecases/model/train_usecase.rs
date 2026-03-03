//! Use case for training GNN models and linking them to GFS commits.
//!
//! Provides integration between model training and version control, enabling
//! model provenance tracking and reproducibility.

use std::sync::Arc;
use std::path::PathBuf;

use crate::model::errors::DomainError;
use crate::model::model_artifact::ModelArtifact;
use crate::ports::model_registry::ModelRegistry;
use crate::ports::repository::Repository;

/// Use case for training models with commit tracking.
///
/// This use case orchestrates model training and links the trained model
/// to the current GFS commit, enabling:
/// - Model provenance tracking
/// - Reproducibility (checkout commit → retrain)
/// - Model-data lineage visualization
pub struct TrainModelUseCase<R: Repository> {
    repository: Arc<R>,
    model_registry: Arc<dyn ModelRegistry>,
    repo_path: PathBuf,
}

impl<R: Repository> TrainModelUseCase<R> {
    /// Create a new train model use case.
    ///
    /// # Arguments
    /// * `repository` - GFS repository for commit tracking
    /// * `model_registry` - Model registry for storing trained models
    /// * `repo_path` - Path to the GFS repository
    pub fn new(repository: Arc<R>, model_registry: Arc<dyn ModelRegistry>, repo_path: PathBuf) -> Self {
        Self {
            repository,
            model_registry,
            repo_path,
        }
    }

    /// Get the current commit hash from the repository.
    ///
    /// # Returns
    /// * `Ok(Some(hash))` if HEAD points to a commit
    /// * `Ok(None)` if repository is in initial state (no commits yet)
    /// * `Err(DomainError)` if retrieval failed
    pub async fn get_current_commit_hash(&self) -> Result<Option<String>, DomainError> {
        // Get current commit ID
        let commit_id = self.repository
            .get_current_commit_id(&self.repo_path)
            .await
            .map_err(|e| DomainError::Internal(format!("Failed to get current commit: {}", e)))?;

        // "0" means no commits yet (initial state)
        if commit_id == "0" {
            Ok(None)
        } else {
            Ok(Some(commit_id))
        }
    }

    /// Link a trained model to the current commit.
    ///
    /// This method updates a ModelArtifact with the current commit hash
    /// and registers it in the model registry.
    ///
    /// # Arguments
    /// * `artifact` - The model artifact to link (commit_hash will be updated)
    ///
    /// # Returns
    /// * `Ok(artifact)` with updated commit_hash
    /// * `Err(DomainError)` if linking failed
    pub async fn link_model_to_current_commit(
        &self,
        mut artifact: ModelArtifact,
    ) -> Result<ModelArtifact, DomainError> {
        // Get current commit hash
        let commit_hash = self.get_current_commit_hash().await?;

        // Update artifact with commit hash
        artifact.commit_hash = commit_hash;

        // Register the model
        self.model_registry.register_model(artifact.clone()).await?;

        Ok(artifact)
    }

    /// Find models trained on a specific commit.
    ///
    /// # Arguments
    /// * `commit_hash` - The commit hash to search for
    ///
    /// # Returns
    /// * `Ok(Vec<artifact>)` with models trained on that commit
    /// * `Err(DomainError)` if search failed
    pub async fn find_models_by_commit(
        &self,
        commit_hash: &str,
    ) -> Result<Vec<ModelArtifact>, DomainError> {
        use crate::ports::model_registry::ModelQuery;

        let mut query = ModelQuery::new();
        query.commit_hash = Some(commit_hash.to_string());

        self.model_registry.search_models(&query).await
    }

    /// Verify if a model can be reproduced.
    ///
    /// Checks if the commit referenced by the model still exists in the repository.
    ///
    /// # Arguments
    /// * `artifact` - The model artifact to verify
    ///
    /// # Returns
    /// * `Ok(true)` if the model can be reproduced (commit exists)
    /// * `Ok(false)` if the commit no longer exists
    /// * `Err(DomainError)` if verification failed
    pub async fn can_reproduce_model(&self, artifact: &ModelArtifact) -> Result<bool, DomainError> {
        match &artifact.commit_hash {
            Some(hash) => {
                // Try to parse the revision (commit hash)
                match self.repository.rev_parse(&self.repo_path, hash).await {
                    Ok(_) => Ok(true),
                    Err(_) => Ok(false),
                }
            }
            None => {
                // No commit hash recorded, cannot reproduce
                Ok(false)
            }
        }
    }

    /// Create a ModelArtifact from training results.
    ///
    /// Helper method to construct a ModelArtifact with proper metadata.
    ///
    /// # Arguments
    /// * `model_path` - Path where model files were saved
    /// * `version` - Model version string
    /// * `config` - Model configuration
    /// * `training_data` - Training data summary
    /// * `metrics` - Performance metrics
    ///
    /// # Returns
    /// * `Ok(artifact)` ready to be registered
    pub fn create_artifact(
        &self,
        model_path: PathBuf,
        version: String,
        config: crate::model::model_artifact::ModelConfig,
        training_data: crate::model::model_artifact::TrainingDataInfo,
        metrics: std::collections::HashMap<String, f32>,
    ) -> ModelArtifact {
        ModelArtifact {
            id: ModelArtifact::generate_id(),
            version,
            name: None,
            description: None,
            trained_at: chrono::Utc::now(),
            trained_by: ModelArtifact::current_user(),
            config,
            training_data,
            metrics,
            model_path,
            commit_hash: None, // Will be set by link_model_to_current_commit
            tags: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::model::model_artifact::{ModelConfig, TrainingDataInfo};
    use crate::ports::model_registry::ModelQuery;
    use async_trait::async_trait;

    // Mock implementations for testing
    struct MockRepository {
        current_commit: String,
        valid_commits: std::collections::HashSet<String>,
    }

    #[async_trait]
    impl Repository for MockRepository {
        async fn init(&self, _path: &std::path::Path, _mount_point: Option<String>) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn get_workspace_data_dir_for_head(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<PathBuf> {
            unimplemented!()
        }

        async fn get_active_workspace_data_dir(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<PathBuf> {
            unimplemented!()
        }

        async fn update_environment_config(&self, _repo: &std::path::Path, _config: crate::model::config::EnvironmentConfig) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn update_runtime_config(&self, _repo: &std::path::Path, _config: crate::model::config::RuntimeConfig) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn clone_repo(&self, _url: &str, _target: &std::path::Path) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn commit(&self, _repo: &std::path::Path, _new_commit: crate::model::commit::NewCommit) -> crate::ports::repository::Result<String> {
            unimplemented!()
        }

        async fn checkout(&self, _repo: &std::path::Path, _revision: &str) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn create_branch(&self, _repo: &std::path::Path, _name: &str, _commit_hash: &str) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn log(&self, _repo: &std::path::Path, _options: crate::ports::repository::LogOptions) -> crate::ports::repository::Result<Vec<crate::model::commit::CommitWithRefs>> {
            unimplemented!()
        }

        async fn rev_parse(&self, _repo: &std::path::Path, revision: &str) -> crate::ports::repository::Result<String> {
            if self.valid_commits.contains(revision) {
                Ok(revision.to_string())
            } else {
                Err(crate::ports::repository::RepositoryError::RevisionNotFound(revision.to_string()))
            }
        }

        async fn push(&self, _repo: &std::path::Path, _options: crate::ports::repository::RemoteOptions) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn pull(&self, _repo: &std::path::Path, _options: crate::ports::repository::RemoteOptions) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn fetch(&self, _repo: &std::path::Path, _options: crate::ports::repository::RemoteOptions) -> crate::ports::repository::Result<()> {
            unimplemented!()
        }

        async fn get_current_branch(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<String> {
            unimplemented!()
        }

        async fn get_current_commit_id(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<String> {
            Ok(self.current_commit.clone())
        }

        async fn get_runtime_config(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<Option<crate::model::config::RuntimeConfig>> {
            unimplemented!()
        }

        async fn get_mount_point(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<Option<String>> {
            unimplemented!()
        }

        async fn get_environment_config(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<Option<crate::model::config::EnvironmentConfig>> {
            unimplemented!()
        }

        async fn get_user_config(&self, _repo: &std::path::Path) -> crate::ports::repository::Result<Option<crate::model::config::UserConfig>> {
            unimplemented!()
        }

        async fn ensure_snapshot_path(&self, _repo: &std::path::Path, _hash: &str) -> crate::ports::repository::Result<PathBuf> {
            unimplemented!()
        }
    }

    struct MockModelRegistry {
        models: std::sync::Mutex<Vec<ModelArtifact>>,
    }

    impl MockModelRegistry {
        fn new() -> Self {
            Self {
                models: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ModelRegistry for MockModelRegistry {
        async fn register_model(&self, artifact: ModelArtifact) -> Result<(), DomainError> {
            let mut models = self.models.lock().unwrap();
            models.push(artifact);
            Ok(())
        }

        async fn get_model(&self, id: &str) -> Result<Option<ModelArtifact>, DomainError> {
            let models = self.models.lock().unwrap();
            Ok(models.iter().find(|m| m.id == id).cloned())
        }

        async fn list_models(&self) -> Result<Vec<ModelArtifact>, DomainError> {
            let models = self.models.lock().unwrap();
            Ok(models.clone())
        }

        async fn delete_model(&self, id: &str) -> Result<(), DomainError> {
            let mut models = self.models.lock().unwrap();
            let len_before = models.len();
            models.retain(|m| m.id != id);
            if models.len() == len_before {
                Err(DomainError::NotFound(format!("Model '{}' not found", id)))
            } else {
                Ok(())
            }
        }

        async fn search_models(&self, query: &ModelQuery) -> Result<Vec<ModelArtifact>, DomainError> {
            let models = self.models.lock().unwrap();
            Ok(models.iter().filter(|m| query.matches(m)).cloned().collect())
        }
    }

    fn create_test_artifact() -> ModelArtifact {
        ModelArtifact {
            id: "test_model_1".to_string(),
            version: "1.0.0".to_string(),
            name: Some("Test Model".to_string()),
            description: None,
            trained_at: chrono::Utc::now(),
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
                tables: vec!["products".to_string()],
                row_count: 1000,
                split_ratios: Some([0.8, 0.1, 0.1]),
            },
            metrics: HashMap::new(),
            model_path: PathBuf::from("models/test_model_1"),
            commit_hash: None,
            tags: vec![],
        }
    }

    #[tokio::test]
    async fn test_link_model_to_current_commit() {
        let mut valid_commits = std::collections::HashSet::new();
        valid_commits.insert("abc123def456".to_string());

        let repo = Arc::new(MockRepository {
            current_commit: "abc123def456".to_string(),
            valid_commits,
        });
        let registry = Arc::new(MockModelRegistry::new());
        let repo_path = PathBuf::from("/test/repo");
        let usecase = TrainModelUseCase::new(repo, registry.clone(), repo_path);

        let artifact = create_test_artifact();
        let linked = usecase.link_model_to_current_commit(artifact).await.unwrap();

        assert_eq!(linked.commit_hash, Some("abc123def456".to_string()));

        // Verify it was registered
        let registered = registry.get_model(&linked.id).await.unwrap();
        assert!(registered.is_some());
    }

    #[tokio::test]
    async fn test_link_model_no_commit() {
        let repo = Arc::new(MockRepository {
            current_commit: "0".to_string(), // "0" means no commits
            valid_commits: std::collections::HashSet::new(),
        });
        let registry = Arc::new(MockModelRegistry::new());
        let repo_path = PathBuf::from("/test/repo");
        let usecase = TrainModelUseCase::new(repo, registry, repo_path);

        let artifact = create_test_artifact();
        let linked = usecase.link_model_to_current_commit(artifact).await.unwrap();

        assert_eq!(linked.commit_hash, None);
    }

    #[tokio::test]
    async fn test_can_reproduce_model() {
        let mut valid_commits = std::collections::HashSet::new();
        valid_commits.insert("abc123".to_string());

        let repo = Arc::new(MockRepository {
            current_commit: "abc123".to_string(),
            valid_commits,
        });
        let registry = Arc::new(MockModelRegistry::new());
        let repo_path = PathBuf::from("/test/repo");
        let usecase = TrainModelUseCase::new(repo, registry, repo_path);

        let mut artifact = create_test_artifact();
        artifact.commit_hash = Some("abc123".to_string());

        let can_reproduce = usecase.can_reproduce_model(&artifact).await.unwrap();
        assert!(can_reproduce);
    }

    #[tokio::test]
    async fn test_cannot_reproduce_model_missing_commit() {
        let mut valid_commits = std::collections::HashSet::new();
        valid_commits.insert("xyz789".to_string());

        let repo = Arc::new(MockRepository {
            current_commit: "xyz789".to_string(),
            valid_commits,
        });
        let registry = Arc::new(MockModelRegistry::new());
        let repo_path = PathBuf::from("/test/repo");
        let usecase = TrainModelUseCase::new(repo, registry, repo_path);

        let mut artifact = create_test_artifact();
        artifact.commit_hash = Some("abc123".to_string()); // Different commit

        let can_reproduce = usecase.can_reproduce_model(&artifact).await.unwrap();
        assert!(!can_reproduce);
    }

    #[tokio::test]
    async fn test_create_artifact() {
        let repo = Arc::new(MockRepository {
            current_commit: "abc123".to_string(),
            valid_commits: std::collections::HashSet::new(),
        });
        let registry = Arc::new(MockModelRegistry::new());
        let repo_path = PathBuf::from("/test/repo");
        let usecase = TrainModelUseCase::new(repo, registry, repo_path);

        let config = ModelConfig {
            dimension: 128,
            epochs: 10,
            learning_rate: 0.025,
            hyperparameters: HashMap::new(),
        };

        let training_data = TrainingDataInfo {
            num_nodes: 1000,
            num_edges: 5000,
            tables: vec!["products".to_string()],
            row_count: 1000,
            split_ratios: Some([0.8, 0.1, 0.1]),
        };

        let artifact = usecase.create_artifact(
            PathBuf::from("models/test"),
            "1.0.0".to_string(),
            config,
            training_data,
            HashMap::new(),
        );

        assert_eq!(artifact.version, "1.0.0");
        assert!(artifact.id.starts_with("gnn_"));
        assert!(artifact.trained_by.contains('@'));
    }
}
