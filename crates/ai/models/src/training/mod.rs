//! Training orchestration for graph neural networks.
//!
//! This module provides infrastructure for training GraphSemanticEngine models
//! with progress tracking, metrics collection, checkpointing, and evaluation.

pub mod config;
pub mod metrics;
pub mod checkpoint;
pub mod trainer;
pub mod data_split;

pub use config::{
    TrainingConfig, DataConfig, TrainingParams, CheckpointConfig, EvaluationConfig, MetricType,
};
pub use metrics::MetricsCollector;
pub use checkpoint::{CheckpointManager, CheckpointInfo};
pub use trainer::{Trainer, TrainingReport};
pub use data_split::{split_graph, SplitError};
