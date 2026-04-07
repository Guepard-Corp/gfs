//! `gfs checkout <revision>` — switch branch or checkout commit (detached HEAD).
//! `gfs checkout -b <branch_name> [<start_revision>]` — create a new branch and switch to it.
//!
//! When the repo has a compute container, the use case stops it before checkout
//! and starts (or recreates with the new workspace mount) after checkout.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::checkout_repo_usecase::CheckoutRepoUseCase;
use serde_json::json;

use super::compute_support::compute_for_repo;
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn checkout(
    path: Option<PathBuf>,
    revision: Option<String>,
    create_branch: Option<String>,
    json_output: bool,
) -> Result<()> {
    let (revision, create_branch) = match (&revision, &create_branch) {
        (Some(r), None) => (r.clone(), None),
        (None, Some(b)) => (String::new(), Some(b.clone())),
        (Some(r), Some(b)) => (r.clone(), Some(b.clone())),
        (None, None) => {
            anyhow::bail!("revision required or use -b <branch_name>");
        }
    };

    let repo_path = path.unwrap_or_else(get_repo_dir);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Arc<dyn Compute> = compute_for_repo(&repository, &repo_path).await?;
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let use_case = CheckoutRepoUseCase::new(repository, compute, registry);
    let commit_hash = use_case
        .run(repo_path, revision.clone(), create_branch.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if json_output {
        println!(
            "{}",
            json!({
                "hash": commit_hash,
                "branch": create_branch.as_deref().unwrap_or(&revision),
                "new_branch": create_branch.is_some(),
            })
        );
    } else {
        let short_hash = &commit_hash[..7.min(commit_hash.len())];
        if let Some(ref name) = create_branch {
            println!(
                "{} Switched to new branch '{}' ({})",
                green("✓"),
                green(name.trim()),
                dimmed(short_hash)
            );
        } else {
            println!(
                "{} Switched to {} ({})",
                green("✓"),
                cyan(revision.trim()),
                dimmed(short_hash)
            );
        }
    }
    Ok(())
}
