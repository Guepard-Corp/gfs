//! Checkpoint management for training.
//!
//! Provides utilities for saving and loading model checkpoints during training,
//! enabling resumption of interrupted training sessions.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::graph::GraphSemanticEngine;
use super::config::CheckpointConfig;
use super::metrics::MetricsCollector;

/// Checkpoint manager for saving/loading training state.
///
/// Manages model checkpoints including the model state, metrics, and metadata.
/// Automatically handles checkpoint rotation based on configuration.
pub struct CheckpointManager {
    config: CheckpointConfig,
}

impl CheckpointManager {
    /// Create a new checkpoint manager.
    ///
    /// # Arguments
    ///
    /// * `config` - Checkpoint configuration
    pub fn new(config: CheckpointConfig) -> Self {
        Self { config }
    }

    /// Save a checkpoint for the current epoch.
    ///
    /// Creates a directory with the model files, metrics, and metadata.
    ///
    /// # Arguments
    ///
    /// * `epoch` - Current epoch number
    /// * `engine` - Trained GraphSemanticEngine
    /// * `metrics` - MetricsCollector with training history
    ///
    /// # Returns
    ///
    /// Path to the created checkpoint directory
    ///
    /// # Errors
    ///
    /// Returns error if checkpoint cannot be saved.
    pub fn save_checkpoint(
        &self,
        epoch: usize,
        engine: &GraphSemanticEngine,
        metrics: &MetricsCollector,
    ) -> Result<PathBuf, CheckpointError> {
        // Create checkpoint directory
        let checkpoint_dir = self
            .config
            .checkpoint_dir
            .join(format!("checkpoint_epoch_{:04}", epoch));

        fs::create_dir_all(&checkpoint_dir).map_err(|e| {
            CheckpointError::IoError(format!("failed to create checkpoint dir: {}", e))
        })?;

        // Save model files (w2v, hnsw, gnodes)
        let model_path = checkpoint_dir.join("model");
        engine.save(model_path.to_str().unwrap()).map_err(|e| {
            CheckpointError::ModelError(format!("failed to save model: {}", e))
        })?;

        // Save metrics
        let metrics_path = checkpoint_dir.join("metrics.json");
        let metrics_json = serde_json::to_string_pretty(metrics).map_err(|e| {
            CheckpointError::SerializationError(format!("failed to serialize metrics: {}", e))
        })?;
        fs::write(&metrics_path, metrics_json).map_err(|e| {
            CheckpointError::IoError(format!("failed to write metrics: {}", e))
        })?;

        // Save metadata
        let metadata = CheckpointMetadata {
            epoch,
            timestamp: SystemTime::now(),
            model_dimension: engine.model_info().map(|i| i.dimension).unwrap_or(0),
            vocab_size: engine.model_info().map(|i| i.vocab_size).unwrap_or(0),
        };

        let metadata_path = checkpoint_dir.join("metadata.json");
        let metadata_json = serde_json::to_string_pretty(&metadata).map_err(|e| {
            CheckpointError::SerializationError(format!("failed to serialize metadata: {}", e))
        })?;
        fs::write(&metadata_path, metadata_json).map_err(|e| {
            CheckpointError::IoError(format!("failed to write metadata: {}", e))
        })?;

        Ok(checkpoint_dir)
    }

    /// Load the latest checkpoint.
    ///
    /// Searches for the most recent checkpoint and loads the model and metrics.
    ///
    /// # Arguments
    ///
    /// * `engine` - GraphSemanticEngine to load into
    ///
    /// # Returns
    ///
    /// `Some((epoch, metrics))` if checkpoint found, `None` otherwise
    ///
    /// # Errors
    ///
    /// Returns error if checkpoint loading fails.
    pub fn load_latest_checkpoint(
        &self,
        engine: &mut GraphSemanticEngine,
    ) -> Result<Option<(usize, MetricsCollector)>, CheckpointError> {
        let checkpoints = self.list_checkpoints()?;

        if checkpoints.is_empty() {
            return Ok(None);
        }

        // Get latest checkpoint (highest epoch)
        let latest = checkpoints
            .iter()
            .max_by_key(|c| c.epoch)
            .ok_or_else(|| CheckpointError::NotFound("no checkpoints found".to_string()))?;

        self.load_checkpoint(engine, &latest.path)
    }

    /// Load a specific checkpoint.
    ///
    /// # Arguments
    ///
    /// * `engine` - GraphSemanticEngine to load into
    /// * `checkpoint_dir` - Path to checkpoint directory
    ///
    /// # Returns
    ///
    /// `(epoch, metrics)` tuple
    fn load_checkpoint(
        &self,
        engine: &mut GraphSemanticEngine,
        checkpoint_dir: &Path,
    ) -> Result<Option<(usize, MetricsCollector)>, CheckpointError> {
        // Load metadata
        let metadata_path = checkpoint_dir.join("metadata.json");
        let metadata_json = fs::read_to_string(&metadata_path).map_err(|e| {
            CheckpointError::IoError(format!("failed to read metadata: {}", e))
        })?;
        let metadata: CheckpointMetadata = serde_json::from_str(&metadata_json).map_err(|e| {
            CheckpointError::SerializationError(format!("failed to parse metadata: {}", e))
        })?;

        // Load model
        let model_path = checkpoint_dir.join("model");
        engine.load(model_path.to_str().unwrap()).map_err(|e| {
            CheckpointError::ModelError(format!("failed to load model: {}", e))
        })?;

        // Load metrics
        let metrics_path = checkpoint_dir.join("metrics.json");
        let metrics_json = fs::read_to_string(&metrics_path).map_err(|e| {
            CheckpointError::IoError(format!("failed to read metrics: {}", e))
        })?;
        let metrics: MetricsCollector = serde_json::from_str(&metrics_json).map_err(|e| {
            CheckpointError::SerializationError(format!("failed to parse metrics: {}", e))
        })?;

        Ok(Some((metadata.epoch, metrics)))
    }

    /// List all checkpoints in the checkpoint directory.
    ///
    /// # Returns
    ///
    /// Vector of checkpoint information sorted by epoch
    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>, CheckpointError> {
        if !self.config.checkpoint_dir.exists() {
            return Ok(vec![]);
        }

        let mut checkpoints = Vec::new();

        for entry in fs::read_dir(&self.config.checkpoint_dir).map_err(|e| {
            CheckpointError::IoError(format!("failed to read checkpoint dir: {}", e))
        })? {
            let entry = entry.map_err(|e| {
                CheckpointError::IoError(format!("failed to read dir entry: {}", e))
            })?;

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Parse epoch from directory name (checkpoint_epoch_XXXX)
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(epoch_str) = dir_name.strip_prefix("checkpoint_epoch_") {
                    if let Ok(epoch) = epoch_str.parse::<usize>() {
                        // Get metadata
                        let metadata_path = path.join("metadata.json");
                        let timestamp = if metadata_path.exists() {
                            let metadata_json = fs::read_to_string(&metadata_path)?;
                            let metadata: CheckpointMetadata =
                                serde_json::from_str(&metadata_json)?;
                            metadata.timestamp
                        } else {
                            SystemTime::now()
                        };

                        // Calculate size
                        let size_bytes = calculate_dir_size(&path)?;

                        checkpoints.push(CheckpointInfo {
                            epoch,
                            path,
                            timestamp,
                            size_bytes,
                        });
                    }
                }
            }
        }

        checkpoints.sort_by_key(|c| c.epoch);
        Ok(checkpoints)
    }

    /// Delete old checkpoints, keeping only the most recent N.
    ///
    /// Based on `config.max_checkpoints`.
    ///
    /// # Errors
    ///
    /// Returns error if checkpoint deletion fails.
    pub fn cleanup_old_checkpoints(&self) -> Result<(), CheckpointError> {
        let mut checkpoints = self.list_checkpoints()?;

        if checkpoints.len() <= self.config.max_checkpoints {
            return Ok(());
        }

        // Sort by epoch (oldest first)
        checkpoints.sort_by_key(|c| c.epoch);

        // Delete oldest checkpoints
        let to_delete = checkpoints.len() - self.config.max_checkpoints;
        for checkpoint in checkpoints.iter().take(to_delete) {
            fs::remove_dir_all(&checkpoint.path).map_err(|e| {
                CheckpointError::IoError(format!("failed to delete checkpoint: {}", e))
            })?;
        }

        Ok(())
    }
}

/// Information about a checkpoint.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    /// Epoch number
    pub epoch: usize,
    /// Path to checkpoint directory
    pub path: PathBuf,
    /// Creation timestamp
    pub timestamp: SystemTime,
    /// Total size in bytes
    pub size_bytes: u64,
}

/// Checkpoint metadata stored with each checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointMetadata {
    epoch: usize,
    timestamp: SystemTime,
    model_dimension: usize,
    vocab_size: usize,
}

/// Checkpoint errors.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("IO error: {0}")]
    IoError(String),

    #[error("serialization error: {0}")]
    SerializationError(String),

    #[error("model error: {0}")]
    ModelError(String),

    #[error("checkpoint not found: {0}")]
    NotFound(String),
}

impl From<std::io::Error> for CheckpointError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError(e.to_string())
    }
}

impl From<serde_json::Error> for CheckpointError {
    fn from(e: serde_json::Error) -> Self {
        Self::SerializationError(e.to_string())
    }
}

/// Calculate total size of a directory recursively.
fn calculate_dir_size(path: &Path) -> Result<u64, std::io::Error> {
    let mut size = 0;

    if path.is_file() {
        return Ok(path.metadata()?.len());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            size += path.metadata()?.len();
        } else if path.is_dir() {
            size += calculate_dir_size(&path)?;
        }
    }

    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_checkpoint_manager_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config = CheckpointConfig {
            checkpoint_dir: temp_dir.path().to_path_buf(),
            checkpoint_interval: 1,
            max_checkpoints: 5,
        };

        let manager = CheckpointManager::new(config);
        assert_eq!(manager.config.max_checkpoints, 5);
    }

    #[test]
    fn test_list_checkpoints_empty() {
        let temp_dir = TempDir::new().unwrap();
        let config = CheckpointConfig {
            checkpoint_dir: temp_dir.path().to_path_buf(),
            checkpoint_interval: 1,
            max_checkpoints: 5,
        };

        let manager = CheckpointManager::new(config);
        let checkpoints = manager.list_checkpoints().unwrap();
        assert_eq!(checkpoints.len(), 0);
    }
}
