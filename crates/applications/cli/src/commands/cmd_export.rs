//! `gfs export` — export data from the running database instance.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::export_repo_usecase::ExportRepoUseCase;
use serde_json::json;

use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::repository::Repository;

use crate::cli_utils::get_repo_dir;
use crate::commands::compute_support::compute_for_repo;
use crate::output::{cyan, green};

pub async fn run(
    path: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    format: String,
    id: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    // Route through the repository's configured runtime. A provider with no
    // container — SQLite — resolves to a no-op runtime rather than failing to
    // reach a Docker daemon it never needed.
    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, &repo_path).await?;

    let _ = id; // container name override is reserved for future use; use case reads from config.

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    // A lazy clone only holds rows for tables that have actually been read, and
    // pg_dump reads those local tables directly (it never goes through the
    // copy-on-read planner hook). Exporting one without materializing first
    // produces a valid-looking dump with entire tables empty, silently (#116).
    // No-op for a normal repository.
    super::cmd_source::materialize_clone(&repo_path, json_output).await?;

    let use_case = ExportRepoUseCase::new(compute, registry);
    // Do not use `.context("export failed")`: anyhow's context Display only prints that string
    // and hides the underlying `ExportRepoError` / `ComputeError` message on stderr.
    let output = use_case.run(&repo_path, output_dir, &format).await?;

    if json_output {
        println!(
            "{}",
            json!({
                "file_path": output.file_path.display().to_string(),
                "format": format,
            })
        );
    } else {
        println!(
            "{} Exported to {}",
            green("✓"),
            cyan(output.file_path.display().to_string())
        );
    }
    if !output.stderr.is_empty() {
        eprintln!("{}", output.stderr.trim_end());
    }

    Ok(())
}
