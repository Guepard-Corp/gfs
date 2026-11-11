use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gfs_db_providers as containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
#[cfg(target_os = "linux")]
use gfs_domain::model::layout::GFS_DIR;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::ports::storage::StoragePort;
use gfs_domain::usecases::repository::commit_repo_usecase::CommitRepoUseCase;
use serde_json::json;

use super::compute_support::compute_for_repo;
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

// ---------------------------------------------------------------------------
// Entry point called from main
// ---------------------------------------------------------------------------

pub async fn commit(
    path: Option<PathBuf>,
    message: String,
    author: Option<String>,
    author_email: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let (commit_hash, branch) = record(repo_path, message.clone(), author, author_email).await?;
    if json_output {
        println!(
            "{}",
            json!({
                "hash": commit_hash,
                "branch": branch,
                "message": message,
            })
        );
    } else {
        let short = &commit_hash[..7.min(commit_hash.len())];
        println!(
            "{} [{}] {}  {}",
            green("✓"),
            cyan(&branch),
            dimmed(short),
            message
        );
    }
    Ok(())
}

/// Commit the repository at `repo_path` without printing anything; returns the
/// new commit's hash and the branch it landed on. For commands that make a
/// commit as one step of something larger and report it in their own words.
pub async fn record(
    repo_path: PathBuf,
    message: String,
    author: Option<String>,
    author_email: Option<String>,
) -> Result<(String, String)> {
    let storage = storage_for(&repo_path).await;
    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Arc<dyn Compute> = compute_for_repo(&repository, &repo_path).await?;
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
        .run(repo_path, message, author, author_email, None, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((commit_hash, branch))
}

#[cfg(target_os = "macos")]
async fn storage_for(_repo_path: &std::path::Path) -> Arc<dyn StoragePort> {
    Arc::new(gfs_storage_apfs::ApfsStorage::new())
}

#[cfg(target_os = "linux")]
async fn storage_for(repo_path: &std::path::Path) -> Arc<dyn StoragePort> {
    storage_for_repo(repo_path).await
}

#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
async fn storage_for(_repo_path: &std::path::Path) -> Arc<dyn StoragePort> {
    Arc::new(gfs_storage_file::FileStorage::new())
}

#[cfg(target_os = "linux")]
async fn storage_for_repo(repo_path: &std::path::Path) -> Arc<dyn StoragePort> {
    if let Ok(cfg) = gfs_domain::model::config::GfsConfig::load(repo_path)
        && cfg
            .runtime
            .as_ref()
            .map(|r| gfs_domain::model::config::is_kubernetes_provider(&r.runtime_provider))
            .unwrap_or(false)
    {
        let s = gfs_storage_kubernetes::KubernetesStorage::new(None)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .expect("kubernetes storage init");
        return Arc::new(s);
    }
    if gfs_storage_btrfs::is_btrfs(&repo_path.join(GFS_DIR)) {
        Arc::new(gfs_storage_btrfs::BtrfsStorage::from_repo(repo_path))
    } else {
        Arc::new(gfs_storage_file::FileStorage::new())
    }
}
