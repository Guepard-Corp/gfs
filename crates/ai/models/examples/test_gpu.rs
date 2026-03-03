//! Test GPU acceleration for Word2Vec training
//!
//! Run with: cargo run --example test_gpu --features metal --release

use gfs_ai_models::core::{Word2Vec, Word2VecConfig};

fn main() {
    println!("=== GPU Acceleration Test ===\n");

    // Create test corpus
    let corpus = vec![
        "apple fruit red sweet".to_string(),
        "banana fruit yellow tropical".to_string(),
        "carrot vegetable orange crunchy".to_string(),
        "database table row column".to_string(),
        "query search find data".to_string(),
        "apple banana fruit basket".to_string(),
        "table query database schema".to_string(),
        "orange carrot vegetable garden".to_string(),
        "apple red fruit tree".to_string(),
        "banana yellow fruit tropical island".to_string(),
    ];

    println!("Test corpus: {} documents", corpus.len());
    println!("Training Word2Vec model...\n");

    // Configure Word2Vec with GPU enabled
    let mut config = Word2VecConfig::default();
    config.dimension = 128;
    config.epochs = 5;
    config.window = 5;
    config.negative_samples = 10;
    config.min_count = 1;
    config.num_threads = 4;
    config.use_gpu = true; // Enable GPU
    config.force_cpu = false;

    println!("Configuration:");
    println!("  - Dimension: {}", config.dimension);
    println!("  - Epochs: {}", config.epochs);
    println!("  - Window: {}", config.window);
    println!("  - Negative samples: {}", config.negative_samples);
    println!("  - GPU enabled: {}", config.use_gpu);
    println!("  - Force CPU: {}\n", config.force_cpu);

    // Train model with GPU
    println!("Training with GPU...");
    let mut w2v_gpu = Word2Vec::with_config(config.clone());
    let start = std::time::Instant::now();
    w2v_gpu.train(&corpus);
    let gpu_time = start.elapsed();

    println!("✓ Training with GPU completed in {:?}", gpu_time);
    println!("  Vocabulary size: {}", w2v_gpu.vocab().size());

    // Train model with CPU for comparison
    println!("\nTraining with CPU for comparison...");
    config.force_cpu = true;
    let mut w2v_cpu = Word2Vec::with_config(config.clone());
    let start = std::time::Instant::now();
    w2v_cpu.train(&corpus);
    let cpu_time = start.elapsed();

    println!("✓ Training with CPU completed in {:?}", cpu_time);
    println!("  Vocabulary size: {}", w2v_cpu.vocab().size());

    // Compare performance
    println!("\n=== Performance Comparison ===");
    println!("GPU time: {:?}", gpu_time);
    println!("CPU time: {:?}", cpu_time);

    if gpu_time < cpu_time {
        let speedup = cpu_time.as_secs_f64() / gpu_time.as_secs_f64();
        println!("\n🚀 GPU is {:.2}x faster than CPU!", speedup);
    } else {
        let slowdown = gpu_time.as_secs_f64() / cpu_time.as_secs_f64();
        println!("\n⚠️  CPU was {:.2}x faster (dataset too small for GPU benefit)", slowdown);
        println!("Note: GPU acceleration benefits increase with larger datasets (>5,000 nodes)");
    }

    // Test embeddings
    println!("\n=== Testing Embeddings ===");
    let test_words = vec!["fruit", "database", "apple"];
    for word in test_words {
        if let Some(word_idx) = w2v_gpu.vocab().lookup(word) {
            if let Some(embedding) = w2v_gpu.embedding(word_idx) {
                println!("Embedding for '{}': [{:.4}, {:.4}, {:.4}, ...]",
                         word, embedding[0], embedding[1], embedding[2]);
            }
        } else {
            println!("Word '{}' not found in vocabulary", word);
        }
    }

    println!("\n=== Test Complete ===");
}
