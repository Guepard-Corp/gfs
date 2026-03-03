//! Training orchestration for GNN models.
//!
//! Provides the main Trainer struct that coordinates model training,
//! metrics collection, checkpointing, and evaluation.

use std::time::Duration;

use crate::graph::{DataGraph, GraphSemanticEngine};
use crate::semantic_search_engine::ModelInfo;
use super::config::TrainingConfig;
use super::metrics::MetricsCollector;
use super::checkpoint::{CheckpointManager, CheckpointError};

/// High-level trainer for GNN models.
///
/// Orchestrates the complete training process including:
/// - Model training
/// - Progress tracking
/// - Metrics collection
/// - Checkpointing and resumption
/// - Evaluation
///
/// # Example
///
/// ```ignore
/// use gfs_ai_models::training::{Trainer, TrainingConfig};
/// use gfs_ai_models::graph::DataGraph;
///
/// let config = TrainingConfig::default();
/// let mut trainer = Trainer::new(config);
///
/// let graph = DataGraph::builder().build();
/// let report = trainer.train(&graph, None).await?;
///
/// println!("Training complete! Dimension: {}", report.model_info.dimension);
/// ```
pub struct Trainer {
    config: TrainingConfig,
    metrics: MetricsCollector,
    checkpoint_manager: CheckpointManager,
    engine: GraphSemanticEngine,
}

impl Trainer {
    /// Create a new trainer with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Training configuration
    pub fn new(config: TrainingConfig) -> Self {
        let total_epochs = config.model.w2v.epochs;
        let metrics = MetricsCollector::new(total_epochs);
        let checkpoint_manager = CheckpointManager::new(config.checkpoint.clone());
        let engine = GraphSemanticEngine::with_config(config.model.clone());

        Self {
            config,
            metrics,
            checkpoint_manager,
            engine,
        }
    }

    /// Train a GNN model on the provided graph.
    ///
    /// This method handles:
    /// - Resuming from checkpoints if available
    /// - Training the model
    /// - Computing metrics at intervals
    /// - Saving checkpoints
    /// - Progress reporting
    ///
    /// # Arguments
    ///
    /// * `graph` - Training graph
    /// * `validation_graph` - Optional validation graph for metrics
    ///
    /// # Returns
    ///
    /// Training report with model info and metrics
    ///
    /// # Errors
    ///
    /// Returns error if training fails.
    pub fn train(
        &mut self,
        graph: &DataGraph,
        validation_graph: Option<&DataGraph>,
    ) -> Result<TrainingReport, TrainerError> {
        // 1. Check for existing checkpoints and resume if found
        let start_epoch = if self.config.checkpoint.checkpoint_interval > 0 {
            match self.checkpoint_manager.load_latest_checkpoint(&mut self.engine) {
                Ok(Some((epoch, metrics))) => {
                    if self.config.training.show_progress {
                        println!("Resuming from checkpoint at epoch {}", epoch);
                    }
                    self.metrics = metrics;
                    epoch + 1
                }
                Ok(None) => 0,
                Err(e) => {
                    eprintln!("Warning: failed to load checkpoint: {}", e);
                    0
                }
            }
        } else {
            0
        };

        // 2. Train the model if starting fresh
        if start_epoch == 0 {
            if self.config.training.show_progress {
                println!("Training model on {} nodes, {} edges...",
                         graph.node_count(), graph.edge_count());
            }

            let model_info = self.engine.train(graph)
                .map_err(|e| TrainerError::TrainingError(format!("model training failed: {}", e)))?;

            if self.config.training.show_progress {
                println!("Model trained: {} dimensions, {} vocab size, {} corpus size",
                         model_info.dimension, model_info.vocab_size, model_info.corpus_size);
            }
        }

        // 3. Training loop for metrics and checkpointing
        // Note: GraphSemanticEngine.train() is not actually iterative,
        // so this is mainly for demonstration and future multi-epoch support
        let total_epochs = self.config.model.w2v.epochs;

        for epoch in start_epoch..total_epochs {
            self.metrics.current_epoch = epoch;

            // Record basic training metrics
            if self.config.training.compute_metrics
                && epoch % self.config.training.metric_interval == 0
            {
                // Record elapsed time for this epoch
                let elapsed_secs = self.metrics.elapsed().as_secs_f32();
                self.metrics.record("elapsed_time_s", elapsed_secs);

                // Record model size metrics
                if let Some(model_info) = self.engine.model_info() {
                    self.metrics.record("vocab_size", model_info.vocab_size as f32);
                    self.metrics.record("corpus_size", model_info.corpus_size as f32);
                }

                // Compute evaluation metrics on the training graph.
                // The engine's HNSW index contains only training nodes, so we evaluate
                // there: for each sampled node, search returns other training nodes,
                // and we check whether graph neighbours appear in the top-K results.
                let eval_metrics = self.metrics.compute_evaluation_metrics(
                    &self.engine,
                    graph,
                    &self.config.evaluation,
                );
                for (name, value) in eval_metrics {
                    self.metrics.record(&name, value);
                }
                let _ = validation_graph; // reserved for future inductive evaluation
            }

            // Save checkpoint if configured
            if self.config.checkpoint.checkpoint_interval > 0
                && epoch > 0
                && epoch % self.config.checkpoint.checkpoint_interval == 0
            {
                if let Err(e) = self.checkpoint_manager.save_checkpoint(
                    epoch,
                    &self.engine,
                    &self.metrics,
                ) {
                    eprintln!("Warning: failed to save checkpoint: {}", e);
                } else {
                    if self.config.training.show_progress {
                        println!("Checkpoint saved at epoch {}", epoch);
                    }

                    // Cleanup old checkpoints
                    if let Err(e) = self.checkpoint_manager.cleanup_old_checkpoints() {
                        eprintln!("Warning: failed to cleanup checkpoints: {}", e);
                    }
                }
            }

            // Print progress
            if self.config.training.show_progress {
                self.metrics.print_progress();
            }
        }

        if self.config.training.show_progress {
            println!(); // Newline after progress
        }

        // 4. Return training report
        let model_info = self.engine.model_info()
            .ok_or_else(|| TrainerError::TrainingError("model not trained".to_string()))?;

        Ok(TrainingReport {
            model_info,
            final_metrics: self.metrics.history.clone(),
            total_time: self.metrics.elapsed(),
        })
    }

    /// Get a reference to the trained engine.
    pub fn engine(&self) -> &GraphSemanticEngine {
        &self.engine
    }

    /// Get a mutable reference to the trained engine.
    pub fn engine_mut(&mut self) -> &mut GraphSemanticEngine {
        &mut self.engine
    }

    /// Get a reference to the metrics collector.
    pub fn metrics(&self) -> &MetricsCollector {
        &self.metrics
    }
}

/// Training report returned after training completes.
#[derive(Debug, Clone)]
pub struct TrainingReport {
    /// Model information (dimension, vocab size, corpus size)
    pub model_info: ModelInfo,

    /// Final metrics by epoch
    pub final_metrics: std::collections::HashMap<usize, std::collections::HashMap<String, f32>>,

    /// Total training time
    pub total_time: Duration,
}

/// Training errors.
#[derive(Debug, thiserror::Error)]
pub enum TrainerError {
    #[error("training error: {0}")]
    TrainingError(String),

    #[error("checkpoint error: {0}")]
    CheckpointError(#[from] CheckpointError),

    #[error("configuration error: {0}")]
    ConfigError(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::training::TrainingConfig;

    #[test]
    fn test_trainer_creation() {
        let config = TrainingConfig::default();
        let trainer = Trainer::new(config);
        assert_eq!(trainer.metrics.total_epochs, 5); // Default from Word2VecConfig
    }

    #[test]
    fn test_trainer_with_custom_config() {
        let mut config = TrainingConfig::default();
        config.model.w2v.epochs = 5;
        config.training.show_progress = false;

        let trainer = Trainer::new(config);
        assert_eq!(trainer.metrics.total_epochs, 5);
        assert!(!trainer.config.training.show_progress);
    }
}
