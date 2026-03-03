//! Metrics collection and evaluation for training.
//!
//! Provides utilities for tracking training progress and computing evaluation
//! metrics like precision@K, recall@K, MRR, and NDCG.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::graph::{DataGraph, GraphSemanticEngine};
use super::config::EvaluationConfig;

/// Metrics collector for tracking training progress.
///
/// Collects metrics during training including loss, evaluation metrics,
/// and timing information. Supports exporting metrics to CSV for analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsCollector {
    /// Training start time
    #[serde(skip)]
    start_time: Option<Instant>,

    /// Current epoch
    pub current_epoch: usize,

    /// Total epochs
    pub total_epochs: usize,

    /// Metric history: epoch → metric_name → value
    pub history: HashMap<usize, HashMap<String, f32>>,
}

impl MetricsCollector {
    /// Create a new metrics collector.
    ///
    /// # Arguments
    ///
    /// * `total_epochs` - Total number of training epochs
    pub fn new(total_epochs: usize) -> Self {
        Self {
            start_time: Some(Instant::now()),
            current_epoch: 0,
            total_epochs,
            history: HashMap::new(),
        }
    }

    /// Record a metric value for the current epoch.
    ///
    /// # Arguments
    ///
    /// * `name` - Metric name (e.g., "loss", "precision@5")
    /// * `value` - Metric value
    pub fn record(&mut self, name: &str, value: f32) {
        self.history
            .entry(self.current_epoch)
            .or_insert_with(HashMap::new)
            .insert(name.to_string(), value);
    }

    /// Get metric history for a specific metric.
    ///
    /// Returns a vector of (epoch, value) pairs sorted by epoch.
    pub fn get_history(&self, name: &str) -> Vec<(usize, f32)> {
        let mut results: Vec<_> = self
            .history
            .iter()
            .filter_map(|(epoch, metrics)| {
                metrics.get(name).map(|&value| (*epoch, value))
            })
            .collect();
        results.sort_by_key(|(epoch, _)| *epoch);
        results
    }

    /// Get the elapsed time since training started.
    pub fn elapsed(&self) -> Duration {
        self.start_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::from_secs(0))
    }

    /// Print a simple progress indicator.
    pub fn print_progress(&self) {
        let elapsed = self.elapsed().as_secs_f32();
        let epoch_metrics = self.history.get(&self.current_epoch);

        print!(
            "\rEpoch {}/{}: {:.1}s",
            self.current_epoch + 1,
            self.total_epochs,
            elapsed
        );

        if let Some(metrics) = epoch_metrics {
            for (name, value) in metrics.iter() {
                print!(" | {}: {:.4}", name, value);
            }
        }

        io::stdout().flush().ok();
    }

    /// Export metrics to CSV file.
    ///
    /// Creates a CSV with columns: epoch, metric1, metric2, ...
    ///
    /// # Errors
    ///
    /// Returns IO error if file cannot be written.
    pub fn export_csv(&self, path: &Path) -> io::Result<()> {
        let mut file = std::fs::File::create(path)?;

        // Collect all unique metric names
        let mut metric_names: Vec<String> = self
            .history
            .values()
            .flat_map(|m| m.keys().cloned())
            .collect();
        metric_names.sort();
        metric_names.dedup();

        // Write header
        write!(file, "epoch")?;
        for name in &metric_names {
            write!(file, ",{}", name)?;
        }
        writeln!(file)?;

        // Write data rows
        let mut epochs: Vec<_> = self.history.keys().collect();
        epochs.sort();

        for &epoch in epochs {
            write!(file, "{}", epoch)?;
            let metrics = &self.history[&epoch];
            for name in &metric_names {
                if let Some(&value) = metrics.get(name) {
                    write!(file, ",{}", value)?;
                } else {
                    write!(file, ",")?;
                }
            }
            writeln!(file)?;
        }

        Ok(())
    }

    /// Compute evaluation metrics for a test graph.
    ///
    /// This is a placeholder for full evaluation implementation.
    /// In practice, you'd need test queries with ground truth labels.
    pub fn compute_evaluation_metrics(
        &self,
        _engine: &GraphSemanticEngine,
        _test_graph: &DataGraph,
        _config: &EvaluationConfig,
    ) -> HashMap<String, f32> {
        // Placeholder implementation
        // In a full implementation, you would:
        // 1. Generate test queries from test_graph
        // 2. Execute queries using engine.search()
        // 3. Compare results to ground truth
        // 4. Compute precision, recall, MRR, NDCG

        HashMap::new()
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Compute precision@K for search results.
///
/// Precision@K measures the fraction of retrieved items that are relevant.
///
/// # Arguments
///
/// * `retrieved` - List of retrieved item IDs (top K results)
/// * `relevant` - Set of relevant item IDs (ground truth)
///
/// # Returns
///
/// Precision value in range [0, 1]
pub fn precision_at_k(retrieved: &[usize], relevant: &[usize]) -> f32 {
    if retrieved.is_empty() {
        return 0.0;
    }

    let relevant_set: std::collections::HashSet<_> = relevant.iter().collect();
    let relevant_count = retrieved
        .iter()
        .filter(|id| relevant_set.contains(id))
        .count();

    relevant_count as f32 / retrieved.len() as f32
}

/// Compute recall@K for search results.
///
/// Recall@K measures the fraction of relevant items that were retrieved.
///
/// # Arguments
///
/// * `retrieved` - List of retrieved item IDs (top K results)
/// * `relevant` - Set of relevant item IDs (ground truth)
///
/// # Returns
///
/// Recall value in range [0, 1]
pub fn recall_at_k(retrieved: &[usize], relevant: &[usize]) -> f32 {
    if relevant.is_empty() {
        return 0.0;
    }

    let relevant_set: std::collections::HashSet<_> = relevant.iter().collect();
    let relevant_count = retrieved
        .iter()
        .filter(|id| relevant_set.contains(id))
        .count();

    relevant_count as f32 / relevant.len() as f32
}

/// Compute Mean Reciprocal Rank (MRR).
///
/// MRR measures the average of the reciprocal ranks of the first relevant item.
///
/// # Arguments
///
/// * `queries` - List of (retrieved, relevant) pairs for each query
///
/// # Returns
///
/// MRR value in range [0, 1]
pub fn mean_reciprocal_rank(queries: &[(Vec<usize>, Vec<usize>)]) -> f32 {
    if queries.is_empty() {
        return 0.0;
    }

    let sum: f32 = queries
        .iter()
        .map(|(retrieved, relevant)| {
            let relevant_set: std::collections::HashSet<_> = relevant.iter().collect();

            // Find rank of first relevant item (1-indexed)
            for (rank, id) in retrieved.iter().enumerate() {
                if relevant_set.contains(id) {
                    return 1.0 / (rank + 1) as f32;
                }
            }
            0.0 // No relevant items found
        })
        .sum();

    sum / queries.len() as f32
}

/// Compute Normalized Discounted Cumulative Gain (NDCG@K).
///
/// NDCG@K measures the quality of ranking with position-based discounting.
///
/// # Arguments
///
/// * `retrieved` - List of retrieved item IDs (top K results)
/// * `relevant` - Set of relevant item IDs (ground truth, binary relevance)
///
/// # Returns
///
/// NDCG value in range [0, 1]
pub fn ndcg_at_k(retrieved: &[usize], relevant: &[usize]) -> f32 {
    if retrieved.is_empty() || relevant.is_empty() {
        return 0.0;
    }

    let relevant_set: std::collections::HashSet<_> = relevant.iter().collect();

    // DCG: sum of (relevance / log2(rank + 1))
    let dcg: f32 = retrieved
        .iter()
        .enumerate()
        .map(|(rank, id)| {
            let relevance = if relevant_set.contains(id) { 1.0 } else { 0.0 };
            relevance / ((rank + 2) as f32).log2() // rank+2 because rank is 0-indexed
        })
        .sum();

    // IDCG: ideal DCG (all relevant items at top)
    let k = retrieved.len().min(relevant.len());
    let idcg: f32 = (0..k)
        .map(|rank| 1.0 / ((rank + 2) as f32).log2())
        .sum();

    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_collector_record() {
        let mut collector = MetricsCollector::new(10);
        collector.current_epoch = 0;
        collector.record("loss", 0.5);
        collector.record("accuracy", 0.8);

        assert_eq!(collector.history[&0]["loss"], 0.5);
        assert_eq!(collector.history[&0]["accuracy"], 0.8);
    }

    #[test]
    fn test_metrics_collector_history() {
        let mut collector = MetricsCollector::new(10);

        collector.current_epoch = 0;
        collector.record("loss", 0.5);

        collector.current_epoch = 1;
        collector.record("loss", 0.3);

        collector.current_epoch = 2;
        collector.record("loss", 0.2);

        let history = collector.get_history("loss");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], (0, 0.5));
        assert_eq!(history[1], (1, 0.3));
        assert_eq!(history[2], (2, 0.2));
    }

    #[test]
    fn test_precision_at_k_perfect() {
        let retrieved = vec![1, 2, 3];
        let relevant = vec![1, 2, 3];
        assert_eq!(precision_at_k(&retrieved, &relevant), 1.0);
    }

    #[test]
    fn test_precision_at_k_half() {
        let retrieved = vec![1, 2, 3, 4];
        let relevant = vec![1, 3];
        assert_eq!(precision_at_k(&retrieved, &relevant), 0.5);
    }

    #[test]
    fn test_precision_at_k_zero() {
        let retrieved = vec![1, 2, 3];
        let relevant = vec![4, 5, 6];
        assert_eq!(precision_at_k(&retrieved, &relevant), 0.0);
    }

    #[test]
    fn test_recall_at_k_perfect() {
        let retrieved = vec![1, 2, 3];
        let relevant = vec![1, 2, 3];
        assert_eq!(recall_at_k(&retrieved, &relevant), 1.0);
    }

    #[test]
    fn test_recall_at_k_partial() {
        let retrieved = vec![1, 2];
        let relevant = vec![1, 2, 3, 4];
        assert_eq!(recall_at_k(&retrieved, &relevant), 0.5);
    }

    #[test]
    fn test_mrr_perfect() {
        let queries = vec![
            (vec![1, 2, 3], vec![1]),
            (vec![1, 2, 3], vec![1]),
        ];
        assert_eq!(mean_reciprocal_rank(&queries), 1.0);
    }

    #[test]
    fn test_mrr_mixed() {
        let queries = vec![
            (vec![1, 2, 3], vec![1]), // rank 1 -> 1.0
            (vec![1, 2, 3], vec![2]), // rank 2 -> 0.5
            (vec![1, 2, 3], vec![3]), // rank 3 -> 0.333
        ];
        let mrr = mean_reciprocal_rank(&queries);
        assert!((mrr - 0.611).abs() < 0.01);
    }

    #[test]
    fn test_ndcg_at_k_perfect() {
        let retrieved = vec![1, 2, 3];
        let relevant = vec![1, 2, 3];
        assert_eq!(ndcg_at_k(&retrieved, &relevant), 1.0);
    }

    #[test]
    fn test_ndcg_at_k_partial() {
        let retrieved = vec![1, 4, 2]; // relevant: 1, 2
        let relevant = vec![1, 2];
        let ndcg = ndcg_at_k(&retrieved, &relevant);
        assert!(ndcg > 0.0 && ndcg < 1.0);
    }
}
