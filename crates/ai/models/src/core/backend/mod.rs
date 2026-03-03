//===----------------------------------------------------------------------===//
//                         GFS
//
// core/backend/mod.rs - Compute Backend Abstraction
//
// Provides abstraction for compute backends (CPU, Metal GPU) to enable
// GPU acceleration for Word2Vec training while maintaining compatibility.
//===----------------------------------------------------------------------===//

pub mod cpu;
#[cfg(feature = "metal")]
pub mod metal;

use anyhow::Result;

/// Compute backend for Word2Vec operations.
///
/// This trait abstracts compute operations (dot products, vector updates, etc.)
/// to allow both CPU and GPU implementations.
pub trait ComputeBackend: Send + Sync {
    /// Initialize the backend (warm up GPU, allocate buffers, etc.)
    fn initialize(&mut self) -> Result<()>;

    /// Compute dot product of two vectors
    ///
    /// # Arguments
    /// * `a` - First vector
    /// * `b` - Second vector
    ///
    /// # Returns
    /// Dot product a · b
    fn dot_product(&self, a: &[f32], b: &[f32]) -> f32;

    /// Compute dot product and update output vector (for training)
    ///
    /// This performs: output += learning_rate * (target - sigmoid(dot(input, output))) * input
    ///
    /// # Arguments
    /// * `input` - Input embedding vector
    /// * `output` - Output embedding vector (modified in place)
    /// * `target` - Target value (0 or 1 for negative/positive samples)
    /// * `learning_rate` - Learning rate for gradient update
    fn dot_and_update(
        &self,
        input: &[f32],
        output: &mut [f32],
        target: f32,
        learning_rate: f32,
    ) -> Result<()>;

    /// Batch embedding lookup (get multiple embeddings at once)
    ///
    /// # Arguments
    /// * `embeddings` - Flattened embedding matrix (vocab_size × dimension)
    /// * `indices` - Indices of embeddings to retrieve
    /// * `dimension` - Embedding dimension
    ///
    /// # Returns
    /// Flattened vector containing requested embeddings
    fn batch_embedding_lookup(
        &self,
        embeddings: &[f32],
        indices: &[usize],
        dimension: usize,
    ) -> Result<Vec<f32>>;

    /// Batch embedding update (update multiple embeddings)
    ///
    /// # Arguments
    /// * `embeddings` - Flattened embedding matrix (modified in place)
    /// * `indices` - Indices of embeddings to update
    /// * `gradients` - Gradient values for each embedding
    /// * `learning_rate` - Learning rate
    /// * `dimension` - Embedding dimension
    fn batch_embedding_update(
        &self,
        embeddings: &mut [f32],
        indices: &[usize],
        gradients: &[f32],
        learning_rate: f32,
        dimension: usize,
    ) -> Result<()>;

    /// Compute sigmoid efficiently
    ///
    /// # Arguments
    /// * `x` - Input value
    ///
    /// # Returns
    /// sigmoid(x) = 1 / (1 + exp(-x))
    fn sigmoid(&self, x: f32) -> f32;

    /// Backend name (for logging)
    fn name(&self) -> &'static str;
}

/// Backend configuration
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Prefer GPU if available
    pub prefer_gpu: bool,
    /// Force CPU even if GPU available
    pub force_cpu: bool,
    /// Batch size for GPU operations
    pub batch_size: usize,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            prefer_gpu: true, // Default to GPU if available
            force_cpu: false,
            batch_size: 1024, // Good balance for Metal
        }
    }
}

/// Create appropriate backend based on configuration and availability
///
/// # Arguments
/// * `config` - Backend configuration
///
/// # Returns
/// Box containing the selected compute backend
pub fn create_backend(config: &BackendConfig) -> Result<Box<dyn ComputeBackend>> {
    if config.force_cpu {
        return Ok(Box::new(cpu::CpuBackend::new()));
    }

    #[cfg(feature = "metal")]
    {
        if config.prefer_gpu {
            match metal::MetalBackend::new(config.batch_size) {
                Ok(backend) => {
                    println!("✓ Using Metal GPU backend for training");
                    return Ok(Box::new(backend));
                }
                Err(e) => {
                    eprintln!("⚠ Failed to initialize Metal backend: {}", e);
                    eprintln!("⚠ Falling back to CPU backend");
                }
            }
        }
    }

    println!("✓ Using CPU backend for training");
    Ok(Box::new(cpu::CpuBackend::new()))
}
