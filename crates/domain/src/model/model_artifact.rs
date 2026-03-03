//! Model artifact metadata and versioning.
//!
//! Tracks trained GNN models with metadata, provenance, and performance metrics.

use std::path::PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A trained model artifact with metadata and provenance.
///
/// Represents a single trained GNN model with all associated metadata including
/// when it was trained, who trained it, what data was used, and performance metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArtifact {
    /// Unique model identifier (generated)
    pub id: String,

    /// Model version (semantic versioning recommended, e.g., "1.0.0")
    pub version: String,

    /// Model name (optional, user-provided)
    pub name: Option<String>,

    /// Model description (optional, user-provided)
    pub description: Option<String>,

    /// When the model was trained
    pub trained_at: DateTime<Utc>,

    /// Who trained it (username@hostname)
    pub trained_by: String,

    /// Model configuration parameters
    pub config: ModelConfig,

    /// Training data summary
    pub training_data: TrainingDataInfo,

    /// Model performance metrics
    pub metrics: std::collections::HashMap<String, f32>,

    /// Path to model files (relative to model registry)
    pub model_path: PathBuf,

    /// GFS commit hash (if tracked in GFS)
    pub commit_hash: Option<String>,

    /// Tags for categorization (e.g., "production", "experimental")
    pub tags: Vec<String>,
}

impl ModelArtifact {
    /// Generate a unique model ID.
    ///
    /// Format: `gnn_<timestamp>_<random>`
    pub fn generate_id() -> String {
        use chrono::Utc;

        let timestamp = Utc::now().timestamp();
        let random: u32 = rand::random();
        format!("gnn_{}_{:08x}", timestamp, random)
    }

    /// Get the current user identifier.
    ///
    /// Format: `username@hostname`
    pub fn current_user() -> String {
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string());

        format!("{}@{}", username, hostname)
    }
}

/// Model configuration parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Embedding dimension
    pub dimension: usize,

    /// Number of training epochs
    pub epochs: usize,

    /// Learning rate
    pub learning_rate: f64,

    /// Other hyperparameters (key-value pairs)
    #[serde(default)]
    pub hyperparameters: std::collections::HashMap<String, serde_json::Value>,
}

/// Training data summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingDataInfo {
    /// Number of nodes in training graph
    pub num_nodes: usize,

    /// Number of edges in training graph
    pub num_edges: usize,

    /// Tables included in training
    pub tables: Vec<String>,

    /// Total row count used
    pub row_count: usize,

    /// Data split used (train/val/test)
    #[serde(default)]
    pub split_ratios: Option<[f32; 3]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_format() {
        let id = ModelArtifact::generate_id();
        assert!(id.starts_with("gnn_"));

        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "gnn");

        // Timestamp should be numeric
        assert!(parts[1].parse::<i64>().is_ok());

        // Random part should be hex
        assert_eq!(parts[2].len(), 8);
        assert!(u32::from_str_radix(parts[2], 16).is_ok());
    }

    #[test]
    fn test_generate_id_unique() {
        let id1 = ModelArtifact::generate_id();
        let id2 = ModelArtifact::generate_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_current_user_format() {
        let user = ModelArtifact::current_user();
        assert!(user.contains('@'));

        let parts: Vec<&str> = user.split('@').collect();
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
    }

    #[test]
    fn test_model_artifact_serialization() {
        let artifact = ModelArtifact {
            id: "gnn_123_abc".to_string(),
            version: "1.0.0".to_string(),
            name: Some("test_model".to_string()),
            description: Some("A test model".to_string()),
            trained_at: Utc::now(),
            trained_by: "user@host".to_string(),
            config: ModelConfig {
                dimension: 128,
                epochs: 10,
                learning_rate: 0.025,
                hyperparameters: std::collections::HashMap::new(),
            },
            training_data: TrainingDataInfo {
                num_nodes: 1000,
                num_edges: 5000,
                tables: vec!["products".to_string(), "categories".to_string()],
                row_count: 1000,
                split_ratios: Some([0.8, 0.1, 0.1]),
            },
            metrics: std::collections::HashMap::new(),
            model_path: PathBuf::from("models/gnn_123_abc"),
            commit_hash: Some("abc123".to_string()),
            tags: vec!["production".to_string()],
        };

        let json = serde_json::to_string(&artifact).unwrap();
        let deserialized: ModelArtifact = serde_json::from_str(&json).unwrap();

        assert_eq!(artifact.id, deserialized.id);
        assert_eq!(artifact.version, deserialized.version);
    }
}
