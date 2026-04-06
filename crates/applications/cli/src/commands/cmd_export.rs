//! `gfs export` — export data from the running database instance.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::DockerCompute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::export_repo_usecase::ExportRepoUseCase;
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, green};

pub async fn run(
    path: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    format: String,
    id: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let compute = Arc::new(DockerCompute::new().context(
        "failed to connect to Docker/Podman daemon (is your container runtime running?)",
    )?);

    let _ = id; // container name override is reserved for future use; use case reads from config.

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let use_case = ExportRepoUseCase::new(compute, registry);
    let output = use_case
        .run(&repo_path, output_dir, &format)
        .await
        .context("export failed")?;

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
