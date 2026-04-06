use std::path::PathBuf;
use std::sync::Arc;

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::init_repo_usecase::InitRepositoryUseCase;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

pub async fn init(
    path: Option<PathBuf>,
    database_provider: Option<String>,
    database_version: Option<String>,
    database_port: Option<u16>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::trace!("Initializing Guepard environment at: {:?}", path);

    let target_path = path.unwrap_or_else(get_repo_dir);
    let provider_display = database_provider.clone();

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Arc<dyn Compute> = Arc::new(DockerCompute::new()?);

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())?;

    let use_case = InitRepositoryUseCase::new(repository, compute, registry);
    use_case
        .run(
            target_path.clone(),
            None,
            database_provider,
            database_version,
            database_port,
        )
        .await?;

    // Success feedback
    println!(
        "  {} Initialized GFS repository at {}",
        green("✓"),
        cyan(target_path.display().to_string())
    );
    println!();
    println!(
        "    {:<16} {}",
        dimmed("Branch"),
        cyan("main")
    );
    println!(
        "    {:<16} {}",
        dimmed("Config"),
        ".gfs/config.toml"
    );
    if let Some(ref provider) = provider_display {
        println!(
            "    {:<16} {}",
            dimmed("Provider"),
            cyan(provider)
        );
    }

    Ok(())
}
