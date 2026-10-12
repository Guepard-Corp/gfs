//! `gfs clone <source> <target>`: a new, independent database holding the data
//! of one snapshot of another database.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use gfs_compute_docker::DockerCompute;
use gfs_db_providers as containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::{
    DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};
use gfs_domain::ports::repository::Repository;
use gfs_domain::repo_utils::repo_layout;
use gfs_domain::usecases::repository::clone_from_snapshot_usecase::{
    CloneFromSnapshotRequest, CloneFromSnapshotUseCase,
};
use gfs_domain::usecases::repository::init_repo_usecase::{InitRepositoryUseCase, ProvisionSpec};
use gfs_domain::usecases::repository::status_repo_usecase::StatusRepoUseCase;
use serde_json::json;

use super::cmd_commit;
use crate::output::{cyan, dimmed, green, yellow};

pub struct CloneFromSnapshotArgs {
    pub source: PathBuf,
    pub target: PathBuf,
    /// A snapshot id or a branch name; `None` = the source's latest snapshot.
    pub revision: Option<String>,
    pub port: Option<u16>,
    pub image: Option<String>,
    pub platform: Option<String>,
}

pub async fn clone_from_snapshot(mut args: CloneFromSnapshotArgs, json_output: bool) -> Result<()> {
    if is_kubernetes_runtime() {
        return Err(anyhow!(
            "cloning from a snapshot is not available on the Kubernetes runtime yet"
        ));
    }
    // Absolute paths, so what the new repository records about its workspace
    // and connection holds wherever it is later used from, not only from the
    // directory this command ran in.
    args.source = std::path::absolute(&args.source)?;
    args.target = std::path::absolute(&args.target)?;

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| anyhow!("failed to register database providers: {e}"))?;

    // Reach for a container runtime only when the source is a repository whose
    // database runs in one. Anything else -- not a repository, no database, an
    // embedded engine like sqlite -- is refused or handled by the use case, and
    // must not surface as "cannot connect to Docker".
    let needs_container = repo_layout::get_environment_config(&args.source)
        .ok()
        .flatten()
        .and_then(|env| registry.get(&env.database_provider))
        .is_some_and(|provider| provider.requires_compute());
    let compute: Option<Arc<dyn Compute>> = if needs_container {
        Some(Arc::new(
            DockerCompute::new()
                .map_err(|e| anyhow!("{e}"))?
                .with_platform(args.platform.clone()),
        ))
    } else {
        None
    };

    let init = InitRepositoryUseCase::new(repository.clone(), compute.clone(), registry.clone());
    let cloned = CloneFromSnapshotUseCase::new(repository.clone(), init)
        .run(CloneFromSnapshotRequest {
            source: args.source.clone(),
            target: args.target.clone(),
            revision: args.revision.clone(),
            database_port: args.port,
            image: args.image.clone(),
            credentials: None,
            provision: ProvisionSpec::default(),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    let source_short = short(&cloned.source_commit);
    let message = format!("cloned from {} @ {source_short}", args.source.display());
    let (commit, _) = cmd_commit::record(args.target.clone(), message, None, None)
        .await
        .map_err(|e| {
            anyhow!(
                "the copy was created at '{}' but its first commit failed: {e}",
                args.target.display()
            )
        })?;

    let connection_string = connection_string(&args.target, compute, repository, &registry).await;
    let source_is_lazy = is_lazy_clone(&args.source);

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": args.target.display().to_string(),
                "source": args.source.display().to_string(),
                "source_snapshot": cloned.source_commit,
                "commit": commit,
                "provider": cloned.database_provider,
                "version": cloned.database_version,
                "connection_string": connection_string,
                "lazy": source_is_lazy,
            }))?
        );
    } else {
        println!(
            "  {} cloned {} @ {} into {} ({} {})",
            green("✓"),
            cyan(args.source.display().to_string()),
            source_short,
            cyan(args.target.display().to_string()),
            cloned.database_provider,
            cloned.database_version
        );
        if let Some(ref c) = connection_string {
            println!("    {:<16} {}", dimmed("Connection"), cyan(c));
        }
    }
    if source_is_lazy {
        eprintln!(
            "{} {} is a lazy clone, so this copy is one too: it reads rows it has not \
             copied yet from the same external database, and that database's password is \
             stored inside it. `gfs lazy freeze` makes it independent.",
            yellow("note:"),
            args.source.display()
        );
    }
    Ok(())
}

fn is_kubernetes_runtime() -> bool {
    std::env::var("GFS_RUNTIME_PROVIDER")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "kubernetes" | "k8s" | "k3s"
            )
        })
        .unwrap_or(false)
}

/// A lazy clone is labelled `gfs.role=clone` when it is created.
fn is_lazy_clone(repo: &Path) -> bool {
    GfsConfig::load(repo)
        .ok()
        .and_then(|config| config.compute)
        .and_then(|compute| compute.labels.get("gfs.role").cloned())
        .is_some_and(|role| role == "clone")
}

async fn connection_string(
    target: &Path,
    compute: Option<Arc<dyn Compute>>,
    repository: Arc<dyn Repository>,
    registry: &Arc<InMemoryDatabaseProviderRegistry>,
) -> Option<String> {
    match compute {
        Some(compute) => StatusRepoUseCase::new(repository, compute, registry.clone())
            .run(target)
            .await
            .ok()
            .and_then(|status| status.compute)
            .map(|c| c.connection_string)
            .filter(|c| !c.is_empty()),
        None => repo_layout::get_environment_config(target)
            .ok()
            .flatten()
            .and_then(|env| registry.get(&env.database_provider))
            .zip(repo_layout::local_connection_params(target).ok())
            .and_then(|(provider, params)| provider.connection_string(&params).ok()),
    }
}

fn short(hash: &str) -> &str {
    &hash[..7.min(hash.len())]
}
