//! `gfs import` — import data into the running database instance.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::import_repo_usecase::ImportRepoUseCase;
use serde_json::json;

use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::repository::Repository;

use crate::cli_utils::get_repo_dir;
use crate::commands::compute_support::compute_for_repo;
use crate::output::{cyan, green};

pub async fn run(
    path: Option<PathBuf>,
    file: PathBuf,
    format: Option<String>,
    id: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    // Route through the repository's configured runtime. A provider with no
    // container — SQLite — resolves to a no-op runtime rather than failing to
    // reach a Docker daemon it never needed.
    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, &repo_path).await?;

    let _ = id; // container name override reserved for future use.

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let format_str = format.as_deref().unwrap_or("");

    let use_case = ImportRepoUseCase::new(compute, registry);
    let output = use_case
        .run(&repo_path, file, format_str)
        .await
        .context("import failed")?;

    if json_output {
        println!(
            "{}",
            json!({
                "imported_from": output.imported_from.display().to_string(),
            })
        );
    } else {
        println!(
            "{} Imported from {}",
            green("✓"),
            cyan(output.imported_from.display().to_string())
        );
    }
    if !output.stderr.is_empty() {
        eprintln!("{}", output.stderr.trim_end());
    }

    Ok(())
}
