use std::path::PathBuf;
use std::sync::Arc;

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::init_repo_usecase::{
    DatabaseCredentials, InitRepositoryUseCase,
};
use gfs_domain::usecases::repository::status_repo_usecase::StatusRepoUseCase;
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub async fn init(
    path: Option<PathBuf>,
    database_provider: Option<String>,
    database_version: Option<String>,
    database_port: Option<u16>,
    credentials: DatabaseCredentials,
    json_output: bool,
    image: Option<String>,
    platform: Option<String>,
    labels: std::collections::BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::trace!("Initializing Guepard environment at: {:?}", path);

    let target_path = path.unwrap_or_else(get_repo_dir);
    let has_provider = database_provider.is_some();
    let provider_display = database_provider.clone();

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Option<Arc<dyn Compute>> = if database_provider.is_some() {
        Some(Arc::new(
            DockerCompute::new()
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .with_platform(platform),
        ))
    } else {
        None
    };

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())?;

    let use_case =
        InitRepositoryUseCase::new(repository.clone(), compute.clone(), registry.clone());
    use_case
        .run(
            target_path.clone(),
            None,
            database_provider,
            database_version,
            database_port,
            credentials,
            image,
            labels,
        )
        .await?;

    let mut connection_string: Option<String> = None;
    if has_provider && let Some(compute) = compute.clone() {
        let status_uc = StatusRepoUseCase::new(repository, compute, registry);
        if let Ok(status) = status_uc.run(&target_path).await {
            connection_string = status
                .compute
                .and_then(|c| (!c.connection_string.is_empty()).then_some(c.connection_string));
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": target_path.display().to_string(),
                "branch": "main",
                "config": ".gfs/config.toml",
                "provider": provider_display,
                "connection_string": connection_string,
            }))?
        );
    } else {
        println!(
            "  {} Initialized GFS repository at {}",
            green("✓"),
            cyan(target_path.display().to_string())
        );
        println!();
        println!("    {:<16} {}", dimmed("Branch"), cyan("main"));
        println!("    {:<16} .gfs/config.toml", dimmed("Config"));
        if let Some(ref provider) = provider_display {
            println!("    {:<16} {}", dimmed("Provider"), cyan(provider));
        }
        if let Some(ref c) = connection_string {
            println!("    {:<16} {}", dimmed("Connection"), cyan(c));
        }
    }

    Ok(())
}
