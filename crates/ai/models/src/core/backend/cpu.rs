//===----------------------------------------------------------------------===//
//                         GFS
//
// core/backend/cpu.rs - CPU Compute Backend
//
// CPU-only implementation of compute operations. This is the fallback
// backend when GPU acceleration is not available or disabled.
//===----------------------------------------------------------------------===//

use super::ComputeBackend;
use anyhow::Result;

/// CPU-only compute backend
///
/// This backend performs all operations on the CPU using standard Rust code.
/// It's used as a fallback when GPU acceleration is unavailable or disabled.
pub struct CpuBackend;

impl CpuBackend {
    /// Create a new CPU backend
    pub fn new() -> Self {
        Self
    }
}

impl Default for CpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputeBackend for CpuBackend {
    fn initialize(&mut self) -> Result<()> {
        // No initialization needed for CPU backend
        Ok(())
    }

    fn dot_product(&self, a: &[f32], b: &[f32]) -> f32 {
        // Simple dot product implementation
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    fn dot_and_update(
        &self,
        input: &[f32],
        output: &mut [f32],
        target: f32,
        learning_rate: f32,
    ) -> Result<()> {
        // Compute dot product
        let dot = self.dot_product(input, output);

        // Compute error: target - sigmoid(dot)
        let error = target - self.sigmoid(dot);
        let gradient = learning_rate * error;

        // Update output vector: output += gradient * input
        for (out, inp) in output.iter_mut().zip(input.iter()) {
            *out += gradient * inp;
        }

        Ok(())
    }

    fn batch_embedding_lookup(
        &self,
        embeddings: &[f32],
        indices: &[usize],
        dimension: usize,
    ) -> Result<Vec<f32>> {
        let mut result = Vec::with_capacity(indices.len() * dimension);
        for &idx in indices {
            let start = idx * dimension;
            let end = start + dimension;
            result.extend_from_slice(&embeddings[start..end]);
        }
        Ok(result)
    }

    fn batch_embedding_update(
        &self,
        embeddings: &mut [f32],
        indices: &[usize],
        gradients: &[f32],
        learning_rate: f32,
        dimension: usize,
    ) -> Result<()> {
        for (i, &idx) in indices.iter().enumerate() {
            let start = idx * dimension;
            let grad_start = i * dimension;
            for j in 0..dimension {
                embeddings[start + j] += learning_rate * gradients[grad_start + j];
            }
        }
        Ok(())
    }

    fn sigmoid(&self, x: f32) -> f32 {
        // Fast sigmoid approximation with clamping
        // Avoids expensive exp() for large |x|
        if x > 6.0 {
            1.0
        } else if x < -6.0 {
            0.0
        } else {
            1.0 / (1.0 + (-x).exp())
        }
    }

    fn name(&self) -> &'static str {
        "CPU"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dot_product() {
        let backend = CpuBackend::new();
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        let result = backend.dot_product(&a, &b);
        assert_eq!(result, 32.0); // 1*4 + 2*5 + 3*6 = 32
    }

    #[test]
    fn test_sigmoid() {
        let backend = CpuBackend::new();

        // Test clamping
        assert_eq!(backend.sigmoid(10.0), 1.0);
        assert_eq!(backend.sigmoid(-10.0), 0.0);

        // Test middle range
        let mid = backend.sigmoid(0.0);
        assert!((mid - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_batch_embedding_lookup() {
        let backend = CpuBackend::new();

        // 3 embeddings of dimension 2
        let embeddings = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let indices = vec![0, 2];
        let dimension = 2;

        let result = backend.batch_embedding_lookup(&embeddings, &indices, dimension).unwrap();
        assert_eq!(result, vec![1.0, 2.0, 5.0, 6.0]);
    }

    #[test]
    fn test_batch_embedding_update() {
        let backend = CpuBackend::new();

        let mut embeddings = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let indices = vec![0, 2];
        let gradients = vec![0.1, 0.2, 0.3, 0.4];
        let learning_rate = 1.0;
        let dimension = 2;

        backend.batch_embedding_update(&mut embeddings, &indices, &gradients, learning_rate, dimension).unwrap();

        // Check updates
        assert!((embeddings[0] - 1.1).abs() < 0.001); // 1.0 + 1.0*0.1
        assert!((embeddings[1] - 2.2).abs() < 0.001); // 2.0 + 1.0*0.2
        assert_eq!(embeddings[2], 3.0); // Unchanged
        assert_eq!(embeddings[3], 4.0); // Unchanged
        assert!((embeddings[4] - 5.3).abs() < 0.001); // 5.0 + 1.0*0.3
        assert!((embeddings[5] - 6.4).abs() < 0.001); // 6.0 + 1.0*0.4
    }

    #[test]
    fn test_dot_and_update() {
        let backend = CpuBackend::new();

        let input = vec![1.0, 0.5];
        let mut output = vec![0.5, 1.0];
        let target = 1.0;
        let learning_rate = 0.1;

        backend.dot_and_update(&input, &mut output, target, learning_rate).unwrap();

        // Verify output was updated (exact values depend on sigmoid)
        assert_ne!(output[0], 0.5);
        assert_ne!(output[1], 1.0);
    }
}
