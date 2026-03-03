//! `gfs search` — semantic search using trained GNN models.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use gfs_ai_models::graph::{GraphEngineConfig, GraphSemanticEngine};

#[derive(Args)]
pub struct SearchArgs {
    /// Path to the trained model (without extension)
    #[arg(long, short = 'm')]
    pub model: PathBuf,

    /// Search query
    #[arg(long, short = 'q')]
    pub query: String,

    /// Number of results to return
    #[arg(long, short = 'k', default_value = "10")]
    pub top_k: usize,

    /// Show full text content (not just preview)
    #[arg(long)]
    pub full_text: bool,
}

/// Execute semantic search using a trained GNN model.
pub async fn run(args: SearchArgs) -> Result<()> {
    println!("Loading model from {:?}...", args.model);

    // Initialize engine
    let config = GraphEngineConfig::default();
    let mut engine = GraphSemanticEngine::with_config(config);

    // Load the trained model
    let model_path = args.model.to_str().context("invalid model path")?;
    let model_info = engine
        .load(model_path)
        .context("failed to load model")?;

    println!("Model loaded: {} dims, {} vocab, {} nodes\n",
             model_info.dimension, model_info.vocab_size, model_info.corpus_size);

    // Check for out-of-vocabulary words
    if engine.has_oov(&args.query) {
        println!("⚠️  Warning: Query contains out-of-vocabulary words");
        println!("    Results may be less accurate\n");
    }

    // Perform search
    println!("Searching for: \"{}\"\n", args.query);
    let results = engine
        .search(&args.query, args.top_k)
        .context("search failed")?;

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    // Display results
    println!("Found {} results:\n", results.len());
    println!("{}", "=".repeat(80));

    for (i, result) in results.iter().enumerate() {
        let table = result.source_table.as_ref().map(|s| s.as_str()).unwrap_or("unknown");
        println!("\n{}. {} (distance: {:.4})",
                 i + 1, table, result.distance);

        if let Some(row_id) = &result.source_row_id {
            println!("   Row ID: {}", row_id);
        }

        if args.full_text {
            println!("   Text: {}", result.text);
        } else {
            let preview_len = 150;
            let preview: String = result.text.chars().take(preview_len).collect();
            let suffix = if result.text.len() > preview_len { "..." } else { "" };
            println!("   Preview: {}{}", preview, suffix);
        }
    }

    println!("\n{}", "=".repeat(80));

    Ok(())
}
