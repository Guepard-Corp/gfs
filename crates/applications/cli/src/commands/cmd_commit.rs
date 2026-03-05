use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::ports::storage::StoragePort;
use gfs_domain::usecases::repository::commit_repo_usecase::CommitRepoUseCase;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed};

// ---------------------------------------------------------------------------
// Entry point called from main
// ---------------------------------------------------------------------------

pub async fn commit(
    path: Option<PathBuf>,
    message: String,
    author: Option<String>,
    author_email: Option<String>,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use gfs_storage_apfs::ApfsStorage;
        let storage: Arc<dyn StoragePort> = Arc::new(ApfsStorage::new());
        run(path, message, author, author_email, storage).await
    }

    #[cfg(not(target_os = "macos"))]
    {
        use gfs_storage_file::FileStorage;
        let storage: Arc<dyn StoragePort> = Arc::new(FileStorage::new());
        run(path, message, author, author_email, storage).await
    }
}

// ---------------------------------------------------------------------------
// Core logic (platform-agnostic once storage is injected)
// ---------------------------------------------------------------------------

async fn run(
    path: Option<PathBuf>,
    message: String,
    author: Option<String>,
    author_email: Option<String>,
    storage: Arc<dyn StoragePort>,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Arc<dyn Compute> = Arc::new(
        DockerCompute::new()
            .map_err(|e| anyhow::anyhow!("failed to connect to Docker/Podman: {e}"))?,
    );

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| anyhow::anyhow!("failed to register database providers: {e}"))?;

    let use_case = CommitRepoUseCase::new(repository.clone(), compute, storage, registry);

    // Resolve branch before moving repo_path into the use case.
    let branch = repository
        .get_current_branch(&repo_path)
        .await
        .unwrap_or_else(|_| "HEAD".to_string());

    let commit_hash = use_case
        .run(repo_path, message.clone(), author, author_email, None, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    print_commit_result(&branch, &commit_hash, &message);
    Ok(())
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

fn print_commit_result(branch: &str, hash: &str, message: &str) {
    let short = &hash[..7.min(hash.len())];
    println!("[{}] {}  {}", cyan(branch), dimmed(short), message);
}
