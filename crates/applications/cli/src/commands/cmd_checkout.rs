//! `gfs checkout <revision>` — switch branch or checkout commit (detached HEAD).
//! `gfs checkout -b <branch_name> [<start_revision>]` — create a new branch and switch to it.
//!
//! When the repo has a compute container, the use case stops it before checkout
//! and starts (or recreates with the new workspace mount) after checkout.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::DatabaseProviderRegistry;
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
        // 1) stop (best-effort)
        if let Ok(cfg) = GfsConfig::load(&repo_path)
            && let Some(rt) = cfg.runtime
        {
            let _ = compute
                .stop(&gfs_domain::ports::compute::InstanceId(rt.container_name))
                .await;
        }

        // 2) repo checkout (create branch ref first when -b, then switch HEAD)
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
            revision.clone()
        };

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
        let vs_name = format!("gfs-snap-{}", &snapshot_hash[..32.min(snapshot_hash.len())]);
        let new_pvc = format!(
            "gfs-ws-{}-{}",
            &snapshot_hash[..8.min(snapshot_hash.len())],
            chrono::Utc::now().timestamp_millis()
        );

        let storage = gfs_storage_kubernetes::KubernetesStorage::new(None).await?;
        gfs_domain::ports::storage::StoragePort::clone(
            &storage,
                &gfs_domain::ports::storage::VolumeId("unused".into()),
                gfs_domain::ports::storage::VolumeId(new_pvc.clone()),
                gfs_domain::ports::storage::CloneOptions {
                    from_snapshot: Some(gfs_domain::ports::storage::SnapshotId(vs_name)),
                },
        )
        .await?;

        // 4) recreate compute instance bound to the restored PVC
        if let Ok(mut cfg) = GfsConfig::load(&repo_path) {
            if let Some(rt) = cfg.runtime.take() {
                let _ = compute
                    .remove_instance(&gfs_domain::ports::compute::InstanceId(rt.container_name))
                    .await;
            }
            cfg.mount_point = Some(new_pvc.clone());
            let _ = cfg.save(&repo_path);
        }

        // Provision new instance with PVC override via host_data_dir = pvc:<name>
        let provider = registry
            .get(
                gfs_domain::model::config::GfsConfig::load(&repo_path)
                    .ok()
                    .and_then(|c| c.environment.map(|e| e.database_provider))
                    .unwrap_or_else(|| "postgres".into())
                    .as_str(),
            )
            .context("unknown database provider")?;
        let mut def = provider.definition();
        if let Ok(cfg) = GfsConfig::load(&repo_path)
            && let Some(env) = cfg.environment.as_ref()
        {
            let base = def
                .image
                .split(':')
                .next()
                .unwrap_or(&def.image);
            def.image = format!("{}:{}", base, env.database_version);
        }
        def.host_data_dir = Some(std::path::PathBuf::from(format!("pvc:{new_pvc}")));
        let new_id = compute.provision(&def).await?;
        let _ = compute.start(&new_id, Default::default()).await?;
        let runtime = compute.describe_runtime().await.unwrap_or(gfs_domain::ports::compute::RuntimeDescriptor {
            provider: "kubernetes".into(),
            version: "unknown".into(),
        });
        repository
            .update_runtime_config(
                &repo_path,
                gfs_domain::model::config::RuntimeConfig {
                    runtime_provider: runtime.provider,
                    runtime_version: runtime.version,
                    container_name: new_id.0.clone(),
                },
            )
            .await?;

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
