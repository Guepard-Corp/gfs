//! `gfs train` — train GNN models on database data.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;

use gfs_ai_models::features::extractors::DefaultFeatureExtractor;
use gfs_ai_models::training::{split_graph, Trainer, TrainingConfig};
use gfs_ai_postgres::{DatabaseGraphBuilder, GraphBuilderConfig, PostgresQueryExecutor};
use gfs_compute_docker::DockerCompute;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::model::datasource::DatasourceMetadata;
use gfs_domain::ports::compute::{Compute, InstanceId};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};
use gfs_domain::usecases::repository::extract_schema_usecase::ExtractSchemaUseCase;

use crate::cli_utils::get_repo_dir;

/// Training subcommands.
#[derive(Subcommand)]
pub enum TrainAction {
    /// Train a new GNN model on database data
    Start {
        /// Path to GFS repository
        #[arg(long)]
        path: Option<PathBuf>,

        /// Output model path (without extension)
        #[arg(long, short = 'o')]
        output: PathBuf,

        /// Training configuration file (TOML)
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,

        /// Database to train on (e.g., "demo", "postgres")
        #[arg(long, short = 'd')]
        database: Option<String>,

        /// Override: specific tables to include (comma-separated or multiple args)
        #[arg(long)]
        tables: Option<Vec<String>>,

        /// Override: max rows per table
        #[arg(long)]
        row_limit: Option<usize>,

        /// Skip evaluation metrics (faster training)
        #[arg(long)]
        skip_metrics: bool,
    },

    /// Resume training from checkpoint
    Resume {
        /// Path to GFS repository
        #[arg(long)]
        path: Option<PathBuf>,

        /// Checkpoint directory to resume from
        #[arg(long)]
        checkpoint: PathBuf,

        /// Output model path (without extension)
        #[arg(long, short = 'o')]
        output: PathBuf,
    },

    /// Evaluate a trained model (future feature)
    Evaluate {
        /// Path to GFS repository
        #[arg(long)]
        path: Option<PathBuf>,

        /// Model path to evaluate
        #[arg(long, short = 'm')]
        model: PathBuf,

        /// Test queries file (JSON, future feature)
        #[arg(long)]
        test_queries: Option<PathBuf>,
    },
}

/// Execute the train command.
pub async fn run(action: TrainAction) -> Result<()> {
    match action {
        TrainAction::Start {
            path,
            output,
            config,
            database,
            tables,
            row_limit,
            skip_metrics,
        } => run_start(path, output, config, database, tables, row_limit, skip_metrics).await,

        TrainAction::Resume {
            path,
            checkpoint,
            output,
        } => run_resume(path, checkpoint, output).await,

        TrainAction::Evaluate {
            path: _,
            model: _,
            test_queries: _,
        } => {
            println!("Model evaluation is not yet implemented.");
            println!("This feature will be available in a future release.");
            Ok(())
        }
    }
}

/// Start training a new model.
async fn run_start(
    path: Option<PathBuf>,
    output: PathBuf,
    config_path: Option<PathBuf>,
    database: Option<String>,
    tables: Option<Vec<String>>,
    row_limit: Option<usize>,
    skip_metrics: bool,
) -> Result<()> {
    // 1. Load GFS config
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let gfs_config = GfsConfig::load(&repo_path)
        .context("not a GFS repository (run 'gfs init' first)")?;

    let environment = gfs_config
        .environment
        .as_ref()
        .context("no database configured (run 'gfs init' with --database-provider)")?;

    let runtime = gfs_config
        .runtime
        .as_ref()
        .context("no runtime configured (run 'gfs init' with --database-provider)")?;

    // 2. Load or create training config
    let mut train_config = if let Some(cfg_path) = config_path {
        println!("Loading training configuration from {:?}...", cfg_path);
        TrainingConfig::load_from_toml(&cfg_path)
            .with_context(|| format!("failed to load config from {:?}", cfg_path))?
    } else {
        println!("Using default training configuration");
        TrainingConfig::default()
    };

    // 3. Override config with CLI args
    if let Some(table_list) = tables {
        train_config.data.table_filter = Some(table_list);
    }
    if let Some(limit) = row_limit {
        train_config.data.row_limit = Some(limit);
    }
    if skip_metrics {
        train_config.training.compute_metrics = false;
    }

    // 4. Set up infrastructure
    let compute = Arc::new(
        DockerCompute::new().context("failed to connect to Docker (is Docker running?)")?,
    );

    let registry_impl = InMemoryDatabaseProviderRegistry::new();
    gfs_compute_docker::containers::register_all(&registry_impl)
        .context("failed to register database providers")?;
    let registry_arc = Arc::new(registry_impl);
    let registry: Arc<dyn DatabaseProviderRegistry> = registry_arc.clone();

    let provider = registry
        .get(&environment.database_provider)
        .with_context(|| format!("unknown database provider: '{}'", environment.database_provider))?;

    // 5. Get connection info from running container
    let instance_id = InstanceId(runtime.container_name.clone());
    let default_port = provider.default_port();

    let conn_info = compute
        .get_connection_info(&instance_id, default_port)
        .await
        .context("failed to get connection info (is the database running? try 'gfs compute start')")?;

    // 6. Extract schema
    println!("Extracting database schema...");
    let extract_usecase: ExtractSchemaUseCase<InMemoryDatabaseProviderRegistry> = ExtractSchemaUseCase::new(
        compute.clone(),
        registry_arc.clone(),
    );

    let result = extract_usecase
        .run(&repo_path)
        .await
        .context("failed to extract database schema")?;

    let mut metadata: DatasourceMetadata = result.metadata;

    // Determine which database to use
    let target_database = if let Some(db_name) = database {
        db_name
    } else {
        // Auto-select first database with tables
        metadata
            .get_databases()
            .iter()
            .find(|db| !db.tables.is_empty())
            .map(|db| db.name.clone())
            .context("no database with tables found")?
    };

    println!("Using database: {}", target_database);

    // Filter metadata to only include the target database
    if let Some(db) = metadata.get_database(&target_database) {
        // Clone the database and attach columns to each table
        let mut filtered_db = db.clone();

        // For each table, attach its columns
        for table in &mut filtered_db.tables {
            let table_columns: Vec<_> = filtered_db.columns
                .iter()
                .filter(|col| col.schema == table.schema && col.table == table.name)
                .cloned()
                .collect();

            if !table_columns.is_empty() {
                table.columns = Some(table_columns);
            }
        }

        // Replace databases with just the target database
        metadata.databases = Some(vec![filtered_db]);
    } else {
        anyhow::bail!("database '{}' not found", target_database);
    }

    println!("Found {} tables", metadata.get_all_tables().len());

    // Update connection params to use the target database
    let mut env = conn_info.env;

    // Update or add the database name in environment variables (POSTGRES_DB is the key used by provider)
    let mut found = false;
    for (key, value) in &mut env {
        if key == "POSTGRES_DB" {
            *value = target_database.clone();
            found = true;
            break;
        }
    }
    if !found {
        env.push(("POSTGRES_DB".to_string(), target_database.clone()));
    }

    let connection_params = ConnectionParams {
        host: conn_info.host,
        port: conn_info.port,
        env,
    };

    // 7. Build graph from database
    println!("Building graph from database...");

    let query_executor = Arc::new(PostgresQueryExecutor::new(
        provider.clone(),
        compute.clone(),
    ));

    let feature_extractor = Arc::new(DefaultFeatureExtractor::new().with_separator(" "));

    let mut graph_config = GraphBuilderConfig::new()
        .with_table_filter(train_config.data.table_filter.clone().unwrap_or_default())
        .with_relationships(train_config.data.include_relationships)
        .with_feature_extractor(feature_extractor);

    if let Some(limit) = train_config.data.row_limit {
        graph_config = graph_config.with_row_limit(limit);
    }

    let graph_builder = DatabaseGraphBuilder::new(
        provider,
        compute,
        query_executor,
        graph_config,
    );

    let graph = graph_builder
        .build_graph(&metadata, &connection_params)
        .await
        .context("failed to build graph from database")?;

    println!(
        "Graph built: {} nodes, {} edges",
        graph.node_count(),
        graph.edge_count()
    );

    if graph.node_count() == 0 {
        anyhow::bail!("Cannot train on empty graph. Check your table filter and row limit.");
    }

    // 8. Split data into train/val/test
    println!("Splitting data (train/val/test: {:?})...", train_config.data.split_ratios);

    let (train_graph, val_graph, _test_graph) = split_graph(
        &graph,
        train_config.data.split_ratios,
        train_config.data.split_seed,
    )
    .context("failed to split graph")?;

    println!(
        "Split complete: {} train, {} val, {} test nodes",
        train_graph.node_count(),
        val_graph.node_count(),
        _test_graph.node_count()
    );

    // 9. Train model
    println!("Starting training...");
    let mut trainer = Trainer::new(train_config.clone());

    let report = trainer
        .train(&train_graph, Some(&val_graph))
        .context("training failed")?;

    // 10. Save model
    println!("Saving model to {:?}...", output);

    let output_str = output
        .to_str()
        .context("output path contains invalid UTF-8")?;

    trainer
        .engine()
        .save(output_str)
        .context("failed to save model")?;

    // 11. Export metrics if computed
    if train_config.training.compute_metrics {
        let metrics_path = output.with_extension("metrics.csv");
        trainer
            .metrics()
            .export_csv(&metrics_path)
            .context("failed to export metrics")?;

        println!("Metrics exported to {:?}", metrics_path);
    }

    // 12. Print summary
    println!("\n{}", "=".repeat(60));
    println!("Training complete!");
    println!("{}", "=".repeat(60));
    println!("  Model dimension:  {}", report.model_info.dimension);
    println!("  Vocabulary size:  {}", report.model_info.vocab_size);
    println!("  Corpus size:      {}", report.model_info.corpus_size);
    println!("  Training time:    {:.2}s", report.total_time.as_secs_f32());
    println!("{}", "=".repeat(60));
    println!("\nModel files created:");
    println!("  - {}.w2v   (Word2Vec embeddings)", output_str);
    println!("  - {}.hnsw  (HNSW search index)", output_str);
    println!("  - {}.gnodes (Node metadata)", output_str);

    if train_config.training.compute_metrics {
        println!("  - {}.metrics.csv (Training metrics)", output_str);
    }

    println!("\nYou can now use this model for semantic search!");

    Ok(())
}

/// Resume training from a checkpoint.
async fn run_resume(
    _path: Option<PathBuf>,
    _checkpoint: PathBuf,
    _output: PathBuf,
) -> Result<()> {
    println!("Resuming training from checkpoint is not yet fully implemented.");
    println!("This feature will be available in a future release.");
    println!("\nTo train a new model, use: gfs train start --output <path>");
    Ok(())
}
