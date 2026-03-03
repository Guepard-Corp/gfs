//! Training configuration structures.
//!
//! Defines configuration for GNN model training including model parameters,
//! data configuration, training parameters, checkpointing, and evaluation.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use crate::graph::GraphEngineConfig;

/// Complete training configuration for GNN models.
///
/// This structure combines all aspects of model training: the model architecture,
/// data selection, training hyperparameters, checkpointing, and evaluation metrics.
///
/// # Example
///
/// ```rust
/// use gfs_ai_models::training::TrainingConfig;
///
/// let config = TrainingConfig::default();
/// // Customize as needed
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrainingConfig {
    /// Model configuration (Word2Vec, HNSW, DeepWalk)
    pub model: GraphEngineConfig,

    /// Data configuration (table filtering, row limits, splits)
    pub data: DataConfig,

    /// Training parameters (progress, metrics)
    pub training: TrainingParams,

    /// Checkpointing configuration
    pub checkpoint: CheckpointConfig,

    /// Evaluation configuration
    pub evaluation: EvaluationConfig,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            model: GraphEngineConfig::default(),
            data: DataConfig::default(),
            training: TrainingParams::default(),
            checkpoint: CheckpointConfig::default(),
            evaluation: EvaluationConfig::default(),
        }
    }
}

impl TrainingConfig {
    /// Create a new training configuration with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load configuration from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns error if file cannot be read or parsed.
    pub fn load_from_toml(path: &std::path::Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::IoError(format!("failed to read config file: {}", e)))?;

        toml::from_str(&contents)
            .map_err(|e| ConfigError::ParseError(format!("failed to parse TOML: {}", e)))
    }

    /// Save configuration to a TOML file.
    ///
    /// # Errors
    ///
    /// Returns error if serialization or writing fails.
    pub fn save_to_toml(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        let contents = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::ParseError(format!("failed to serialize TOML: {}", e)))?;

        std::fs::write(path, contents)
            .map_err(|e| ConfigError::IoError(format!("failed to write config file: {}", e)))
    }
}

/// Data configuration for training.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataConfig {
    /// Which tables to include (None = all public schema tables)
    pub table_filter: Option<Vec<String>>,

    /// Max rows per table (None = all rows)
    pub row_limit: Option<usize>,

    /// Include FK relationships as edges
    #[serde(default = "default_true")]
    pub include_relationships: bool,

    /// Train/validation/test split ratios (must sum to 1.0)
    #[serde(default = "default_split_ratios")]
    pub split_ratios: [f32; 3],

    /// Random seed for data splitting (reproducibility)
    #[serde(default = "default_seed")]
    pub split_seed: u64,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            table_filter: None,
            row_limit: None,
            include_relationships: true,
            split_ratios: [0.8, 0.1, 0.1],
            split_seed: 42,
        }
    }
}

/// Training runtime parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingParams {
    /// Show progress bar during training
    #[serde(default = "default_true")]
    pub show_progress: bool,

    /// Compute evaluation metrics during training
    #[serde(default = "default_true")]
    pub compute_metrics: bool,

    /// Metric computation interval (every N epochs)
    #[serde(default = "default_metric_interval")]
    pub metric_interval: usize,
}

impl Default for TrainingParams {
    fn default() -> Self {
        Self {
            show_progress: true,
            compute_metrics: true,
            metric_interval: 1,
        }
    }
}

/// Checkpointing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// Directory to save checkpoints
    #[serde(default = "default_checkpoint_dir")]
    pub checkpoint_dir: PathBuf,

    /// Checkpoint every N epochs (0 = disabled)
    #[serde(default)]
    pub checkpoint_interval: usize,

    /// Max checkpoints to keep (older ones deleted)
    #[serde(default = "default_max_checkpoints")]
    pub max_checkpoints: usize,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            checkpoint_dir: PathBuf::from(".gfs/checkpoints"),
            checkpoint_interval: 0,
            max_checkpoints: 5,
        }
    }
}

/// Evaluation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationConfig {
    /// Metrics to compute
    #[serde(default = "default_metrics")]
    pub metrics: Vec<MetricType>,

    /// K values for top-K metrics (e.g., [1, 5, 10])
    #[serde(default = "default_top_k")]
    pub top_k_values: Vec<usize>,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            metrics: vec![MetricType::Precision, MetricType::Recall, MetricType::MRR],
            top_k_values: vec![1, 5, 10],
        }
    }
}

/// Metric types for evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricType {
    /// Precision@K
    Precision,
    /// Recall@K
    Recall,
    /// Mean Reciprocal Rank
    MRR,
    /// Normalized Discounted Cumulative Gain
    NDCG,
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    IoError(String),

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("validation error: {0}")]
    ValidationError(String),
}

// Default value functions for serde
fn default_true() -> bool {
    true
}

fn default_split_ratios() -> [f32; 3] {
    [0.8, 0.1, 0.1]
}

fn default_seed() -> u64 {
    42
}

fn default_metric_interval() -> usize {
    1
}

fn default_checkpoint_dir() -> PathBuf {
    PathBuf::from(".gfs/checkpoints")
}

fn default_max_checkpoints() -> usize {
    5
}

fn default_metrics() -> Vec<MetricType> {
    vec![MetricType::Precision, MetricType::Recall, MetricType::MRR]
}

fn default_top_k() -> Vec<usize> {
    vec![1, 5, 10]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TrainingConfig::default();
        assert_eq!(config.data.split_ratios, [0.8, 0.1, 0.1]);
        assert!(config.training.show_progress);
        assert_eq!(config.checkpoint.max_checkpoints, 5);
    }

    #[test]
    fn test_split_ratios_sum() {
        let config = TrainingConfig::default();
        let sum: f32 = config.data.split_ratios.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_toml_serialization() {
        let config = TrainingConfig::default();
        let toml = toml::to_string(&config).unwrap();
        assert!(toml.contains("split_ratios"));
        assert!(toml.contains("show_progress"));
    }

    #[test]
    fn test_toml_deserialization() {
        let toml = r#"
[model.w2v]
dimension = 128
window = 5
negative_samples = 15
min_count = 1
epochs = 10
num_threads = 8
learning_rate = 0.025
min_learning_rate = 0.0001
subsample_threshold = 1e-5

[data]
split_ratios = [0.7, 0.2, 0.1]

[training]
show_progress = false
        "#;

        let config: TrainingConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.w2v.dimension, 128);
        assert_eq!(config.data.split_ratios, [0.7, 0.2, 0.1]);
        assert!(!config.training.show_progress);
    }
}
