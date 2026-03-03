//! Tests for compute backend implementations.

use gfs_ai_models::core::backend::{cpu::CpuBackend, ComputeBackend};

#[test]
fn test_cpu_backend_dot_product() {
    let backend = CpuBackend::new();

    // Test simple dot product
    let a = vec![1.0, 2.0, 3.0];
    let b = vec![4.0, 5.0, 6.0];
    let result = backend.dot_product(&a, &b);

    assert_eq!(result, 32.0); // 1*4 + 2*5 + 3*6 = 32
}

#[test]
fn test_cpu_backend_sigmoid() {
    let backend = CpuBackend::new();

    // Test sigmoid clamping
    assert_eq!(backend.sigmoid(10.0), 1.0);
    assert_eq!(backend.sigmoid(-10.0), 0.0);

    // Test sigmoid at zero
    let mid = backend.sigmoid(0.0);
    assert!((mid - 0.5).abs() < 0.001);

    // Test sigmoid at 1
    let sig1 = backend.sigmoid(1.0);
    assert!(sig1 > 0.5 && sig1 < 1.0);

    // Test sigmoid at -1
    let sig_neg1 = backend.sigmoid(-1.0);
    assert!(sig_neg1 > 0.0 && sig_neg1 < 0.5);
}

#[test]
fn test_cpu_backend_batch_embedding_lookup() {
    let backend = CpuBackend::new();

    // Create 4 embeddings of dimension 3
    let embeddings = vec![
        1.0, 2.0, 3.0,    // embedding 0
        4.0, 5.0, 6.0,    // embedding 1
        7.0, 8.0, 9.0,    // embedding 2
        10.0, 11.0, 12.0, // embedding 3
    ];
    let indices = vec![0, 2, 3];
    let dimension = 3;

    let result = backend.batch_embedding_lookup(&embeddings, &indices, dimension).unwrap();

    assert_eq!(result.len(), 9); // 3 embeddings × 3 dimensions
    assert_eq!(&result[0..3], &[1.0, 2.0, 3.0]);
    assert_eq!(&result[3..6], &[7.0, 8.0, 9.0]);
    assert_eq!(&result[6..9], &[10.0, 11.0, 12.0]);
}

#[test]
fn test_cpu_backend_batch_embedding_update() {
    let backend = CpuBackend::new();

    let mut embeddings = vec![
        1.0, 2.0,  // embedding 0
        3.0, 4.0,  // embedding 1
        5.0, 6.0,  // embedding 2
    ];
    let indices = vec![0, 2];
    let gradients = vec![
        0.1, 0.2,  // gradient for embedding 0
        0.3, 0.4,  // gradient for embedding 2
    ];
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
fn test_cpu_backend_dot_and_update() {
    let backend = CpuBackend::new();

    let input = vec![1.0, 0.5, 0.3];
    let mut output = vec![0.5, 1.0, 0.2];
    let target = 1.0; // Positive sample
    let learning_rate = 0.1;

    let original_output = output.clone();

    backend.dot_and_update(&input, &mut output, target, learning_rate).unwrap();

    // Verify output was updated (exact values depend on sigmoid)
    // At least one value should change
    let changed = (output[0] - original_output[0]).abs() > 1e-6
        || (output[1] - original_output[1]).abs() > 1e-6
        || (output[2] - original_output[2]).abs() > 1e-6;

    assert!(changed, "At least one output value should have changed");
}

#[cfg(feature = "metal")]
#[test]
fn test_metal_backend_creation() {
    use gfs_ai_models::core::backend::metal::MetalBackend;

    let result = MetalBackend::new(1024);

    // This test will pass if Metal is available, skip otherwise
    match result {
        Ok(backend) => {
            assert_eq!(backend.name(), "Metal");
        }
        Err(e) => {
            println!("Metal backend not available: {}", e);
            // Skip test if Metal not available
        }
    }
}

#[cfg(feature = "metal")]
#[test]
fn test_metal_backend_dot_product() {
    use gfs_ai_models::core::backend::metal::MetalBackend;

    let backend = match MetalBackend::new(1024) {
        Ok(b) => b,
        Err(_) => {
            println!("Skipping test: Metal not available");
            return;
        }
    };

    let a = vec![1.0, 2.0, 3.0];
    let b = vec![4.0, 5.0, 6.0];
    let result = backend.dot_product(&a, &b);

    // Allow for floating point precision
    assert!((result - 32.0).abs() < 1e-4);
}

#[cfg(feature = "metal")]
#[test]
fn test_metal_backend_batch_embedding_lookup() {
    use gfs_ai_models::core::backend::metal::MetalBackend;

    let backend = match MetalBackend::new(1024) {
        Ok(b) => b,
        Err(_) => {
            println!("Skipping test: Metal not available");
            return;
        }
    };

    // 3 embeddings of dimension 2
    let embeddings = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    let indices = vec![0, 2];
    let dimension = 2;

    let result = backend.batch_embedding_lookup(&embeddings, &indices, dimension).unwrap();

    assert_eq!(result.len(), 4);
    assert!((result[0] - 1.0).abs() < 1e-4);
    assert!((result[1] - 2.0).abs() < 1e-4);
    assert!((result[2] - 5.0).abs() < 1e-4);
    assert!((result[3] - 6.0).abs() < 1e-4);
}

#[cfg(feature = "metal")]
#[test]
fn test_metal_vs_cpu_consistency() {
    use gfs_ai_models::core::backend::metal::MetalBackend;

    let metal = match MetalBackend::new(1024) {
        Ok(b) => b,
        Err(_) => {
            println!("Skipping test: Metal not available");
            return;
        }
    };
    let cpu = CpuBackend::new();

    // Test with various vector sizes
    for size in [16, 64, 128, 256] {
        let a = vec![0.5; size];
        let b = vec![0.3; size];

        let metal_result = metal.dot_product(&a, &b);
        let cpu_result = cpu.dot_product(&a, &b);

        // Should be very close (within floating point precision)
        let diff = (metal_result - cpu_result).abs();
        assert!(diff < 1e-3, "Difference {} too large for size {}", diff, size);
    }
}

#[cfg(feature = "metal")]
#[test]
fn test_metal_batch_operations_large() {
    use gfs_ai_models::core::backend::metal::MetalBackend;

    let backend = match MetalBackend::new(1024) {
        Ok(b) => b,
        Err(_) => {
            println!("Skipping test: Metal not available");
            return;
        }
    };

    // Large batch to test GPU performance
    let vocab_size = 1000;
    let dimension = 128;
    let embeddings = vec![0.5; vocab_size * dimension];
    let indices: Vec<usize> = (0..100).collect();

    let result = backend.batch_embedding_lookup(&embeddings, &indices, dimension).unwrap();

    assert_eq!(result.len(), 100 * dimension);
    // All values should be 0.5
    for val in result.iter() {
        assert!((val - 0.5).abs() < 1e-4);
    }
}

#[test]
fn test_backend_selection() {
    use gfs_ai_models::core::backend::{BackendConfig, create_backend};

    // Test CPU backend selection
    let cpu_config = BackendConfig {
        prefer_gpu: false,
        force_cpu: true,
        batch_size: 1024,
    };

    let backend = create_backend(&cpu_config).unwrap();
    assert_eq!(backend.name(), "CPU");

    // Test GPU backend selection (will fall back to CPU if Metal not available)
    let gpu_config = BackendConfig {
        prefer_gpu: true,
        force_cpu: false,
        batch_size: 1024,
    };

    let backend = create_backend(&gpu_config).unwrap();
    // Will be either "Metal" or "CPU" depending on availability
    assert!(backend.name() == "Metal" || backend.name() == "CPU");
}
