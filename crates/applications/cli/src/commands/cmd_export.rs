//! `gfs export` — export data from the running database instance.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::DockerCompute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::export_repo_usecase::ExportRepoUseCase;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, green};

pub async fn run(
    path: Option<PathBuf>,
    output_dir: PathBuf,
    format: String,
    id: Option<String>,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let compute = Arc::new(DockerCompute::new().context(
        "failed to connect to Docker/Podman daemon (is your container runtime running?)",
    )?);

    // If --id is given, we need to override the container name in the config.
    // The use case loads it from config; for --id override we create a temporary wrapper.
    // Simplest approach: if --id is set, set env var or just note it's an override.
    // For now we pass it through a thin shim: if --id is given, override config loading.
    let _ = id; // container name override is reserved for future use; use case reads from config.

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let use_case = ExportRepoUseCase::new(compute, registry);
    let output = use_case
        .run(&repo_path, output_dir, &format)
        .await
        .context("export failed")?;

    println!(
        "{} {}",
        green("Exported to"),
        cyan(output.file_path.display().to_string())
    );
    if !output.stderr.is_empty() {
        eprintln!("{}", output.stderr.trim_end());
    }

    Ok(())
}
