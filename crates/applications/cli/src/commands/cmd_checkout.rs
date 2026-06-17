//! `gfs checkout <revision>` — switch branch or checkout commit (detached HEAD).
//! `gfs checkout -b <branch_name> [<start_revision>]` — create a new branch and switch to it.
//!
//! When the repo has a compute container, the use case stops it before checkout
//! and starts (or recreates with the new workspace mount) after checkout.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_kubernetes::KubernetesCompute;
use gfs_compute_kubernetes::checkout::restore_database_volume_from_snapshot;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
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
    let repo_path = path.clone().unwrap_or_else(get_repo_dir);

    if super::remote_support::is_remote_repo(&repo_path)? {
        return super::cmd_remote_checkout::checkout(path, revision, create_branch, json_output)
            .await;
    }

    let (revision, create_branch) = match (&revision, &create_branch) {
        (Some(r), None) => (r.clone(), None),
        (None, Some(b)) => (String::new(), Some(b.clone())),
        (Some(r), Some(b)) => (r.clone(), Some(b.clone())),
        (None, None) => {
            anyhow::bail!("revision required or use -b <branch_name>");
        }
    };

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Arc<dyn Compute> = compute_for_repo(&repository, &repo_path).await?;
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    // Kubernetes runtime needs PVC restore; the generic checkout use case assumes filesystem snapshots.
    let is_k8s = GfsConfig::load(&repo_path)
        .ok()
        .and_then(|c| c.runtime.map(|r| r.runtime_provider))
        .map(|p| p.trim().eq_ignore_ascii_case("kubernetes"))
        .unwrap_or(false);

    let commit_hash = if is_k8s {
        // Validate refs before stopping compute — bad input must not leave the DB offline.
        let checkout_rev = if let Some(ref branch_name) = create_branch {
            let branch_name = branch_name.trim();
            if branch_name.is_empty() {
                anyhow::bail!("empty branch name");
            }
            let start_rev = if revision.trim().is_empty() {
                "HEAD"
            } else {
                revision.trim()
            };
            let tip = repository
                .rev_parse(&repo_path, start_rev)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if tip == "0" {
                anyhow::bail!("cannot create branch: start revision has no commits");
            }
            repository
                .create_branch(&repo_path, branch_name, &tip)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            branch_name.to_string()
        } else {
            if revision.trim().is_empty() {
                anyhow::bail!("revision required or use -b <branch_name>");
            }
            let target = revision.trim();
            repository
                .rev_parse(&repo_path, target)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            target.to_string()
        };

        if let Ok(cfg) = GfsConfig::load(&repo_path)
            && let Some(rt) = cfg.runtime
        {
            let _ = compute
                .stop(&gfs_domain::ports::compute::InstanceId(rt.container_name))
                .await;
        }

        repository
            .checkout(&repo_path, &checkout_rev)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let commit_hash = repository.get_current_commit_id(&repo_path).await?;

        // 3) restore PVC from VolumeSnapshot mapped to commit.snapshot_hash
        let commit =
            gfs_domain::repo_utils::repo_layout::get_commit_from_hash(&repo_path, &commit_hash)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        let snapshot_hash = commit.snapshot_hash;

        let storage = gfs_storage_kubernetes::KubernetesStorage::new(None).await?;
        let k8s_compute = KubernetesCompute::new(None)
            .await
            .map_err(|e| anyhow::anyhow!("kubernetes compute: {e}"))?;

        restore_database_volume_from_snapshot(
            &storage,
            &k8s_compute,
            registry.clone(),
            repository.clone(),
            &repo_path,
            &snapshot_hash,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        commit_hash
    } else {
        let use_case = CheckoutRepoUseCase::new(repository, compute, registry);
        use_case
            .run(repo_path, revision.clone(), create_branch.clone())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
    };

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
