//! `gfs schema` — database schema operations (extract, show, diff).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use gfs_compute_docker::DockerCompute;
use gfs_domain::model::datasource::diff::compute_schema_diff;
use gfs_domain::model::datasource::diff_formatter::{
    AgenticFormatter, JsonFormatter, PrettyFormatter,
};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::repo_utils::repo_layout;
use gfs_domain::usecases::repository::extract_schema_usecase::ExtractSchemaUseCase;

use crate::cli_utils::get_repo_dir;
use crate::output::{bold, cyan, dimmed, green};

/// Extract schema from the running database instance.
pub async fn run_extract(
    path: Option<PathBuf>,
    output: Option<PathBuf>,
    compact: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let compute = Arc::new(DockerCompute::new().context(
        "failed to connect to Docker/Podman daemon (is your container runtime running?)",
    )?);

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let use_case = ExtractSchemaUseCase::new(compute, registry);
    let result = use_case
        .run(&repo_path)
        .await
        .context("schema extraction failed")?;

    // Serialize to JSON
    let json = if compact {
        serde_json::to_string(&result.metadata)
            .context("failed to serialize schema metadata to JSON")?
    } else {
        serde_json::to_string_pretty(&result.metadata)
            .context("failed to serialize schema metadata to JSON")?
    };

    // Output (do not color JSON to stdout - machine-readable)
    if let Some(output_path) = output {
        std::fs::write(&output_path, &json)
            .with_context(|| format!("failed to write schema to {}", output_path.display()))?;
        println!(
            "{} {}",
            green("Schema extracted to"),
            cyan(output_path.display().to_string())
        );
    } else {
        println!("{}", json);
    }

    Ok(())
}

/// Show schema from a specific commit.
pub async fn run_show(
    commit: String,
    path: Option<PathBuf>,
    metadata_only: bool,
    ddl_only: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    // Resolve commit hash
    let commit_hash = repo_layout::rev_parse(&repo_path, &commit)
        .with_context(|| format!("failed to resolve commit '{}'", commit))?;

    // Load commit
    let commit_obj = repo_layout::get_commit_from_hash(&repo_path, &commit_hash)
        .with_context(|| format!("failed to load commit {}", commit_hash))?;

    // Get schema hash
    let schema_hash = commit_obj.schema_hash.ok_or_else(|| {
        anyhow!(
            "commit {} has no schema (schema versioning was not enabled)",
            commit_hash
        )
    })?;

    // Load schema object
    let (metadata, ddl) = repo_layout::get_schema_by_hash(&repo_path, &schema_hash)
        .with_context(|| format!("failed to load schema object {}", schema_hash))?;

    // Output based on flags (do not color ddl_only or metadata_only - raw output)
    if ddl_only {
        println!("{}", ddl);
    } else if metadata_only {
        let json = serde_json::to_string_pretty(&metadata)
            .context("failed to serialize schema metadata")?;
        println!("{}", json);
    } else {
        // Show both metadata and DDL with colors
        println!("{} {}", dimmed("Schema Hash:"), cyan(schema_hash));
        println!(
            "{} {} {}",
            dimmed("Driver:"),
            metadata.driver,
            metadata.version
        );
        println!("\n{}", bold("=== Metadata (JSON) ==="));
        let json = serde_json::to_string_pretty(&metadata)
            .context("failed to serialize schema metadata")?;
        println!("{}", json);
        println!("\n{}", bold("=== DDL (SQL) ==="));
        println!("{}", ddl);
    }

    Ok(())
}

/// Compare schemas between two commits.
pub async fn run_diff(
    commit1: String,
    commit2: String,
    path: Option<PathBuf>,
    pretty: bool,
    json: bool,
    no_color: bool,
) -> Result<()> {
    // Check for mutual exclusivity
    if pretty && json {
        return Err(anyhow!("--pretty and --json cannot be used together"));
    }

    let repo_path = path.unwrap_or_else(get_repo_dir);

    // Resolve commit hashes
    let hash1 = repo_layout::rev_parse(&repo_path, &commit1)
        .with_context(|| format!("failed to resolve commit '{}'", commit1))?;
    let hash2 = repo_layout::rev_parse(&repo_path, &commit2)
        .with_context(|| format!("failed to resolve commit '{}'", commit2))?;

    // Load commits
    let commit1_obj = repo_layout::get_commit_from_hash(&repo_path, &hash1)
        .with_context(|| format!("failed to load commit {}", hash1))?;
    let commit2_obj = repo_layout::get_commit_from_hash(&repo_path, &hash2)
        .with_context(|| format!("failed to load commit {}", hash2))?;

    // Get schema hashes
    let schema_hash1 = commit1_obj
        .schema_hash
        .ok_or_else(|| anyhow!("commit {} has no schema", hash1))?;
    let schema_hash2 = commit2_obj
        .schema_hash
        .ok_or_else(|| anyhow!("commit {} has no schema", hash2))?;

    // Load schema objects
    let (metadata1, _ddl1) = repo_layout::get_schema_by_hash(&repo_path, &schema_hash1)
        .with_context(|| format!("failed to load schema object {}", schema_hash1))?;
    let (metadata2, _ddl2) = repo_layout::get_schema_by_hash(&repo_path, &schema_hash2)
        .with_context(|| format!("failed to load schema object {}", schema_hash2))?;

    // Generate diff
    let diff = compute_schema_diff(&metadata1, &metadata2, &hash1, &hash2);

    // Format output
    let output = if json {
        JsonFormatter::format(&diff).context("failed to serialize JSON output")?
    } else if pretty {
        let formatter = PrettyFormatter::new(!no_color);
        formatter.format(&diff)
    } else {
        AgenticFormatter::format(&diff)
    };

    println!("{}", output);

    // Exit with appropriate code
    std::process::exit(diff.exit_code());
}
